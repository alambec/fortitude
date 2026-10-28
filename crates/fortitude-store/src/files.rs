//! Storage of the original Part-10 files.
//!
//! Every stored instance gets a new random path (`ab/cd/<uuid>.dcm`), so a file is never
//! overwritten and a file only becomes visible once the index references it.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

#[derive(Debug)]
pub struct FileStore {
    root: PathBuf,
    fsync: bool,
}

impl FileStore {
    pub fn new(root: PathBuf, fsync: bool) -> io::Result<Self> {
        std::fs::create_dir_all(&root)?;
        Ok(Self { root, fsync })
    }

    /// A fresh relative path for a new file.
    pub fn new_path() -> String {
        let id = uuid::Uuid::new_v4().simple().to_string();
        format!("{}/{}/{id}.dcm", &id[..2], &id[2..4])
    }

    pub fn path(&self, relative: &str) -> PathBuf {
        self.root.join(relative)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn write(&self, relative: &str, bytes: &[u8]) -> io::Result<()> {
        let path = self.path(relative);
        let mut file = match File::create(&path) {
            Ok(file) => file,
            Err(e) if e.kind() == io::ErrorKind::NotFound => {
                std::fs::create_dir_all(path.parent().unwrap_or(&self.root))?;
                File::create(&path)?
            }
            Err(e) => return Err(e),
        };
        file.write_all(bytes)?;
        if self.fsync {
            file.sync_all()?;
        }
        Ok(())
    }

    pub fn remove(&self, relative: &str) -> io::Result<()> {
        match std::fs::remove_file(self.path(relative)) {
            Err(e) if e.kind() != io::ErrorKind::NotFound => Err(e),
            _ => Ok(()),
        }
    }
}
