//! The single index writer.
//!
//! SQLite allows one writer at a time, so all writes go through one thread. Commands that
//! queue up while a transaction commits are applied together in the next transaction (group
//! commit): under concurrent STOW-RS, the cost of a commit is shared by many instances. Each
//! command runs in its own savepoint, so one failing instance does not affect the others.

use std::collections::BTreeSet;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fortitude_core::attrs::{self, INDEXED, Kind, Level};
use rusqlite::types::Value;
use rusqlite::{Connection, OptionalExtension, params};
use tokio::sync::{mpsc, oneshot};

use super::IndexError;
use super::schema;
use crate::ingest::InstanceRecord;
use crate::retrieve::Scope;

/// Maximum number of commands applied in one transaction.
const MAX_BATCH: usize = 512;
/// Inserts between refreshes of the query planner statistics.
const ANALYZE_EVERY: usize = 10_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InsertOutcome {
    Inserted,
    /// An instance with the same SOP Instance UID is already stored.
    Duplicate,
    /// The series is already stored under another study.
    SeriesConflict,
}

pub(crate) enum Command {
    Insert(
        Box<InstanceRecord>,
        oneshot::Sender<Result<InsertOutcome, IndexError>>,
    ),
    /// Delete the instances of a scope; replies with the paths of their files.
    Delete(Scope, oneshot::Sender<Result<Vec<String>, IndexError>>),
}

enum Applied {
    Insert(rusqlite::Result<InsertOutcome>),
    Delete(rusqlite::Result<Vec<String>>),
}

pub(crate) fn spawn(conn: Connection) -> std::io::Result<mpsc::Sender<Command>> {
    let (tx, rx) = mpsc::channel(MAX_BATCH * 4);
    std::thread::Builder::new()
        .name("index-writer".into())
        .spawn(move || run(conn, rx))?;
    Ok(tx)
}

fn run(mut conn: Connection, mut rx: mpsc::Receiver<Command>) {
    let statements = Statements::new();
    let mut batch = Vec::with_capacity(MAX_BATCH);
    // Without statistics, SQLite cannot tell a selective index from one that is not.
    optimize(&conn);
    let mut since_analyze = 0;
    while let Some(first) = rx.blocking_recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(command) => batch.push(command),
                Err(_) => break,
            }
        }
        since_analyze += batch.len();
        apply(&mut conn, &statements, &mut batch);
        if since_analyze >= ANALYZE_EVERY {
            optimize(&conn);
            since_analyze = 0;
        }
    }
}

fn optimize(conn: &Connection) {
    // 0x10002: analyze every table whose statistics may be stale, without size limit.
    if let Err(e) = conn.execute_batch("PRAGMA optimize = 0x10002;") {
        tracing::warn!(error = %e, "cannot refresh index statistics");
    }
}

fn apply(conn: &mut Connection, statements: &Statements, batch: &mut Vec<Command>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let started = Instant::now();
    let mut results = Vec::with_capacity(batch.len());
    let committed = (|| -> rusqlite::Result<()> {
        let mut tx = conn.transaction()?;
        for command in batch.iter() {
            let sp = tx.savepoint()?;
            let (applied, keep) = match command {
                Command::Insert(record, _) => {
                    let result = insert(&sp, statements, record, now);
                    let keep = matches!(result, Ok(InsertOutcome::Inserted));
                    (Applied::Insert(result), keep)
                }
                Command::Delete(scope, _) => {
                    let result = delete(&sp, scope);
                    let keep = result.as_ref().is_ok_and(|paths| !paths.is_empty());
                    (Applied::Delete(result), keep)
                }
            };
            if keep {
                sp.commit()?;
            }
            results.push(applied);
        }
        tx.commit()
    })();
    metrics::histogram!("fortitude_index_batch_size").record(batch.len() as f64);
    metrics::histogram!("fortitude_index_commit_seconds").record(started.elapsed().as_secs_f64());

    let mut results = results.into_iter();
    for command in batch.drain(..) {
        let applied = results.next();
        let failure = || match &committed {
            Err(e) => IndexError::Sqlite(e.to_string()),
            Ok(()) => IndexError::Sqlite("transaction aborted".into()),
        };
        match (command, applied) {
            (Command::Insert(_, reply), Some(Applied::Insert(result))) if committed.is_ok() => {
                let _ = reply.send(result.map_err(IndexError::from));
            }
            (Command::Delete(_, reply), Some(Applied::Delete(result))) if committed.is_ok() => {
                let _ = reply.send(result.map_err(IndexError::from));
            }
            (Command::Insert(_, reply), _) => {
                let _ = reply.send(Err(failure()));
            }
            (Command::Delete(_, reply), _) => {
                let _ = reply.send(Err(failure()));
            }
        }
    }
}

/// Delete the instances of `scope`, then the series and studies left empty, and refresh
/// the counters, representative instances and modalities of the others.
fn delete(conn: &Connection, scope: &Scope) -> rusqlite::Result<Vec<String>> {
    let (sql, params) = scope.select("i.pk, i.path, i.series_pk, i.study_pk");
    let rows: Vec<(i64, String, i64, i64)> = conn
        .prepare(&sql)?
        .query_map(rusqlite::params_from_iter(params), |r| {
            Ok((r.get(0)?, r.get(1)?, r.get(2)?, r.get(3)?))
        })?
        .collect::<rusqlite::Result<_>>()?;
    let mut series = BTreeSet::new();
    let mut studies = BTreeSet::new();
    for (pk, _, series_pk, study_pk) in &rows {
        conn.prepare_cached("DELETE FROM instances WHERE pk = ?")?
            .execute([pk])?;
        series.insert(*series_pk);
        studies.insert(*study_pk);
    }
    for pk in series {
        conn.prepare_cached(
            "DELETE FROM series WHERE pk = ?1 AND NOT EXISTS (SELECT 1 FROM instances WHERE series_pk = ?1)",
        )?
        .execute([pk])?;
        conn.prepare_cached(
            "UPDATE series SET
                num_instances = (SELECT count(*) FROM instances WHERE series_pk = ?1),
                rep_instance = (SELECT min(pk) FROM instances WHERE series_pk = ?1)
             WHERE pk = ?1",
        )?
        .execute([pk])?;
    }
    for pk in studies {
        conn.prepare_cached(
            "DELETE FROM studies WHERE pk = ?1 AND NOT EXISTS (SELECT 1 FROM series WHERE study_pk = ?1)",
        )?
        .execute([pk])?;
        conn.prepare_cached(
            "UPDATE studies SET
                num_series = (SELECT count(*) FROM series WHERE study_pk = ?1),
                num_instances = (SELECT count(*) FROM instances WHERE study_pk = ?1),
                rep_instance = (SELECT min(pk) FROM instances WHERE study_pk = ?1),
                modalities = coalesce((SELECT group_concat(m, '\\') FROM (
                    SELECT DISTINCT modality AS m FROM series
                    WHERE study_pk = ?1 AND coalesce(modality, '') != ''
                )), '')
             WHERE pk = ?1",
        )?
        .execute([pk])?;
    }
    Ok(rows.into_iter().map(|(_, path, _, _)| path).collect())
}

/// SQL statements built once from the indexed attributes.
struct Statements {
    insert_study: String,
    insert_series: String,
    insert_instance: String,
}

impl Statements {
    fn new() -> Self {
        let insert = |level: Level, fixed: &[&str]| {
            let columns: Vec<String> = fixed
                .iter()
                .map(|c| (*c).to_owned())
                .chain(schema::columns(level))
                .collect();
            let placeholders = vec!["?"; columns.len()].join(", ");
            format!(
                "INSERT INTO {} ({}) VALUES ({placeholders})",
                level.table(),
                columns.join(", ")
            )
        };
        Self {
            insert_study: insert(Level::Study, &["attrs", "created_at"]),
            insert_series: insert(Level::Series, &["study_pk", "attrs", "created_at"]),
            insert_instance: insert(
                Level::Instance,
                &[
                    "series_pk",
                    "study_pk",
                    "transfer_syntax",
                    "frames",
                    "path",
                    "size",
                    "pixel",
                    "attrs",
                    "metadata",
                    "created_at",
                ],
            ),
        }
    }
}

/// Values of the indexed columns of `level`, matching [`schema::columns`].
fn indexed_values(record: &InstanceRecord, level: Level) -> Vec<Value> {
    let mut values = Vec::new();
    for (attribute, value) in INDEXED.iter().zip(&record.values) {
        if attribute.level != level {
            continue;
        }
        let text = || value.clone().map_or(Value::Null, Value::Text);
        match attribute.kind {
            Kind::Integer => values.push(
                value
                    .as_deref()
                    .and_then(|v| v.trim().parse::<i64>().ok())
                    .map_or(Value::Null, Value::Integer),
            ),
            Kind::PersonName => {
                values.push(text());
                values.push(
                    value
                        .as_deref()
                        .map_or(Value::Null, |v| Value::Text(person_name_key(v))),
                );
            }
            _ => values.push(text()),
        }
    }
    values
}

/// Normalized form of a person name for case-insensitive matching.
pub fn person_name_key(name: &str) -> String {
    name.trim().to_uppercase()
}

fn insert(
    conn: &Connection,
    statements: &Statements,
    record: &InstanceRecord,
    now: i64,
) -> rusqlite::Result<InsertOutcome> {
    let exists = conn
        .prepare_cached("SELECT 1 FROM instances WHERE sop_uid = ?")?
        .query_row([&record.sop_uid], |_| Ok(()))
        .optional()?;
    if exists.is_some() {
        return Ok(InsertOutcome::Duplicate);
    }

    let study_pk: i64 = match conn
        .prepare_cached("SELECT pk FROM studies WHERE study_uid = ?")?
        .query_row([&record.study_uid], |r| r.get(0))
        .optional()?
    {
        Some(pk) => pk,
        None => {
            let mut values = vec![
                Value::Blob(record.study_attrs.clone().into_bytes()),
                Value::Integer(now),
            ];
            values.extend(indexed_values(record, Level::Study));
            conn.prepare_cached(&statements.insert_study)?
                .execute(rusqlite::params_from_iter(values))?;
            conn.last_insert_rowid()
        }
    };

    let existing_series: Option<(i64, i64)> = conn
        .prepare_cached("SELECT pk, study_pk FROM series WHERE series_uid = ?")?
        .query_row([&record.series_uid], |r| Ok((r.get(0)?, r.get(1)?)))
        .optional()?;
    let series_pk = match existing_series {
        Some((_, owner)) if owner != study_pk => return Ok(InsertOutcome::SeriesConflict),
        Some((pk, _)) => pk,
        None => {
            let mut values = vec![
                Value::Integer(study_pk),
                Value::Blob(record.series_attrs.clone().into_bytes()),
                Value::Integer(now),
            ];
            values.extend(indexed_values(record, Level::Series));
            conn.prepare_cached(&statements.insert_series)?
                .execute(rusqlite::params_from_iter(values))?;
            let pk = conn.last_insert_rowid();
            let modality = attrs::find_indexed(dicom_dictionary_std::tags::MODALITY)
                .and_then(|a| record.value(a))
                .unwrap_or_default();
            conn.prepare_cached(
                "UPDATE studies SET num_series = num_series + 1,
                    modalities = CASE
                        WHEN ?2 = '' OR instr('\\' || modalities || '\\', '\\' || ?2 || '\\') > 0 THEN modalities
                        WHEN modalities = '' THEN ?2
                        ELSE modalities || '\\' || ?2 END
                 WHERE pk = ?1",
            )?
            .execute(params![study_pk, modality])?;
            pk
        }
    };

    let mut values = vec![
        Value::Integer(series_pk),
        Value::Integer(study_pk),
        Value::Text(record.transfer_syntax.clone()),
        Value::Integer(i64::from(record.frames)),
        Value::Text(record.path.clone()),
        Value::Integer(record.size as i64),
        record
            .pixel
            .as_ref()
            .map_or(Value::Null, |p| Value::Blob(p.encode())),
        Value::Blob(record.instance_attrs.clone().into_bytes()),
        Value::Blob(record.metadata.clone().into_bytes()),
        Value::Integer(now),
    ];
    values.extend(indexed_values(record, Level::Instance));
    conn.prepare_cached(&statements.insert_instance)?
        .execute(rusqlite::params_from_iter(values))?;
    let instance_pk = conn.last_insert_rowid();

    conn.prepare_cached(
        "UPDATE series SET num_instances = num_instances + 1, rep_instance = coalesce(rep_instance, ?2) WHERE pk = ?1",
    )?
    .execute(params![series_pk, instance_pk])?;
    conn.prepare_cached(
        "UPDATE studies SET num_instances = num_instances + 1, rep_instance = coalesce(rep_instance, ?2) WHERE pk = ?1",
    )?
    .execute(params![study_pk, instance_pk])?;
    Ok(InsertOutcome::Inserted)
}
