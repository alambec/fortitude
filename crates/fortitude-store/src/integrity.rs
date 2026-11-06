//! Integrity check: compare the files of the storage with the index.
//!
//! Files and index are not updated in one transaction: a file is written before its index
//! row is committed, and an instance is removed from the index before its file is deleted.
//! A crash, or a failed removal, leaves **orphan** files that no instance references. Files
//! can also go **missing** or change size behind the server's back (removed by hand, a lost
//! disk, a bucket lifecycle rule). The check finds both:
//!
//! - orphan files older than a grace period are removed (younger ones may belong to an
//!   ingest in progress, whose index row is not committed yet);
//! - instances whose file is missing or does not have the size recorded at ingest are
//!   reported, never deleted: whether to delete them or restore their files is the
//!   administrator's decision.
//!
//! On request, the check also reads every file and compares its CRC-32 with the one recorded
//! at ingest, to find files corrupted with their size intact (bit rot, a faulty disk or
//! controller, a bad copy). This reads the whole storage, so it is not done by default.
//!
//! Storage keys are `ab/cd/<uuid>.dcm`, so the check proceeds one first-level directory
//! (`ab/`, 256 of them) at a time, with an index range query on `instances.path` for each:
//! memory stays bounded by the size of one directory, whatever the size of the archive.
//! Files whose key does not have this form were not written by Fortitude, and are left
//! alone.
//!
//! The check runs while the server stores and deletes instances. Before anything is reported
//! or removed, it is checked again: a missing file may have been written since the listing
//! (or its instance deleted), a size mismatch may be a file listed while it was written, and
//! an orphan is looked up in the index once more right before its removal. A file is never
//! rewritten under the same key, so a file that matches neither its size nor its checksum
//! while its instance is indexed, before and after it was looked at, is damaged.

use std::collections::HashMap;
use std::io;
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::stream::{self, StreamExt};
use serde::{Serialize, Serializer};

use crate::Store;
use crate::files::StoredFile;
use crate::index::{IndexError, Value};
use crate::retrieve::Scope;

/// Problems listed in a report; the counts include the others.
pub const MAX_LISTED_PROBLEMS: usize = 1000;
/// Errors listed in a report.
const MAX_LISTED_ERRORS: usize = 100;
/// First-level directories checked at the same time.
const CONCURRENT_SHARDS: usize = 4;
/// Files whose size is looked up, or whose checksum is computed, at the same time by a check
/// of a study or series.
const CONCURRENT_FILES: usize = 16;
/// Files whose checksum is computed at the same time in each directory of a full check.
const CONCURRENT_CHECKSUMS: usize = 4;

#[derive(Debug, Clone, Copy)]
pub struct IntegrityOptions {
    /// Remove orphan files; when `false`, they are only counted.
    pub remove_orphans: bool,
    /// Files younger than this are never orphans.
    pub orphan_grace: Duration,
    /// Read every file and compare its checksum with the one recorded at ingest.
    pub verify_checksums: bool,
}

/// Result of an integrity check.
#[derive(Debug, Clone, Default, PartialEq, Serialize)]
pub struct IntegrityReport {
    /// The study, and series, of a check limited to them; absent for a full check.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub study_uid: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub series_uid: Option<String>,
    /// Start, in seconds since the Unix epoch.
    pub started_at: u64,
    pub duration_seconds: f64,
    pub remove_orphans: bool,
    pub verify_checksums: bool,
    /// Stored files, and indexed instances, that were checked.
    pub files: u64,
    pub instances: u64,
    /// Files that no instance references, older than the grace period, and their total size.
    pub orphans: u64,
    pub orphans_size: u64,
    pub orphans_removed: u64,
    /// Unreferenced files younger than the grace period, left alone.
    pub recent_files: u64,
    /// Files whose key was not written by Fortitude, left alone.
    pub foreign_files: u64,
    /// Instances whose file is missing.
    pub missing: u64,
    /// Instances whose file does not have the size recorded at ingest.
    pub size_mismatches: u64,
    /// When verifying checksums: files whose checksum matched, and files of the right size
    /// whose checksum did not.
    pub checksums_verified: u64,
    pub checksum_mismatches: u64,
    /// The first [`MAX_LISTED_PROBLEMS`] missing files, size and checksum mismatches.
    pub problems: Vec<Problem>,
    /// Directories that could not be checked, and files that could not be removed.
    pub errors: Vec<String>,
}

impl IntegrityReport {
    fn new(started_at: SystemTime) -> Self {
        Self {
            started_at: started_at
                .duration_since(UNIX_EPOCH)
                .unwrap_or_default()
                .as_secs(),
            ..Self::default()
        }
    }

    /// No missing file, no size or checksum mismatch, no error.
    pub fn is_clean(&self) -> bool {
        self.missing == 0
            && self.size_mismatches == 0
            && self.checksum_mismatches == 0
            && self.errors.is_empty()
    }

    /// Count a problem, and log and list it while the list is not full.
    fn push_problem(&mut self, problem: Problem) {
        match problem.kind {
            ProblemKind::Missing => self.missing += 1,
            ProblemKind::SizeMismatch => self.size_mismatches += 1,
            ProblemKind::ChecksumMismatch => self.checksum_mismatches += 1,
        }
        if self.problems.len() < MAX_LISTED_PROBLEMS {
            tracing::warn!(
                kind = ?problem.kind,
                study_uid = problem.study_uid,
                series_uid = problem.series_uid,
                sop_uid = problem.sop_uid,
                path = problem.path,
                expected_size = problem.expected_size,
                actual_size = problem.actual_size,
                expected_crc32 = problem.expected_crc32.map(|c| c.to_string()),
                actual_crc32 = problem.actual_crc32.map(|c| c.to_string()),
                "integrity check: stored file does not match the index"
            );
            self.problems.push(problem);
        }
    }

    fn push_error(&mut self, error: String) {
        tracing::error!(error, "integrity check");
        if self.errors.len() < MAX_LISTED_ERRORS {
            self.errors.push(error);
        }
    }

    fn finish(&mut self, started: Instant) {
        self.problems.sort_by(|a, b| a.path.cmp(&b.path));
        self.duration_seconds = started.elapsed().as_secs_f64();
        tracing::info!(
            study_uid = self.study_uid,
            series_uid = self.series_uid,
            files = self.files,
            instances = self.instances,
            orphans = self.orphans,
            orphans_removed = self.orphans_removed,
            missing = self.missing,
            size_mismatches = self.size_mismatches,
            checksums_verified = self.checksums_verified,
            checksum_mismatches = self.checksum_mismatches,
            errors = self.errors.len(),
            seconds = self.duration_seconds,
            "integrity check finished"
        );
    }
}

/// An instance whose file is missing, or has the wrong size or checksum.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct Problem {
    pub kind: ProblemKind,
    pub study_uid: String,
    pub series_uid: String,
    pub sop_uid: String,
    pub path: String,
    /// Size recorded at ingest, and size of the file (`None` when missing).
    pub expected_size: u64,
    pub actual_size: Option<u64>,
    /// Checksum recorded at ingest, and that of the file: only for checksum mismatches.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expected_crc32: Option<Crc32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actual_crc32: Option<Crc32>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProblemKind {
    Missing,
    SizeMismatch,
    /// The file has the right size, but not the checksum recorded at ingest.
    ChecksumMismatch,
}

/// A CRC-32, shown (and serialized) as 8 hexadecimal digits.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Crc32(pub u32);

impl std::fmt::Display for Crc32 {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{:08x}", self.0)
    }
}

impl Serialize for Crc32 {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

/// What the index records about the file of an instance.
#[derive(Debug, Clone, Copy)]
struct Indexed {
    size: u64,
    crc32: u32,
}

/// What one directory contributes to a report.
#[derive(Default)]
struct ShardReport {
    files: u64,
    instances: u64,
    orphans: u64,
    orphans_size: u64,
    orphans_removed: u64,
    recent_files: u64,
    foreign_files: u64,
    checksums_verified: u64,
    problems: Vec<Problem>,
    errors: Vec<String>,
}

impl Store {
    /// Compare the stored files with the index; see the [module](self) documentation.
    pub async fn check_integrity(self: &Arc<Self>, options: IntegrityOptions) -> IntegrityReport {
        let started = Instant::now();
        let started_at = SystemTime::now();
        tracing::info!(
            remove_orphans = options.remove_orphans,
            verify_checksums = options.verify_checksums,
            "integrity check started"
        );
        let mut report = IntegrityReport {
            remove_orphans: options.remove_orphans,
            verify_checksums: options.verify_checksums,
            ..IntegrityReport::new(started_at)
        };
        // Files modified after this are recent, wherever the check is when it lists them.
        let orphan_before = started_at
            .checked_sub(options.orphan_grace)
            .unwrap_or(UNIX_EPOCH);

        let mut shards = stream::iter(0..=255u8)
            .map(|shard| {
                let store = self.clone();
                async move {
                    let prefix = format!("{shard:02x}/");
                    match store.check_shard(&prefix, options, orphan_before).await {
                        Ok(shard) => shard,
                        Err(e) => ShardReport {
                            errors: vec![format!("{prefix}: {e}")],
                            ..ShardReport::default()
                        },
                    }
                }
            })
            .buffer_unordered(CONCURRENT_SHARDS);
        while let Some(shard) = shards.next().await {
            report.files += shard.files;
            report.instances += shard.instances;
            report.orphans += shard.orphans;
            report.orphans_size += shard.orphans_size;
            report.orphans_removed += shard.orphans_removed;
            report.recent_files += shard.recent_files;
            report.foreign_files += shard.foreign_files;
            report.checksums_verified += shard.checksums_verified;
            for problem in shard.problems {
                report.push_problem(problem);
            }
            for error in shard.errors {
                report.push_error(error);
            }
        }
        report.finish(started);
        metrics::gauge!("fortitude_integrity_last_run_timestamp_seconds")
            .set(report.started_at as f64);
        metrics::gauge!("fortitude_integrity_last_run_duration_seconds")
            .set(report.duration_seconds);
        metrics::gauge!("fortitude_integrity_orphans").set(report.orphans as f64);
        metrics::gauge!("fortitude_integrity_missing").set(report.missing as f64);
        metrics::gauge!("fortitude_integrity_size_mismatches").set(report.size_mismatches as f64);
        // Not measured by a check without checksums: the last measure stands.
        if report.verify_checksums {
            metrics::gauge!("fortitude_integrity_checksum_mismatches")
                .set(report.checksum_mismatches as f64);
        }
        metrics::gauge!("fortitude_integrity_errors").set(report.errors.len() as f64);
        metrics::counter!("fortitude_integrity_orphans_removed_total")
            .increment(report.orphans_removed);
        report
    }

    /// Check the files of the instances of a study, or of a series: missing files, size
    /// mismatches and, with `verify_checksums`, checksum mismatches. Orphan files belong to no
    /// study: only the full check finds them. `None` when the study or series is not stored.
    pub async fn check_scope_integrity(
        self: &Arc<Self>,
        scope: Scope,
        verify_checksums: bool,
    ) -> Result<Option<IntegrityReport>, IndexError> {
        let started = Instant::now();
        let mut report = IntegrityReport {
            study_uid: Some(scope.study.clone()),
            series_uid: scope.series.clone(),
            verify_checksums,
            ..IntegrityReport::new(SystemTime::now())
        };
        let (sql, params) = scope.select("i.path, i.size, i.crc32");
        let instances: Vec<(String, Indexed)> = self
            .index()
            .readers()
            .run(move |conn| {
                conn.query(&sql, &params, |r| {
                    let indexed = Indexed {
                        size: r.i64(1)? as u64,
                        crc32: r.i64(2)? as u32,
                    };
                    Ok((r.str(0)?.to_owned(), indexed))
                })
            })
            .await?;
        if instances.is_empty() {
            return Ok(None);
        }
        report.instances = instances.len() as u64;
        let mut observed = stream::iter(instances)
            .map(|(path, indexed)| async move {
                let observed = match self.files().size(&path).await {
                    Ok(Some(size)) if size != indexed.size => Ok(Observed::Suspect),
                    Ok(Some(_)) if !verify_checksums => Ok(Observed::Intact),
                    Ok(Some(_)) => self.verify_checksum(&path, indexed.crc32).await,
                    Ok(None) => Ok(Observed::Suspect),
                    Err(e) => Err(e),
                };
                (path, observed)
            })
            .buffer_unordered(CONCURRENT_FILES);
        let mut suspects = Vec::new();
        while let Some((path, observed)) = observed.next().await {
            match observed {
                Ok(Observed::Suspect) => suspects.push(path),
                Ok(observed) => {
                    report.files += 1;
                    report.checksums_verified += u64::from(observed == Observed::Verified);
                }
                Err(e) => report.push_error(format!("{path}: {e}")),
            }
        }
        for path in suspects {
            match self.recheck(&path, verify_checksums).await {
                Ok(Some(problem)) => {
                    report.files += u64::from(problem.actual_size.is_some());
                    report.push_problem(problem);
                }
                // Deleted, or fixed, meanwhile.
                Ok(None) => {}
                Err(e) => report.push_error(format!("{path}: {e}")),
            }
        }
        report.finish(started);
        Ok(Some(report))
    }

    async fn check_shard(
        self: &Arc<Self>,
        prefix: &str,
        options: IntegrityOptions,
        orphan_before: SystemTime,
    ) -> Result<ShardReport, String> {
        let mut shard = ShardReport::default();
        // Listed first: a file written after the listing cannot be taken for an orphan.
        let listed = self.files().list(prefix).await.map_err(|e| e.to_string())?;
        let verify = options.verify_checksums;
        let mut indexed = self
            .indexed_files(prefix)
            .await
            .map_err(|e| e.to_string())?;
        shard.instances = indexed.len() as u64;

        let mut orphans: Vec<StoredFile> = Vec::new();
        let mut suspects = Vec::new();
        let mut intact = Vec::new();
        for file in listed {
            if !is_storage_key(&file.key) {
                shard.foreign_files += 1;
                continue;
            }
            shard.files += 1;
            match indexed.remove(&file.key) {
                Some(indexed) if indexed.size != file.size => suspects.push(file.key),
                Some(indexed) if verify => intact.push((file.key, indexed.crc32)),
                Some(_) => {}
                None if file.modified < orphan_before => orphans.push(file),
                None => shard.recent_files += 1,
            }
        }

        let mut verified = stream::iter(intact)
            .map(|(path, crc32)| async move {
                let observed = self.verify_checksum(&path, crc32).await;
                (path, observed)
            })
            .buffer_unordered(CONCURRENT_CHECKSUMS);
        while let Some((path, observed)) = verified.next().await {
            match observed {
                Ok(Observed::Verified) => shard.checksums_verified += 1,
                Ok(Observed::Intact) => {}
                Ok(Observed::Suspect) => suspects.push(path),
                Err(e) => shard.errors.push(format!("{path}: {e}")),
            }
        }

        // Not listed: missing, or written since the listing, or deleted since the query.
        for path in indexed.into_keys().chain(suspects) {
            match self.recheck(&path, verify).await {
                Ok(problem) => shard.problems.extend(problem),
                Err(e) => shard.errors.push(format!("{path}: {e}")),
            }
        }

        if !orphans.is_empty() {
            let paths: Vec<String> = orphans.iter().map(|f| f.key.clone()).collect();
            let referenced = self.referenced(paths).await.map_err(|e| e.to_string())?;
            orphans.retain(|f| !referenced.contains(&f.key));
            shard.orphans = orphans.len() as u64;
            shard.orphans_size = orphans.iter().map(|f| f.size).sum();
            if options.remove_orphans && !orphans.is_empty() {
                let count = orphans.len() as u64;
                let errors = self
                    .files()
                    .remove_all(orphans.into_iter().map(|f| f.key).collect())
                    .await;
                shard.orphans_removed = count.saturating_sub(errors.len() as u64);
                shard.errors.extend(
                    errors
                        .iter()
                        .map(|e| format!("cannot remove orphan file: {e}")),
                );
            }
        }
        Ok(shard)
    }

    /// Path, size and checksum of the indexed instances whose file is under `prefix`, read
    /// from the `instances_path` index alone.
    async fn indexed_files(&self, prefix: &str) -> Result<HashMap<String, Indexed>, IndexError> {
        let (start, end) = key_range(prefix);
        self.index()
            .readers()
            .run(move |conn| {
                let mut files = HashMap::new();
                conn.for_each(
                    "SELECT path, size, crc32 FROM instances WHERE path >= ? AND path < ?",
                    &[Value::Text(start), Value::Text(end)],
                    |r| {
                        let indexed = Indexed {
                            size: r.i64(1)? as u64,
                            crc32: r.i64(2)? as u32,
                        };
                        files.insert(r.str(0)?.to_owned(), indexed);
                        Ok(true)
                    },
                )?;
                Ok(files)
            })
            .await
    }

    /// Compare the checksum of a file of the right size with the one recorded at ingest.
    /// A file that is gone is a suspect: missing, or of an instance deleted meanwhile.
    async fn verify_checksum(&self, path: &str, expected: u32) -> io::Result<Observed> {
        match self.files().crc32(path).await {
            Ok(actual) if actual == expected => Ok(Observed::Verified),
            Ok(_) => Ok(Observed::Suspect),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(Observed::Suspect),
            Err(e) => Err(e),
        }
    }

    /// Those of `paths` that an instance references.
    async fn referenced(&self, paths: Vec<String>) -> Result<Vec<String>, IndexError> {
        self.index()
            .readers()
            .run(move |conn| {
                let mut referenced = Vec::new();
                for path in paths {
                    let found = conn.query_row(
                        "SELECT 1 FROM instances WHERE path = ?",
                        &[Value::Text(path.clone())],
                        |_| Ok(()),
                    )?;
                    if found.is_some() {
                        referenced.push(path);
                    }
                }
                Ok(referenced)
            })
            .await
    }

    /// Look at a suspect file and its instance again: the problem, if it is still there.
    ///
    /// The instance is looked up before and after its file: an instance is removed from the
    /// index before its file is deleted, and indexed after its file is written, so a file
    /// looked at while its instance is indexed throughout is complete and not being deleted.
    async fn recheck(&self, path: &str, verify_checksum: bool) -> Result<Option<Problem>, String> {
        // Deleted since the listing: nothing is wrong.
        let Some(instance) = self.indexed_instance(path).await? else {
            return Ok(None);
        };
        let actual_size = self.files().size(path).await.map_err(|e| e.to_string())?;
        let mut actual_crc32 = None;
        let kind = match actual_size {
            None => ProblemKind::Missing,
            Some(size) if size != instance.size => ProblemKind::SizeMismatch,
            Some(_) if !verify_checksum => return Ok(None),
            Some(_) => match self.files().crc32(path).await {
                Ok(actual) if actual == instance.crc32 => return Ok(None),
                Ok(actual) => {
                    actual_crc32 = Some(Crc32(actual));
                    ProblemKind::ChecksumMismatch
                }
                // Removed since its size was read: deleted, or missing.
                Err(e) if e.kind() == io::ErrorKind::NotFound => ProblemKind::Missing,
                Err(e) => return Err(e.to_string()),
            },
        };
        if self.indexed_instance(path).await?.is_none() {
            return Ok(None);
        }
        Ok(Some(Problem {
            kind,
            study_uid: instance.study_uid,
            series_uid: instance.series_uid,
            sop_uid: instance.sop_uid,
            path: path.to_owned(),
            expected_size: instance.size,
            actual_size: actual_size.filter(|_| kind != ProblemKind::Missing),
            expected_crc32: actual_crc32.map(|_| Crc32(instance.crc32)),
            actual_crc32,
        }))
    }

    /// The instance whose file is at `path`, if any.
    async fn indexed_instance(&self, path: &str) -> Result<Option<IndexedInstance>, String> {
        let key = Value::Text(path.to_owned());
        self.index()
            .readers()
            .run(move |conn| {
                conn.query_row(
                    "SELECT st.study_uid, s.series_uid, i.sop_uid, i.size, i.crc32
                     FROM instances i
                     JOIN series s ON s.pk = i.series_pk
                     JOIN studies st ON st.pk = i.study_pk
                     WHERE i.path = ?",
                    &[key],
                    |r| {
                        Ok(IndexedInstance {
                            study_uid: r.str(0)?.to_owned(),
                            series_uid: r.str(1)?.to_owned(),
                            sop_uid: r.str(2)?.to_owned(),
                            size: r.i64(3)? as u64,
                            crc32: r.i64(4)? as u32,
                        })
                    },
                )
            })
            .await
            .map_err(|e| e.to_string())
    }
}

/// What looking at a file found.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Observed {
    /// Right size; checksum not verified.
    Intact,
    /// Right size and checksum.
    Verified,
    /// Missing, or not of the right size or checksum: to be checked again.
    Suspect,
}

struct IndexedInstance {
    study_uid: String,
    series_uid: String,
    sop_uid: String,
    size: u64,
    crc32: u32,
}

/// Whether `key` has the form of the keys Fortitude writes: `ab/cd/<32 hex digits>.dcm`,
/// where `abcd` starts the hex digits.
pub fn is_storage_key(key: &str) -> bool {
    let hex = |s: &str| s.bytes().all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
    let mut parts = key.split('/');
    let (Some(first), Some(second), Some(name), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return false;
    };
    let Some(id) = name.strip_suffix(".dcm") else {
        return false;
    };
    id.len() == 32 && hex(id) && id.get(..2) == Some(first) && id.get(2..4) == Some(second)
}

/// Bounds of the keys starting with `prefix` (which ends with `/`): `[prefix, end)`, compared
/// bytewise as both index backends do (`/` + 1 is `0`).
fn key_range(prefix: &str) -> (String, String) {
    let stem = prefix.trim_end_matches('/');
    (format!("{stem}/"), format!("{stem}0"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::files::FileStore;

    #[test]
    fn storage_keys() {
        let key = FileStore::new_path();
        assert!(is_storage_key(&key), "{key}");
        assert!(is_storage_key("ab/cd/abcd0123456789abcdef0123456789ab.dcm"));
        for foreign in [
            "ab/cd/abcd0123456789abcdef0123456789ab.dcm.tmp",
            "ab/ce/abcd0123456789abcdef0123456789ab.dcm",
            "ab/cd/ABCD0123456789ABCDEF0123456789AB.dcm",
            "ab/cd/abcd.dcm",
            "abcd0123456789abcdef0123456789ab.dcm",
            "x/ab/cd/abcd0123456789abcdef0123456789ab.dcm",
            "README",
        ] {
            assert!(!is_storage_key(foreign), "{foreign}");
        }
    }

    #[test]
    fn key_ranges() {
        let (start, end) = key_range("ab/");
        assert!(start.as_str() <= "ab/cd/x.dcm" && "ab/ff/x.dcm" < end.as_str());
        assert!("ab.dcm" < start.as_str() && end.as_str() <= "ab0");
        assert!("ac/00/x.dcm" >= end.as_str());
    }
}
