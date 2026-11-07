//! The single index writer.
//!
//! SQLite allows one writer at a time, so all writes go through one thread (PostgreSQL could
//! take more, but one keeps both backends alike and conflict-free). Commands that
//! queue up while a transaction commits are applied together in the next transaction (group
//! commit): under concurrent STOW-RS, the cost of a commit is shared by many instances. Each
//! command runs in its own savepoint, so one failing instance does not affect the others.

use std::collections::BTreeSet;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use fortitude_core::attrs::{self, INDEXED, Kind, Level};
use tokio::sync::{mpsc, oneshot};

use super::IndexError;
use super::db::{Connection, Dialect, Value};
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
    /// Delete the instances of a scope; replies with the paths of their files. With a time
    /// (milliseconds since the Unix epoch), only if no series of the study was stored since
    /// (retention).
    Delete(
        Scope,
        Option<i64>,
        oneshot::Sender<Result<Vec<String>, IndexError>>,
    ),
    /// Any other write (e.g. the registered modalities); replies with the number of rows it
    /// changed.
    Write(Box<WriteFn>, oneshot::Sender<Result<u64, IndexError>>),
}

pub(crate) type WriteFn = dyn FnOnce(&mut Connection) -> Result<u64, IndexError> + Send;

enum Applied {
    Insert(Result<InsertOutcome, IndexError>),
    Delete(Result<Vec<String>, IndexError>),
    Write(Result<u64, IndexError>),
}

type Opener = dyn Fn() -> Result<Connection, IndexError> + Send;

pub(crate) fn spawn(
    conn: Connection,
    reopen: impl Fn() -> Result<Connection, IndexError> + Send + 'static,
) -> std::io::Result<mpsc::Sender<Command>> {
    let (tx, rx) = mpsc::channel(MAX_BATCH * 4);
    std::thread::Builder::new()
        .name("index-writer".into())
        .spawn(move || run(conn, Box::new(reopen), rx))?;
    Ok(tx)
}

fn run(mut conn: Connection, reopen: Box<Opener>, mut rx: mpsc::Receiver<Command>) {
    let statements = Statements::new(conn.dialect());
    let mut batch = Vec::with_capacity(MAX_BATCH);
    // Without statistics, SQLite cannot tell a selective index from one that is not.
    optimize(&mut conn);
    let mut since_analyze = 0;
    while let Some(first) = rx.blocking_recv() {
        batch.push(first);
        while batch.len() < MAX_BATCH {
            match rx.try_recv() {
                Ok(command) => batch.push(command),
                Err(_) => break,
            }
        }
        if !conn.is_usable() {
            match reopen() {
                Ok(new) => conn = new,
                Err(e) => {
                    tracing::error!(error = %e, "cannot reconnect to the index");
                    fail(&mut batch, &e);
                    continue;
                }
            }
        }
        since_analyze += batch.len();
        apply(&mut conn, &statements, &mut batch);
        if since_analyze >= ANALYZE_EVERY {
            optimize(&mut conn);
            since_analyze = 0;
        }
    }
}

fn optimize(conn: &mut Connection) {
    let sql = match conn.dialect() {
        // 0x10002: analyze every table whose statistics may be stale, without size limit.
        Dialect::Sqlite => "PRAGMA optimize = 0x10002;",
        Dialect::Postgres => "ANALYZE studies, series, instances;",
    };
    if let Err(e) = conn.batch(sql) {
        tracing::warn!(error = %e, "cannot refresh index statistics");
    }
}

/// Reply to every command of `batch` with `error`.
fn fail(batch: &mut Vec<Command>, error: &IndexError) {
    for command in batch.drain(..) {
        match command {
            Command::Insert(_, reply) => {
                let _ = reply.send(Err(error.clone()));
            }
            Command::Delete(_, _, reply) => {
                let _ = reply.send(Err(error.clone()));
            }
            Command::Write(_, reply) => {
                let _ = reply.send(Err(error.clone()));
            }
        }
    }
}

fn apply(conn: &mut Connection, statements: &Statements, batch: &mut Vec<Command>) {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as i64);
    let started = Instant::now();
    let mut results = Vec::with_capacity(batch.len());
    let committed = (|| -> Result<(), IndexError> {
        conn.batch("BEGIN")?;
        for command in batch.iter_mut() {
            conn.batch("SAVEPOINT command")?;
            let (applied, keep) = match command {
                Command::Insert(record, _) => {
                    let result = insert(conn, statements, record, now);
                    let keep = matches!(result, Ok(InsertOutcome::Inserted));
                    (Applied::Insert(result), keep)
                }
                Command::Delete(scope, unchanged_since, _) => {
                    let result = match unchanged_since {
                        Some(since) => match has_series_since(conn, &scope.study, *since) {
                            Ok(true) => Ok(Vec::new()),
                            Ok(false) => delete(conn, statements.dialect, scope),
                            Err(e) => Err(e),
                        },
                        None => delete(conn, statements.dialect, scope),
                    };
                    let keep = result.as_ref().is_ok_and(|paths| !paths.is_empty());
                    (Applied::Delete(result), keep)
                }
                Command::Write(write, _) => {
                    // Taken out of the command, which only keeps its reply.
                    let write = std::mem::replace(write, Box::new(|_| Ok(0)));
                    let result = write(conn);
                    let keep = result.is_ok();
                    (Applied::Write(result), keep)
                }
            };
            conn.batch(if keep {
                "RELEASE SAVEPOINT command"
            } else {
                "ROLLBACK TO SAVEPOINT command; RELEASE SAVEPOINT command"
            })?;
            results.push(applied);
        }
        conn.batch("COMMIT")
    })();
    if committed.is_err() {
        let _ = conn.batch("ROLLBACK");
    }
    metrics::histogram!("fortitude_index_batch_size").record(batch.len() as f64);
    metrics::histogram!("fortitude_index_commit_seconds").record(started.elapsed().as_secs_f64());

    let mut results = results.into_iter();
    for command in batch.drain(..) {
        let applied = results.next();
        let failure = || match &committed {
            Err(e) => e.clone(),
            Ok(()) => IndexError::Database("transaction aborted".into()),
        };
        match (command, applied) {
            (Command::Insert(_, reply), Some(Applied::Insert(result))) if committed.is_ok() => {
                let _ = reply.send(result);
            }
            (Command::Delete(_, _, reply), Some(Applied::Delete(result))) if committed.is_ok() => {
                let _ = reply.send(result);
            }
            (Command::Write(_, reply), Some(Applied::Write(result))) if committed.is_ok() => {
                let _ = reply.send(result);
            }
            (Command::Insert(_, reply), _) => {
                let _ = reply.send(Err(failure()));
            }
            (Command::Delete(_, _, reply), _) => {
                let _ = reply.send(Err(failure()));
            }
            (Command::Write(_, reply), _) => {
                let _ = reply.send(Err(failure()));
            }
        }
    }
}

/// Whether a series of the study was stored at or after `since` (milliseconds).
fn has_series_since(conn: &mut Connection, study: &str, since: i64) -> Result<bool, IndexError> {
    let found = conn.query_row(
        "SELECT 1 FROM series s JOIN studies st ON st.pk = s.study_pk
         WHERE st.study_uid = ? AND s.created_at >= ?",
        &[Value::Text(study.to_owned()), Value::Integer(since)],
        |_| Ok(()),
    )?;
    Ok(found.is_some())
}

/// Delete the instances of `scope`, then the series and studies left empty, and refresh
/// the counters, representative instances and modalities of the others.
fn delete(
    conn: &mut Connection,
    dialect: Dialect,
    scope: &Scope,
) -> Result<Vec<String>, IndexError> {
    let (sql, params) = scope.select("i.pk, i.path, i.series_pk, i.study_pk");
    let rows: Vec<(i64, String, i64, i64)> = conn.query(&sql, &params, |r| {
        Ok((r.i64(0)?, r.str(1)?.to_owned(), r.i64(2)?, r.i64(3)?))
    })?;
    let mut series = BTreeSet::new();
    let mut studies = BTreeSet::new();
    for (pk, _, series_pk, study_pk) in &rows {
        conn.execute("DELETE FROM instances WHERE pk = ?", &[Value::Integer(*pk)])?;
        series.insert(*series_pk);
        studies.insert(*study_pk);
    }
    for pk in series {
        let pk = [Value::Integer(pk)];
        conn.execute(
            "DELETE FROM series WHERE pk = ?1 AND NOT EXISTS (SELECT 1 FROM instances WHERE series_pk = ?1)",
            &pk,
        )?;
        conn.execute(
            "UPDATE series SET
                num_instances = (SELECT count(*) FROM instances WHERE series_pk = ?1),
                rep_instance = (SELECT min(pk) FROM instances WHERE series_pk = ?1)
             WHERE pk = ?1",
            &pk,
        )?;
    }
    for pk in studies {
        let pk = [Value::Integer(pk)];
        conn.execute(
            "DELETE FROM studies WHERE pk = ?1 AND NOT EXISTS (SELECT 1 FROM series WHERE study_pk = ?1)",
            &pk,
        )?;
        conn.execute(
            &format!(
                "UPDATE studies SET
                    num_series = (SELECT count(*) FROM series WHERE study_pk = ?1),
                    num_instances = (SELECT count(*) FROM instances WHERE study_pk = ?1),
                    rep_instance = (SELECT min(pk) FROM instances WHERE study_pk = ?1),
                    modalities = coalesce((SELECT {} FROM (
                        SELECT DISTINCT modality AS m FROM series
                        WHERE study_pk = ?1 AND coalesce(modality, '') != ''
                    ) AS d), '')
                 WHERE pk = ?1",
                dialect.concat("m", "'\\'")
            ),
            &pk,
        )?;
    }
    Ok(rows.into_iter().map(|(_, path, _, _)| path).collect())
}

/// SQL statements built once from the indexed attributes.
struct Statements {
    dialect: Dialect,
    insert_study: String,
    insert_series: String,
    insert_instance: String,
    add_series: String,
}

impl Statements {
    fn new(dialect: Dialect) -> Self {
        let insert = |level: Level, fixed: &[&str]| {
            let columns: Vec<String> = fixed
                .iter()
                .map(|c| (*c).to_owned())
                .chain(schema::columns(level))
                .collect();
            let placeholders = vec!["?"; columns.len()].join(", ");
            format!(
                "INSERT INTO {} ({}) VALUES ({placeholders}) RETURNING pk",
                level.table(),
                columns.join(", ")
            )
        };
        let has_modality = dialect.contains("'\\' || modalities || '\\'", "'\\' || ?2 || '\\'");
        Self {
            dialect,
            add_series: format!(
                "UPDATE studies SET num_series = num_series + 1,
                    modalities = CASE
                        WHEN ?2 = '' OR {has_modality} THEN modalities
                        WHEN modalities = '' THEN ?2
                        ELSE modalities || '\\' || ?2 END
                 WHERE pk = ?1"
            ),
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
                    "crc32",
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
    conn: &mut Connection,
    statements: &Statements,
    record: &InstanceRecord,
    now: i64,
) -> Result<InsertOutcome, IndexError> {
    let pk = |r: &super::Row<'_>| r.i64(0);
    let exists = conn.query_row(
        "SELECT 1 FROM instances WHERE sop_uid = ?",
        &[Value::Text(record.sop_uid.clone())],
        |_| Ok(()),
    )?;
    if exists.is_some() {
        return Ok(InsertOutcome::Duplicate);
    }

    let existing_study = conn.query_row(
        "SELECT pk FROM studies WHERE study_uid = ?",
        &[Value::Text(record.study_uid.clone())],
        pk,
    )?;
    let study_pk = match existing_study {
        Some(pk) => pk,
        None => {
            let mut values = vec![
                Value::Blob(record.study_attrs.clone().into_bytes()),
                Value::Integer(now),
            ];
            values.extend(indexed_values(record, Level::Study));
            inserted(conn.query_row(&statements.insert_study, &values, pk)?)?
        }
    };

    let existing_series = conn.query_row(
        "SELECT pk, study_pk FROM series WHERE series_uid = ?",
        &[Value::Text(record.series_uid.clone())],
        |r| Ok((r.i64(0)?, r.i64(1)?)),
    )?;
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
            let series_pk = inserted(conn.query_row(&statements.insert_series, &values, pk)?)?;
            let modality = attrs::find_indexed(dicom_dictionary_std::tags::MODALITY)
                .and_then(|a| record.value(a))
                .unwrap_or_default();
            conn.execute(
                &statements.add_series,
                &[Value::Integer(study_pk), Value::Text(modality.to_owned())],
            )?;
            series_pk
        }
    };

    let mut values = vec![
        Value::Integer(series_pk),
        Value::Integer(study_pk),
        Value::Text(record.transfer_syntax.clone()),
        Value::Integer(i64::from(record.frames)),
        Value::Text(record.path.clone()),
        Value::Integer(record.size as i64),
        Value::Integer(i64::from(record.crc32)),
        record
            .pixel
            .as_ref()
            .map_or(Value::Null, |p| Value::Blob(p.encode())),
        Value::Blob(record.instance_attrs.clone().into_bytes()),
        Value::Blob(record.metadata.clone().into_bytes()),
        Value::Integer(now),
    ];
    values.extend(indexed_values(record, Level::Instance));
    let instance_pk = inserted(conn.query_row(&statements.insert_instance, &values, pk)?)?;

    let pks = [Value::Integer(series_pk), Value::Integer(instance_pk)];
    conn.execute(
        "UPDATE series SET num_instances = num_instances + 1, rep_instance = coalesce(rep_instance, ?2) WHERE pk = ?1",
        &pks,
    )?;
    let pks = [Value::Integer(study_pk), Value::Integer(instance_pk)];
    conn.execute(
        "UPDATE studies SET num_instances = num_instances + 1, rep_instance = coalesce(rep_instance, ?2) WHERE pk = ?1",
        &pks,
    )?;
    Ok(InsertOutcome::Inserted)
}

/// Key of a row returned by `INSERT … RETURNING pk`.
fn inserted(pk: Option<i64>) -> Result<i64, IndexError> {
    pk.ok_or_else(|| IndexError::Database("insert returned no key".into()))
}
