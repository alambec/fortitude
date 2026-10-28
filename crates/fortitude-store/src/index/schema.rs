//! SQLite schema, generated from [`fortitude_core::attrs::INDEXED`].
//!
//! One flat table per level. Every indexed attribute is a column with its own index; person
//! names also get an upper-cased `<column>_key` column for case-insensitive matching.
//! `attrs` holds the pre-encoded QIDO-RS attributes of the level and `rep_instance` the first
//! stored instance, used to answer for attributes that are not indexed.

use fortitude_core::attrs::{self, Kind, Level};
use rusqlite::Connection;

const VERSION: i64 = 1;

pub fn configure(conn: &Connection) -> rusqlite::Result<()> {
    conn.execute_batch(
        "PRAGMA journal_mode = WAL;
         PRAGMA synchronous = NORMAL;
         PRAGMA temp_store = MEMORY;
         PRAGMA cache_size = -65536;
         PRAGMA mmap_size = 268435456;
         PRAGMA busy_timeout = 5000;",
    )
}

pub fn migrate(conn: &Connection) -> rusqlite::Result<()> {
    let version: i64 = conn.query_row("PRAGMA user_version", [], |r| r.get(0))?;
    if version == VERSION {
        return Ok(());
    }
    if version != 0 {
        return Err(rusqlite::Error::InvalidParameterName(format!(
            "unsupported index schema version {version}"
        )));
    }
    conn.execute_batch(&format!(
        "BEGIN; {} PRAGMA user_version = {VERSION}; COMMIT;",
        ddl()
    ))
}

/// Columns of indexed attributes of `level`, in [`attrs::INDEXED`] order, person name keys
/// included right after their column.
pub fn columns(level: Level) -> Vec<String> {
    attrs::indexed(level)
        .flat_map(|a| {
            let key = (a.kind == Kind::PersonName).then(|| format!("{}_key", a.column));
            std::iter::once(a.column.to_owned()).chain(key)
        })
        .collect()
}

fn ddl() -> String {
    let column_defs = |level: Level| -> String {
        attrs::indexed(level)
            .map(|a| match a.kind {
                Kind::Uid if a.level == level && is_own_uid(a.column) => {
                    format!("{} TEXT NOT NULL UNIQUE,", a.column)
                }
                Kind::Integer => format!("{} INTEGER,", a.column),
                Kind::PersonName => format!("{0} TEXT, {0}_key TEXT,", a.column),
                _ => format!("{} TEXT,", a.column),
            })
            .collect()
    };
    let indexes = |level: Level| -> String {
        attrs::indexed(level)
            .filter(|a| !is_own_uid(a.column))
            .map(|a| {
                let column = match a.kind {
                    Kind::PersonName => format!("{}_key", a.column),
                    _ => a.column.to_owned(),
                };
                format!("CREATE INDEX {0}_{column} ON {0}({column});", level.table())
            })
            .collect()
    };
    format!(
        "CREATE TABLE studies (
            pk INTEGER PRIMARY KEY,
            {study}
            modalities TEXT NOT NULL DEFAULT '',
            num_series INTEGER NOT NULL DEFAULT 0,
            num_instances INTEGER NOT NULL DEFAULT 0,
            attrs BLOB NOT NULL,
            rep_instance INTEGER,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE series (
            pk INTEGER PRIMARY KEY,
            study_pk INTEGER NOT NULL,
            {series}
            num_instances INTEGER NOT NULL DEFAULT 0,
            attrs BLOB NOT NULL,
            rep_instance INTEGER,
            created_at INTEGER NOT NULL
        );
        CREATE TABLE instances (
            pk INTEGER PRIMARY KEY,
            series_pk INTEGER NOT NULL,
            study_pk INTEGER NOT NULL,
            {instance}
            transfer_syntax TEXT NOT NULL,
            frames INTEGER NOT NULL,
            path TEXT NOT NULL,
            size INTEGER NOT NULL,
            pixel BLOB,
            attrs BLOB NOT NULL,
            metadata BLOB NOT NULL,
            created_at INTEGER NOT NULL
        );
        CREATE INDEX series_study ON series(study_pk, series_number);
        CREATE INDEX instances_series ON instances(series_pk, instance_number);
        CREATE INDEX instances_study ON instances(study_pk);
        {study_indexes}{series_indexes}{instance_indexes}",
        study = column_defs(Level::Study),
        series = column_defs(Level::Series),
        instance = column_defs(Level::Instance),
        study_indexes = indexes(Level::Study),
        series_indexes = indexes(Level::Series),
        instance_indexes = indexes(Level::Instance),
    )
}

/// The UID identifying rows of a table (`study_uid`, `series_uid`, `sop_uid`).
fn is_own_uid(column: &str) -> bool {
    matches!(column, "study_uid" | "series_uid" | "sop_uid")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn schema_applies_and_is_idempotent() {
        let conn = Connection::open_in_memory().unwrap();
        migrate(&conn).unwrap();
        migrate(&conn).unwrap();
        let count: i64 = conn
            .query_row(
                "SELECT count(*) FROM sqlite_master WHERE type = 'index'",
                [],
                |r| r.get(0),
            )
            .unwrap();
        assert!(count > 20, "{count} indexes");
        assert_eq!(columns(Level::Study)[0], "study_uid");
        assert!(columns(Level::Study).contains(&"patient_name_key".to_owned()));
    }
}
