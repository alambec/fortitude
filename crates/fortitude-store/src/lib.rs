//! Persistence: Part-10 files (in a local directory or an S3 bucket), ingest (header parsing,
//! pixel data offsets, pre-encoded metadata) and the index (SQLite or PostgreSQL) with its
//! group-committing writer.

pub mod convert;
pub mod files;
pub mod imaging;
pub mod index;
pub mod ingest;
pub mod integrity;
mod locations;
pub mod modalities;
pub mod pixel;
pub mod retention;
pub mod retrieve;
pub mod search;
pub mod walker;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;

use crate::files::{FileBackend, FileStore};
use crate::index::{Database, Index, IndexError, InsertOutcome};
use crate::ingest::{Identifiers, IngestError, IngestOptions};
use crate::locations::LocationCache;

/// Database of the index.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum IndexBackend {
    /// `index.sqlite` in the storage directory.
    #[default]
    Sqlite,
    /// A PostgreSQL database, by connection string: a URL
    /// (`postgres://user:password@host/database`) or key-value pairs (`host=… dbname=…`).
    Postgres(String),
}

impl std::fmt::Debug for IndexBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            IndexBackend::Sqlite => f.write_str("Sqlite"),
            // The connection string may hold a password.
            IndexBackend::Postgres(_) => f.write_str("Postgres(..)"),
        }
    }
}

#[derive(Debug, Clone)]
pub struct StoreOptions {
    /// Directory holding `files/` with the filesystem backend, and `index.sqlite` with the
    /// SQLite backend.
    pub path: PathBuf,
    pub files: FileBackend,
    /// Flush every file to disk before acknowledging it (filesystem backend).
    pub fsync: bool,
    pub index: IndexBackend,
    /// Number of read connections to the index.
    pub index_readers: usize,
    /// Memory for the pixel data locations of recently read series, in bytes; 0 disables
    /// the cache.
    pub location_cache_bytes: usize,
    /// How long the locations of a series are kept after they were loaded.
    pub location_cache_ttl: Duration,
    /// Binary values up to this size are inlined in metadata.
    pub inline_binary_max: usize,
}

#[derive(Debug, thiserror::Error)]
pub enum OpenError {
    #[error("cannot create storage directory: {0}")]
    Io(#[from] std::io::Error),
    #[error("invalid S3 configuration: {0}")]
    S3(String),
    #[error(transparent)]
    Index(#[from] IndexError),
}

/// Why an instance was not stored.
#[derive(Debug, thiserror::Error)]
pub enum StoreFailure {
    #[error(transparent)]
    Ingest(#[from] IngestError),
    #[error("instance belongs to study {actual}, not {expected}")]
    StudyMismatch { expected: String, actual: String },
    #[error("series {0} is already stored under another study")]
    SeriesConflict(String),
    #[error("cannot write file: {0}")]
    Io(#[from] std::io::Error),
    #[error(transparent)]
    Index(#[from] IndexError),
}

impl StoreFailure {
    /// Failure Reason (0008,1197) of a STOW-RS response (PS3.18 Table I.2-2).
    pub fn failure_reason(&self) -> u16 {
        match self {
            StoreFailure::Ingest(IngestError::Unreadable(_)) => 0xC000,
            StoreFailure::Ingest(IngestError::Invalid(_))
            | StoreFailure::StudyMismatch { .. }
            | StoreFailure::SeriesConflict(_) => 0xA900,
            StoreFailure::Io(_) => 0xA700,
            StoreFailure::Index(_) => 0x0110,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Stored {
    New,
    /// Already stored; the new copy was discarded.
    Duplicate,
}

/// Result of storing one instance.
#[derive(Debug)]
pub struct StoreResult {
    pub sop_class_uid: Option<String>,
    pub sop_uid: Option<String>,
    pub study_uid: Option<String>,
    pub series_uid: Option<String>,
    pub outcome: Result<Stored, StoreFailure>,
}

pub struct Store {
    files: FileStore,
    index: Index,
    locations: LocationCache,
    options: StoreOptions,
}

impl Store {
    pub fn open(options: StoreOptions) -> Result<Arc<Self>, OpenError> {
        std::fs::create_dir_all(&options.path)?;
        let files = match &options.files {
            FileBackend::Filesystem => FileStore::local(options.path.join("files"), options.fsync)?,
            FileBackend::S3(s3) => FileStore::s3(s3).map_err(|e| OpenError::S3(e.to_string()))?,
        };
        let database = match &options.index {
            IndexBackend::Sqlite => Database::Sqlite(options.path.join("index.sqlite")),
            IndexBackend::Postgres(url) => Database::Postgres(url.clone()),
        };
        let index = Index::open(&database, options.index_readers)?;
        Ok(Arc::new(Self {
            files,
            index,
            locations: LocationCache::new(options.location_cache_bytes, options.location_cache_ttl),
            options,
        }))
    }

    pub fn index(&self) -> &Index {
        &self.index
    }

    pub fn files(&self) -> &FileStore {
        &self.files
    }

    /// Store one Part-10 instance. With `expected_study`, instances of other studies are
    /// rejected (STOW-RS to `/studies/{study}`).
    pub async fn store(
        self: &Arc<Self>,
        bytes: Bytes,
        expected_study: Option<String>,
    ) -> StoreResult {
        let options = IngestOptions {
            inline_binary_max: self.options.inline_binary_max,
        };
        let parsed = {
            let bytes = bytes.clone();
            tokio::task::spawn_blocking(move || ingest::parse(&bytes, options)).await
        };
        let mut record = match parsed {
            Ok(Ok(record)) => record,
            Ok(Err((ids, error))) => return failed(ids, error.into()),
            Err(join) => {
                return failed(
                    Identifiers::default(),
                    IngestError::Unreadable(format!("parser crashed: {join}")).into(),
                );
            }
        };
        let mut result = StoreResult {
            sop_class_uid: Some(record.sop_class_uid.clone()),
            sop_uid: Some(record.sop_uid.clone()),
            study_uid: Some(record.study_uid.clone()),
            series_uid: Some(record.series_uid.clone()),
            outcome: Ok(Stored::New),
        };
        if let Some(expected) = expected_study
            && expected != record.study_uid
        {
            result.outcome = Err(StoreFailure::StudyMismatch {
                expected,
                actual: record.study_uid,
            });
            return result;
        }
        result.outcome = self.persist(&mut record, bytes).await;
        result
    }

    async fn persist(
        self: &Arc<Self>,
        record: &mut ingest::InstanceRecord,
        bytes: Bytes,
    ) -> Result<Stored, StoreFailure> {
        // Cheap early exit for re-sent instances; the writer checks again authoritatively.
        if self.index.instance_exists(&record.sop_uid).await? {
            return Ok(Stored::Duplicate);
        }
        let path = FileStore::new_path();
        self.files.write(&path, bytes).await?;
        record.path = path.clone();

        let outcome = self.index.insert(record.clone()).await;
        let stored = match outcome {
            Ok(InsertOutcome::Inserted) => return Ok(Stored::New),
            Ok(InsertOutcome::Duplicate) => Ok(Stored::Duplicate),
            Ok(InsertOutcome::SeriesConflict) => {
                Err(StoreFailure::SeriesConflict(record.series_uid.clone()))
            }
            Err(e) => Err(e.into()),
        };
        if let Err(e) = self.files.remove(&path).await {
            tracing::warn!(%path, error = %e, "cannot remove discarded file");
        }
        stored
    }
}

/// Counts and size of the stored data.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
pub struct Statistics {
    pub studies: u64,
    pub series: u64,
    pub instances: u64,
    /// Total size of the stored files, in bytes.
    pub size: u64,
}

impl Store {
    /// Delete the instances of `scope` and their files; returns how many were deleted.
    pub async fn delete(self: &Arc<Self>, scope: retrieve::Scope) -> Result<usize, IndexError> {
        let deleted = self.index.delete(scope.clone()).await;
        self.locations.invalidate(&scope);
        let paths = deleted?;
        let count = paths.len();
        for e in self.files.remove_all(paths).await {
            tracing::warn!(error = %e, "cannot remove deleted file");
        }
        Ok(count)
    }

    pub async fn statistics(self: &Arc<Self>) -> Result<Statistics, IndexError> {
        self.index
            .readers()
            .run(|conn| {
                let counts = conn.query_row(
                    "SELECT (SELECT count(*) FROM studies), (SELECT count(*) FROM series),
                        count(*), CAST(coalesce(sum(size), 0) AS BIGINT) FROM instances",
                    &[],
                    |r| {
                        Ok(Statistics {
                            studies: r.i64(0)? as u64,
                            series: r.i64(1)? as u64,
                            instances: r.i64(2)? as u64,
                            size: r.i64(3)? as u64,
                        })
                    },
                )?;
                counts.ok_or_else(|| IndexError::Database("no statistics".into()))
            })
            .await
    }
}

fn failed(ids: Identifiers, failure: StoreFailure) -> StoreResult {
    StoreResult {
        sop_class_uid: ids.sop_class_uid,
        sop_uid: ids.sop_uid,
        study_uid: None,
        series_uid: None,
        outcome: Err(failure),
    }
}
