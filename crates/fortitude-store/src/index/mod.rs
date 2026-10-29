//! The index, in SQLite or PostgreSQL: one writer thread, a pool of read connections.

mod db;
mod readers;
pub mod schema;
mod writer;

use std::path::PathBuf;
use std::sync::Arc;

use tokio::sync::{mpsc, oneshot};

pub use db::{Connection, Dialect, Row, Value};
pub use readers::Readers;
pub use writer::{InsertOutcome, person_name_key};

use crate::ingest::InstanceRecord;
use crate::retrieve::Scope;
use writer::Command;

/// Database holding the index.
#[derive(Clone, PartialEq, Eq)]
pub enum Database {
    /// SQLite database file.
    Sqlite(PathBuf),
    /// PostgreSQL connection string: a URL (`postgres://user:password@host/database`) or
    /// key-value pairs (`host=… user=… dbname=…`).
    Postgres(String),
}

impl std::fmt::Debug for Database {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Database::Sqlite(path) => f.debug_tuple("Sqlite").field(path).finish(),
            // The connection string may hold a password.
            Database::Postgres(_) => f.write_str("Postgres(..)"),
        }
    }
}

impl Database {
    fn connect(&self) -> Result<Connection, IndexError> {
        match self {
            Database::Sqlite(path) => Connection::sqlite(path),
            Database::Postgres(url) => Connection::postgres(url),
        }
    }

    fn connect_reader(&self) -> Result<Connection, IndexError> {
        let mut conn = self.connect()?;
        conn.batch(match self {
            Database::Sqlite(_) => "PRAGMA query_only = ON;",
            Database::Postgres(_) => "SET default_transaction_read_only = on;",
        })?;
        Ok(conn)
    }
}

#[derive(Debug, Clone, thiserror::Error)]
pub enum IndexError {
    #[error("index error: {0}")]
    Database(String),
    #[error("index writer stopped")]
    WriterStopped,
}

impl From<rusqlite::Error> for IndexError {
    fn from(e: rusqlite::Error) -> Self {
        IndexError::Database(e.to_string())
    }
}

impl From<rusqlite::types::FromSqlError> for IndexError {
    fn from(e: rusqlite::types::FromSqlError) -> Self {
        IndexError::Database(e.to_string())
    }
}

impl From<postgres::Error> for IndexError {
    fn from(e: postgres::Error) -> Self {
        // The source carries the server's message, which `Display` leaves out.
        IndexError::Database(match std::error::Error::source(&e) {
            Some(source) => format!("{e}: {source}"),
            None => e.to_string(),
        })
    }
}

pub struct Index {
    writer: mpsc::Sender<Command>,
    readers: Readers,
}

impl Index {
    pub fn open(database: &Database, readers: usize) -> Result<Self, IndexError> {
        // The PostgreSQL client blocks on a runtime of its own, which cannot be done from
        // within an asynchronous context.
        std::thread::scope(|scope| {
            scope
                .spawn(|| Self::open_blocking(database, readers))
                .join()
        })
        .map_err(|_| IndexError::Database("cannot open the index".into()))?
    }

    fn open_blocking(database: &Database, readers: usize) -> Result<Self, IndexError> {
        let mut conn = database.connect()?;
        schema::migrate(&mut conn)?;
        let writer = writer::spawn(conn, {
            let database = database.clone();
            move || database.connect()
        })
        .map_err(|e| IndexError::Database(e.to_string()))?;
        let database = Arc::new(database.clone());
        let readers = Readers::new(readers.max(1), move || database.connect_reader())?;
        Ok(Self { writer, readers })
    }

    pub fn dialect(&self) -> Dialect {
        self.readers.dialect()
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

    /// Delete the instances of `scope` from the index; returns the paths of their files.
    pub async fn delete(&self, scope: Scope) -> Result<Vec<String>, IndexError> {
        let (reply, response) = oneshot::channel();
        self.writer
            .send(Command::Delete(scope, reply))
            .await
            .map_err(|_| IndexError::WriterStopped)?;
        response.await.map_err(|_| IndexError::WriterStopped)?
    }

    pub async fn instance_exists(&self, sop_uid: &str) -> Result<bool, IndexError> {
        let sop_uid = Value::Text(sop_uid.to_owned());
        self.readers
            .run(move |conn| {
                Ok(conn
                    .query_row(
                        "SELECT 1 FROM instances WHERE sop_uid = ?",
                        &[sop_uid],
                        |_| Ok(()),
                    )?
                    .is_some())
            })
            .await
    }
}
