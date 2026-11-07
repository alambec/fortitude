//! Storage of the original Part-10 files: a local directory or an S3 bucket.
//!
//! Every stored instance gets a new random key (`ab/cd/<uuid>.dcm`), so a file is never
//! overwritten and a file only becomes visible once the index references it. Keys are
//! relative to the storage root: the `files/` directory, or the key prefix in the bucket.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::SystemTime;

use bytes::Bytes;
use dicom_dictionary_std::tags;
use dicom_object::{DefaultDicomObject, OpenFileOptions};
use futures::stream::{BoxStream, StreamExt, TryStreamExt};
use object_store::aws::{AmazonS3, AmazonS3Builder};
use object_store::{ObjectStore, ObjectStoreExt, PutPayload};

use crate::pixel::Range;

/// Where the files are stored.
#[derive(Clone, Default, PartialEq, Eq)]
pub enum FileBackend {
    /// `files/` in the storage directory.
    #[default]
    Filesystem,
    S3(S3Options),
}

impl std::fmt::Debug for FileBackend {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            FileBackend::Filesystem => f.write_str("Filesystem"),
            // The options may hold a secret key.
            FileBackend::S3(s3) => write!(f, "S3({}/{})", s3.bucket, s3.prefix),
        }
    }
}

/// An S3 bucket (AWS or compatible: MinIO, Ceph, Garage, …).
#[derive(Clone, Default, PartialEq, Eq)]
pub struct S3Options {
    pub bucket: String,
    /// Prefix of the keys of the files in the bucket.
    pub prefix: String,
    /// `AWS_REGION` or `us-east-1` when unset.
    pub region: Option<String>,
    /// Endpoint of an S3-compatible service (e.g. `http://localhost:9000`); AWS when unset.
    pub endpoint: Option<String>,
    /// Credentials; when unset, those of the environment (`AWS_ACCESS_KEY_ID`, …, web
    /// identity, instance metadata).
    pub access_key_id: Option<String>,
    pub secret_access_key: Option<String>,
    /// Address buckets as `https://bucket.endpoint` instead of `https://endpoint/bucket`.
    pub virtual_hosted_style: bool,
    /// Remove files with `DeleteObjects` requests (up to 1000 files each) rather than one
    /// request per file. Some S3-compatible services do not implement it.
    pub bulk_delete: bool,
}

#[derive(Clone)]
pub struct FileStore {
    backend: Arc<Backend>,
}

enum Backend {
    Local {
        root: PathBuf,
        fsync: bool,
    },
    S3 {
        bucket: AmazonS3,
        prefix: String,
        /// `s3://bucket/prefix`, for messages.
        url: String,
    },
}

impl std::fmt::Debug for FileStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match &*self.backend {
            Backend::Local { root, .. } => write!(f, "FileStore({})", root.display()),
            Backend::S3 { url, .. } => write!(f, "FileStore({url})"),
        }
    }
}

/// Size of the chunks of a local file stream.
const STREAM_CHUNK: usize = 256 * 1024;

/// A file of the storage, as listed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoredFile {
    pub key: String,
    pub size: u64,
    /// Time of the last modification: for a file that is never rewritten, of its creation.
    pub modified: SystemTime,
}

/// Bytes of a stored file, in chunks.
pub type FileStream = BoxStream<'static, io::Result<Bytes>>;

impl FileStore {
    /// Files in the directory `root`, flushed to disk on write with `fsync`.
    pub fn local(root: PathBuf, fsync: bool) -> io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self {
            backend: Arc::new(Backend::Local { root, fsync }),
        })
    }

    pub fn s3(options: &S3Options) -> object_store::Result<Self> {
        let mut builder = AmazonS3Builder::from_env()
            .with_bucket_name(&options.bucket)
            .with_virtual_hosted_style_request(options.virtual_hosted_style)
            .with_disable_bulk_delete(!options.bulk_delete);
        if let Some(region) = &options.region {
            builder = builder.with_region(region);
        }
        if let Some(endpoint) = &options.endpoint {
            builder = builder
                .with_endpoint(endpoint)
                .with_allow_http(endpoint.starts_with("http://"));
        }
        if let Some(key) = &options.access_key_id {
            builder = builder.with_access_key_id(key);
        }
        if let Some(secret) = &options.secret_access_key {
            builder = builder.with_secret_access_key(secret);
        }
        let bucket = builder.build()?;
        let prefix = options.prefix.trim_matches('/').to_owned();
        Ok(Self {
            backend: Arc::new(Backend::S3 {
                bucket,
                url: format!("s3://{}/{prefix}", options.bucket),
                prefix,
            }),
        })
    }

    /// A fresh key for a new file.
    pub fn new_path() -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("{}/{}/{id}.dcm", &id[..2], &id[2..4])
    }

    /// Check that the storage can be reached (the bucket exists and the credentials work).
    pub async fn check(&self) -> io::Result<()> {
        match &*self.backend {
            Backend::Local { .. } => Ok(()),
            Backend::S3 { bucket, prefix, .. } => {
                let prefix = object_store::path::Path::from(prefix.as_str());
                bucket
                    .list_with_delimiter(Some(&prefix))
                    .await
                    .map(drop)
                    .map_err(io_error)
            }
        }
    }

    pub async fn write(&self, key: &str, bytes: Bytes) -> io::Result<()> {
        match &*self.backend {
            Backend::Local { root, fsync } => {
                let (path, fsync) = (root.join(key), *fsync);
                blocking(move || write_file(&path, &bytes, fsync)).await
            }
            Backend::S3 { bucket, prefix, .. } => bucket
                .put(&location(prefix, key), PutPayload::from_bytes(bytes))
                .await
                .map(drop)
                .map_err(io_error),
        }
    }

    /// Remove a file; removing a missing file succeeds.
    pub async fn remove(&self, key: &str) -> io::Result<()> {
        match &*self.backend {
            Backend::Local { root, .. } => match tokio::fs::remove_file(root.join(key)).await {
                Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
                _ => Ok(()),
            },
            Backend::S3 { bucket, prefix, .. } => {
                match bucket.delete(&location(prefix, key)).await {
                    Err(object_store::Error::NotFound { .. }) | Ok(()) => Ok(()),
                    Err(e) => Err(io_error(e)),
                }
            }
        }
    }

    /// Remove files; returns the errors of those that could not be removed (missing files are
    /// not errors).
    pub async fn remove_all(&self, keys: Vec<String>) -> Vec<io::Error> {
        match &*self.backend {
            Backend::Local { root, .. } => {
                let root = root.clone();
                let removed = blocking(move || {
                    Ok(keys
                        .iter()
                        .filter_map(|key| match std::fs::remove_file(root.join(key)) {
                            Err(e) if e.kind() != io::ErrorKind::NotFound => Some(e),
                            _ => None,
                        })
                        .collect())
                })
                .await;
                removed.unwrap_or_else(|e| vec![e])
            }
            Backend::S3 { bucket, prefix, .. } => {
                let locations: Vec<_> = keys.iter().map(|key| Ok(location(prefix, key))).collect();
                bucket
                    .delete_stream(futures::stream::iter(locations).boxed())
                    .filter_map(|deleted| async move {
                        match deleted {
                            Err(e) if is_missing(&e) => None,
                            Ok(_) => None,
                            Err(e) => Some(io_error(e)),
                        }
                    })
                    .collect()
                    .await
            }
        }
    }

    /// Keys of all the stored files.
    pub async fn keys(&self) -> io::Result<Vec<String>> {
        Ok(self.list("").await?.into_iter().map(|f| f.key).collect())
    }

    /// The stored files whose key starts with the directory `prefix` (e.g. `ab/`, or `""` for
    /// all of them), in no particular order.
    pub async fn list(&self, prefix: &str) -> io::Result<Vec<StoredFile>> {
        let prefix = prefix.trim_matches('/');
        match &*self.backend {
            Backend::Local { root, .. } => {
                let root = root.clone();
                let dir = root.join(prefix);
                blocking(move || {
                    let mut files = Vec::new();
                    match list_dir(&root, &dir, &mut files) {
                        Err(e) if e.kind() == io::ErrorKind::NotFound && dir != root => Ok(files),
                        result => result.map(|()| files),
                    }
                })
                .await
            }
            Backend::S3 {
                bucket,
                prefix: base,
                ..
            } => {
                let skip = object_store::path::Path::from(base.as_str())
                    .parts()
                    .count();
                let listed = if prefix.is_empty() {
                    object_store::path::Path::from(base.as_str())
                } else {
                    location(base, prefix)
                };
                bucket
                    .list(Some(&listed))
                    .map_ok(|meta| StoredFile {
                        key: meta
                            .location
                            .parts()
                            .skip(skip)
                            .map(|part| part.as_ref().to_owned())
                            .collect::<Vec<_>>()
                            .join("/"),
                        size: meta.size,
                        modified: meta.last_modified.into(),
                    })
                    .try_collect()
                    .await
                    .map_err(io_error)
            }
        }
    }

    /// Size of a file; `None` when it does not exist.
    pub async fn size(&self, key: &str) -> io::Result<Option<u64>> {
        let size = match &*self.backend {
            Backend::Local { root, .. } => {
                tokio::fs::metadata(root.join(key)).await.map(|m| m.len())
            }
            Backend::S3 { bucket, prefix, .. } => bucket
                .head(&location(prefix, key))
                .await
                .map(|meta| meta.size)
                .map_err(io_error),
        };
        match size {
            Ok(size) => Ok(Some(size)),
            Err(e) if e.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(e) => Err(e),
        }
    }

    pub async fn read(&self, key: &str) -> io::Result<Bytes> {
        match &*self.backend {
            Backend::Local { root, .. } => tokio::fs::read(root.join(key)).await.map(Bytes::from),
            Backend::S3 { bucket, prefix, .. } => {
                let result = bucket.get(&location(prefix, key)).await;
                result.map_err(io_error)?.bytes().await.map_err(io_error)
            }
        }
    }

    /// The file in chunks, for files too large to be read at once.
    pub async fn stream(&self, key: &str) -> io::Result<FileStream> {
        match &*self.backend {
            Backend::Local { root, .. } => {
                let file = tokio::fs::File::open(root.join(key)).await?;
                Ok(tokio_util::io::ReaderStream::with_capacity(file, STREAM_CHUNK).boxed())
            }
            Backend::S3 { bucket, prefix, .. } => {
                let result = bucket.get(&location(prefix, key)).await;
                Ok(result
                    .map_err(io_error)?
                    .into_stream()
                    .map_err(io_error)
                    .boxed())
            }
        }
    }

    /// CRC-32 of a file, read in chunks; fails with [`io::ErrorKind::NotFound`] when it does not
    /// exist.
    pub async fn crc32(&self, key: &str) -> io::Result<u32> {
        match &*self.backend {
            Backend::Local { root, .. } => {
                let path = root.join(key);
                blocking(move || {
                    use std::io::Read;
                    let mut file = File::open(path)?;
                    let mut hasher = crc32fast::Hasher::new();
                    let mut buffer = vec![0; STREAM_CHUNK];
                    loop {
                        match file.read(&mut buffer) {
                            Ok(0) => return Ok(hasher.finalize()),
                            Ok(read) => hasher.update(&buffer[..read]),
                            Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                            Err(e) => return Err(e),
                        }
                    }
                })
                .await
            }
            Backend::S3 { .. } => {
                let mut chunks = self.stream(key).await?;
                let mut hasher = crc32fast::Hasher::new();
                while let Some(chunk) = chunks.try_next().await? {
                    hasher.update(&chunk);
                }
                Ok(hasher.finalize())
            }
        }
    }

    /// Parts of a file, each the concatenation of byte ranges (e.g. frames, made of fragments).
    pub async fn read_parts(&self, key: &str, parts: Vec<Vec<Range>>) -> io::Result<Vec<Vec<u8>>> {
        match &*self.backend {
            Backend::Local { root, .. } => {
                let path = root.join(key);
                blocking(move || {
                    use std::os::unix::fs::FileExt;
                    let file = File::open(path)?;
                    parts
                        .iter()
                        .map(|ranges| {
                            let total: u64 = ranges.iter().map(|r| r.length).sum();
                            let mut out = vec![0; total as usize];
                            let mut position = 0;
                            for range in ranges {
                                let end = position + range.length as usize;
                                file.read_exact_at(&mut out[position..end], range.offset)?;
                                position = end;
                            }
                            Ok(out)
                        })
                        .collect()
                })
                .await
            }
            Backend::S3 { bucket, prefix, .. } => {
                let wanted: Vec<_> = parts
                    .iter()
                    .flatten()
                    .map(|r| r.offset..r.offset + r.length)
                    .collect();
                // Close ranges (fragments, consecutive frames) are fetched in one request.
                let mut fetched = bucket
                    .get_ranges(&location(prefix, key), &wanted)
                    .await
                    .map_err(io_error)?
                    .into_iter();
                Ok(parts
                    .iter()
                    .map(|ranges| {
                        fetched
                            .by_ref()
                            .take(ranges.len())
                            .collect::<Vec<_>>()
                            .concat()
                    })
                    .collect())
            }
        }
    }

    /// The data set of a file up to its pixel data (excluded). `header_length`, when known,
    /// is the length of the file before the pixel data value: only that part is read.
    pub async fn header(
        &self,
        key: &str,
        header_length: Option<u64>,
    ) -> io::Result<DefaultDicomObject> {
        let options = OpenFileOptions::new().read_until(tags::FLOAT_PIXEL_DATA);
        match (&*self.backend, header_length) {
            (Backend::Local { root, .. }, _) => {
                let path = root.join(key);
                blocking(move || options.open_file(path).map_err(io::Error::other)).await
            }
            (Backend::S3 { .. }, Some(length)) => {
                let range = Range { offset: 0, length };
                let bytes = self.read_parts(key, vec![vec![range]]).await?.concat();
                blocking(move || options.from_reader(&bytes[..]).map_err(io::Error::other)).await
            }
            (Backend::S3 { .. }, None) => {
                let bytes = self.read(key).await?;
                blocking(move || options.from_reader(&bytes[..]).map_err(io::Error::other)).await
            }
        }
    }

    /// The whole data set of a file, pixel data included.
    pub async fn object(&self, key: &str) -> io::Result<DefaultDicomObject> {
        match &*self.backend {
            Backend::Local { root, .. } => {
                let path = root.join(key);
                blocking(move || dicom_object::open_file(path).map_err(io::Error::other)).await
            }
            Backend::S3 { .. } => {
                let bytes = self.read(key).await?;
                blocking(move || {
                    OpenFileOptions::new()
                        .from_reader(&bytes[..])
                        .map_err(io::Error::other)
                })
                .await
            }
        }
    }
}

fn location(prefix: &str, key: &str) -> object_store::path::Path {
    if prefix.is_empty() {
        key.into()
    } else {
        format!("{prefix}/{key}").into()
    }
}

/// Whether a removal failed only because the object was already gone. AWS reports keys
/// missing from a DeleteObjects request as deleted, but some compatible services (e.g.
/// versitygw) report them as per-key `NoSuchKey` errors, which `object_store` only passes
/// on as generic errors.
fn is_missing(e: &object_store::Error) -> bool {
    match e {
        object_store::Error::NotFound { .. } => true,
        object_store::Error::Generic { source, .. } => {
            source.to_string().contains("(code: NoSuchKey)")
        }
        _ => false,
    }
}

fn io_error(e: object_store::Error) -> io::Error {
    match e {
        object_store::Error::NotFound { .. } => io::Error::new(io::ErrorKind::NotFound, e),
        e => io::Error::other(e),
    }
}

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> io::Result<T> + Send + 'static,
) -> io::Result<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(io::Error::other)?
}

fn write_file(path: &Path, bytes: &[u8], fsync: bool) -> io::Result<()> {
    let mut file = match File::create(path) {
        Ok(file) => file,
        Err(e) if e.kind() == io::ErrorKind::NotFound => {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent)?;
            }
            File::create(path)?
        }
        Err(e) => return Err(e),
    };
    file.write_all(bytes)?;
    if fsync {
        file.sync_all()?;
    }
    Ok(())
}

fn list_dir(root: &Path, dir: &Path, files: &mut Vec<StoredFile>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)? {
        let entry = entry?;
        if entry.file_type()?.is_dir() {
            list_dir(root, &entry.path(), files)?;
        } else if let Ok(key) = entry.path().strip_prefix(root) {
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                // Removed since the directory was read.
                Err(e) if e.kind() == io::ErrorKind::NotFound => continue,
                Err(e) => return Err(e),
            };
            files.push(StoredFile {
                key: key.to_string_lossy().into_owned(),
                size: metadata.len(),
                modified: metadata.modified()?,
            });
        }
    }
    Ok(())
}

#[cfg(test)]
mod missing_tests {
    use super::*;

    #[test]
    fn keys_already_deleted_are_missing() {
        let generic = |message: &str| object_store::Error::Generic {
            store: "S3",
            source: message.to_owned().into(),
        };
        assert!(is_missing(&generic(
            "DeleteObjects request failed for key a/b/c.dcm: The specified key does not exist. \
             (code: NoSuchKey)"
        )));
        assert!(!is_missing(&generic(
            "DeleteObjects request failed for key a/b/c.dcm: Access Denied (code: AccessDenied)"
        )));
        assert!(is_missing(&object_store::Error::NotFound {
            path: "a/b/c.dcm".into(),
            source: "gone".into(),
        }));
    }
}
