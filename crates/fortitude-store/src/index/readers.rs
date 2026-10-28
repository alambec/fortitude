//! Pool of read connections. Queries run on the blocking thread pool, at most one per
//! connection; WAL mode lets them proceed while the writer commits.

use std::sync::{Arc, Mutex};

use rusqlite::Connection;
use tokio::sync::Semaphore;

use super::IndexError;

type Opener = dyn Fn() -> rusqlite::Result<Connection> + Send + Sync;

pub struct Readers {
    idle: Arc<Mutex<Vec<Connection>>>,
    permits: Semaphore,
    open: Box<Opener>,
}

impl Readers {
    pub fn new(
        size: usize,
        open: impl Fn() -> rusqlite::Result<Connection> + Send + Sync + 'static,
    ) -> Result<Self, IndexError> {
        let idle = (0..size)
            .map(|_| open())
            .collect::<rusqlite::Result<Vec<_>>>()?;
        Ok(Self {
            idle: Arc::new(Mutex::new(idle)),
            permits: Semaphore::new(size),
            open: Box::new(open),
        })
    }

    /// Run `query` on a read connection.
    pub async fn run<R, F>(&self, query: F) -> Result<R, IndexError>
    where
        F: FnOnce(&Connection) -> Result<R, IndexError> + Send + 'static,
        R: Send + 'static,
    {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| IndexError::WriterStopped)?;
        let popped = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        // A connection is missing only if a query panicked while holding it.
        let conn = match popped {
            Some(conn) => conn,
            None => (self.open)()?,
        };
        let idle = self.idle.clone();
        tokio::task::spawn_blocking(move || {
            let result = query(&conn);
            idle.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
            result
        })
        .await
        .map_err(|e| IndexError::Sqlite(format!("query task failed: {e}")))?
    }
}
