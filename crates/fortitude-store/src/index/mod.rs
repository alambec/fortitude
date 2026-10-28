//! The SQLite index: one writer thread, a pool of read-only connections.

mod readers;
pub mod schema;
mod writer;

use std::path::{Path, PathBuf};

use rusqlite::{Connection, OpenFlags, OptionalExtension};
use tokio::sync::{mpsc, oneshot};

pub use readers::Readers;
pub use writer::{InsertOutcome, person_name_key};

use crate::ingest::InstanceRecord;
use writer::Command;

#[derive(Debug, Clone, thiserror::Error)]
pub enum IndexError {
    #[error("index error: {0}")]
    Sqlite(String),
    #[error("index writer stopped")]
    WriterStopped,
}

impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        IndexError::Sqlite(e.to_string())
    }
}

pub struct Index {
    writer: mpsc::Sender<Command>,
    readers: Readers,
    path: PathBuf,
}

impl Index {
    pub fn open(path: &Path, readers: usize) -> Result<Self, IndexError> {
        let conn = Connection::open(path)?;
        schema::configure(&conn)?;
        schema::migrate(&conn)?;
        let writer = writer::spawn(conn).map_err(|e| IndexError::Sqlite(e.to_string()))?;
        let readers = Readers::new(readers.max(1), {
            let path = path.to_owned();
            move || open_reader(&path)
        })?;
        Ok(Self {
            writer,
            readers,
            path: path.to_owned(),
        })
    }

    pub fn path(&self) -> &Path {
        &self.path
    }

    pub fn readers(&self) -> &Readers {
        &self.readers
    }

    pub async fn insert(&self, record: InstanceRecord) -> Result<InsertOutcome, IndexError> {
        let (reply, response) = oneshot::channel();
        self.writer
            .send(Command::Insert(Box::new(record), reply))
            .await
            .map_err(|_| IndexError::WriterStopped)?;
        response.await.map_err(|_| IndexError::WriterStopped)?
    }

    pub async fn instance_exists(&self, sop_uid: &str) -> Result<bool, IndexError> {
        let sop_uid = sop_uid.to_owned();
        self.readers
            .run(move |conn| {
                Ok(conn
                    .prepare_cached("SELECT 1 FROM instances WHERE sop_uid = ?")?
                    .query_row([sop_uid], |_| Ok(()))
                    .optional()?
                    .is_some())
            })
            .await
    }
}

fn open_reader(path: &Path) -> rusqlite::Result<Connection> {
    let conn = Connection::open_with_flags(
        path,
        OpenFlags::SQLITE_OPEN_READ_WRITE
            | OpenFlags::SQLITE_OPEN_NO_MUTEX
            | OpenFlags::SQLITE_OPEN_URI,
    )?;
    schema::configure(&conn)?;
    conn.execute_batch("PRAGMA query_only = ON;")?;
    Ok(conn)
}
