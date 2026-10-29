//! Pool of read connections. Queries run on the blocking thread pool, at most one per
//! connection; with SQLite, WAL mode lets them proceed while the writer commits.

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

use super::IndexError;
use super::db::{Connection, Dialect};

type Opener = dyn Fn() -> Result<Connection, IndexError> + Send + Sync;

pub struct Readers {
    idle: Arc<Mutex<Vec<Connection>>>,
    permits: Semaphore,
    open: Arc<Opener>,
    dialect: Dialect,
}

impl Readers {
    pub fn new(
        size: usize,
        open: impl Fn() -> Result<Connection, IndexError> + Send + Sync + 'static,
    ) -> Result<Self, IndexError> {
        let idle = (0..size).map(|_| open()).collect::<Result<Vec<_>, _>>()?;
        let dialect = idle.first().map_or(Dialect::Sqlite, Connection::dialect);
        Ok(Self {
            idle: Arc::new(Mutex::new(idle)),
            permits: Semaphore::new(size),
            open: Arc::new(open),
            dialect,
        })
    }

    pub fn dialect(&self) -> Dialect {
        self.dialect
    }

    /// Run `query` on a read connection.
    pub async fn run<R, F>(&self, query: F) -> Result<R, IndexError>
    where
        F: FnOnce(&mut Connection) -> Result<R, IndexError> + Send + 'static,
        R: Send + 'static,
    {
        let _permit = self
            .permits
            .acquire()
            .await
            .map_err(|_| IndexError::WriterStopped)?;
        let popped = self.idle.lock().unwrap_or_else(|e| e.into_inner()).pop();
        let idle = self.idle.clone();
        let open = self.open.clone();
        tokio::task::spawn_blocking(move || {
            // A connection is missing if a query panicked while holding it, or if it was
            // closed (e.g. the database server restarted).
            let mut conn = match popped {
                Some(conn) => conn,
                None => open()?,
            };
            let result = query(&mut conn);
            if conn.is_usable() {
                idle.lock().unwrap_or_else(|e| e.into_inner()).push(conn);
            }
            result
        })
        .await
        .map_err(|e| IndexError::Database(format!("query task failed: {e}")))?
    }
}
