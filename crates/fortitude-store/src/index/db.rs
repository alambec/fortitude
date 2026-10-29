//! Database connections: SQLite or PostgreSQL behind one small interface.
//!
//! Statements are written once, with `?` or `?N` placeholders, in the SQL both databases
//! understand; the few constructs that differ come from [`Dialect`]. PostgreSQL statements
//! get `$N` placeholders and are prepared once per connection.

use std::collections::HashMap;
use std::path::Path;

use bytes::BytesMut;
use postgres::fallible_iterator::FallibleIterator;
use postgres::types::{IsNull, ToSql, Type, to_sql_checked};
use rusqlite::OpenFlags;
use rusqlite::types::{ToSqlOutput, ValueRef};

use super::IndexError;

/// Prepared statements kept per PostgreSQL connection; searches produce many shapes.
const MAX_STATEMENTS: usize = 512;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Dialect {
    Sqlite,
    Postgres,
}

impl Dialect {
    /// Case-sensitive operator matching a column against a pattern from [`Dialect::pattern`].
    pub fn pattern_operator(self) -> &'static str {
        match self {
            Dialect::Sqlite => "GLOB",
            Dialect::Postgres => "LIKE",
        }
    }

    /// A DICOM wildcard pattern (`*` and `?`) for [`Dialect::pattern_operator`].
    pub fn pattern(self, wildcard: &str) -> String {
        let mut out = String::with_capacity(wildcard.len());
        for c in wildcard.chars() {
            match (self, c) {
                (Dialect::Sqlite, '[') => out.push_str("[[]"),
                (Dialect::Sqlite, ']') => out.push_str("[]]"),
                (Dialect::Postgres, '*') => out.push('%'),
                (Dialect::Postgres, '?') => out.push('_'),
                (Dialect::Postgres, '%' | '_' | '\\') => {
                    out.push('\\');
                    out.push(c);
                }
                (_, c) => out.push(c),
            }
        }
        out
    }

    /// SQL expression: whether `needle` occurs in `haystack`.
    pub fn contains(self, haystack: &str, needle: &str) -> String {
        match self {
            Dialect::Sqlite => format!("instr({haystack}, {needle}) > 0"),
            Dialect::Postgres => format!("strpos({haystack}, {needle}) > 0"),
        }
    }

    /// Aggregate concatenating the values of `expression` with `separator` (a SQL string).
    pub fn concat(self, expression: &str, separator: &str) -> String {
        match self {
            Dialect::Sqlite => format!("group_concat({expression}, {separator})"),
            Dialect::Postgres => format!("string_agg({expression}, {separator})"),
        }
    }
}

/// A statement parameter.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Value {
    Null,
    Integer(i64),
    Text(String),
    Blob(Vec<u8>),
}

impl rusqlite::ToSql for Value {
    fn to_sql(&self) -> rusqlite::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self {
            Value::Null => ValueRef::Null,
            Value::Integer(v) => ValueRef::Integer(*v),
            Value::Text(v) => ValueRef::Text(v.as_bytes()),
            Value::Blob(v) => ValueRef::Blob(v),
        }))
    }
}

impl ToSql for Value {
    fn to_sql(
        &self,
        ty: &Type,
        out: &mut BytesMut,
    ) -> Result<IsNull, Box<dyn std::error::Error + Sync + Send>> {
        match self {
            Value::Null => Ok(IsNull::Yes),
            Value::Integer(v) if *ty == Type::INT4 => i32::try_from(*v)?.to_sql(ty, out),
            Value::Integer(v) if *ty == Type::INT8 => v.to_sql(ty, out),
            Value::Text(v) if <&str as ToSql>::accepts(ty) => v.to_sql(ty, out),
            Value::Blob(v) if *ty == Type::BYTEA => v.to_sql(ty, out),
            _ => Err(format!("cannot bind {self:?} as {ty}").into()),
        }
    }

    fn accepts(_: &Type) -> bool {
        true
    }

    to_sql_checked!();
}

pub enum Connection {
    Sqlite(rusqlite::Connection),
    Postgres(Box<Postgres>),
}

pub struct Postgres {
    // Only `None` while dropped.
    client: Option<postgres::Client>,
    statements: HashMap<String, postgres::Statement>,
}

impl Drop for Postgres {
    fn drop(&mut self) {
        // Closing blocks on the client's runtime: not possible within an asynchronous
        // context (e.g. when a store is dropped at the end of `main`).
        if let Some(client) = self.client.take()
            && tokio::runtime::Handle::try_current().is_ok()
        {
            std::thread::spawn(move || drop(client));
        }
    }
}

impl Postgres {
    pub fn connect(url: &str) -> Result<Self, IndexError> {
        Ok(Self {
            client: Some(
                url.parse::<postgres::Config>()?
                    .application_name("fortitude")
                    .connect(postgres::NoTls)?,
            ),
            statements: HashMap::new(),
        })
    }

    fn client(&mut self) -> &mut postgres::Client {
        self.client.as_mut().expect("client taken before drop")
    }

    fn statement(&mut self, sql: &str) -> Result<postgres::Statement, IndexError> {
        if let Some(statement) = self.statements.get(sql) {
            return Ok(statement.clone());
        }
        if self.statements.len() >= MAX_STATEMENTS {
            self.statements.clear();
        }
        let statement = self.client().prepare(&numbered(sql))?;
        self.statements.insert(sql.to_owned(), statement.clone());
        Ok(statement)
    }
}

impl Connection {
    /// A read-write SQLite connection.
    pub fn sqlite(path: &Path) -> Result<Self, IndexError> {
        let conn = rusqlite::Connection::open_with_flags(
            path,
            OpenFlags::SQLITE_OPEN_READ_WRITE
                | OpenFlags::SQLITE_OPEN_CREATE
                | OpenFlags::SQLITE_OPEN_NO_MUTEX
                | OpenFlags::SQLITE_OPEN_URI,
        )?;
        super::schema::configure(&conn)?;
        Ok(Connection::Sqlite(conn))
    }

    pub fn postgres(url: &str) -> Result<Self, IndexError> {
        Ok(Connection::Postgres(Box::new(Postgres::connect(url)?)))
    }

    pub fn dialect(&self) -> Dialect {
        match self {
            Connection::Sqlite(_) => Dialect::Sqlite,
            Connection::Postgres(_) => Dialect::Postgres,
        }
    }

    /// Whether the connection can still be used; a PostgreSQL connection closes when the
    /// server goes away.
    pub fn is_usable(&self) -> bool {
        match self {
            Connection::Sqlite(_) => true,
            Connection::Postgres(pg) => pg.client.as_ref().is_some_and(|c| !c.is_closed()),
        }
    }

    /// Run statements without parameters (transaction control, DDL, settings).
    pub fn batch(&mut self, sql: &str) -> Result<(), IndexError> {
        match self {
            Connection::Sqlite(conn) => conn.execute_batch(sql)?,
            Connection::Postgres(pg) => pg.client().batch_execute(sql)?,
        }
        Ok(())
    }

    /// Run a statement; returns the number of rows changed.
    pub fn execute(&mut self, sql: &str, params: &[Value]) -> Result<u64, IndexError> {
        match self {
            Connection::Sqlite(conn) => Ok(conn
                .prepare_cached(sql)?
                .execute(rusqlite::params_from_iter(params))?
                as u64),
            Connection::Postgres(pg) => {
                let statement = pg.statement(sql)?;
                let params: Vec<&(dyn ToSql + Sync)> =
                    params.iter().map(|p| p as &(dyn ToSql + Sync)).collect();
                Ok(pg.client().execute(&statement, &params)?)
            }
        }
    }

    /// Call `f` on each row of a query until it returns `false`.
    pub fn for_each(
        &mut self,
        sql: &str,
        params: &[Value],
        mut f: impl FnMut(&Row<'_>) -> Result<bool, IndexError>,
    ) -> Result<(), IndexError> {
        match self {
            Connection::Sqlite(conn) => {
                let mut statement = conn.prepare_cached(sql)?;
                let mut rows = statement.query(rusqlite::params_from_iter(params))?;
                while let Some(row) = rows.next()? {
                    if !f(&Row::Sqlite(row))? {
                        break;
                    }
                }
            }
            Connection::Postgres(pg) => {
                let statement = pg.statement(sql)?;
                let mut rows = pg.client().query_raw(&statement, params)?;
                while let Some(row) = rows.next()? {
                    if !f(&Row::Postgres(&row))? {
                        break;
                    }
                }
            }
        }
        Ok(())
    }

    pub fn query<T>(
        &mut self,
        sql: &str,
        params: &[Value],
        mut map: impl FnMut(&Row<'_>) -> Result<T, IndexError>,
    ) -> Result<Vec<T>, IndexError> {
        let mut out = Vec::new();
        self.for_each(sql, params, |row| {
            out.push(map(row)?);
            Ok(true)
        })?;
        Ok(out)
    }

    /// The first row of a query, if any.
    pub fn query_row<T>(
        &mut self,
        sql: &str,
        params: &[Value],
        map: impl FnOnce(&Row<'_>) -> Result<T, IndexError>,
    ) -> Result<Option<T>, IndexError> {
        let mut map = Some(map);
        let mut out = None;
        self.for_each(sql, params, |row| {
            out = map.take().map(|map| map(row)).transpose()?;
            Ok(false)
        })?;
        Ok(out)
    }
}

/// A result row. Accessors are lenient on types: integers of any width, and text read as
/// bytes, so that `NULL` placeholders in select lists need no casts.
pub enum Row<'a> {
    Sqlite(&'a rusqlite::Row<'a>),
    Postgres(&'a postgres::Row),
}

impl Row<'_> {
    pub fn opt_i64(&self, index: usize) -> Result<Option<i64>, IndexError> {
        Ok(match self {
            Row::Sqlite(row) => row.get_ref(index)?.as_i64_or_null()?,
            Row::Postgres(row) => match *row.columns()[index].type_() {
                Type::INT2 => row.try_get::<_, Option<i16>>(index)?.map(i64::from),
                Type::INT4 => row.try_get::<_, Option<i32>>(index)?.map(i64::from),
                _ => row.try_get(index)?,
            },
        })
    }

    pub fn i64(&self, index: usize) -> Result<i64, IndexError> {
        self.opt_i64(index)?.ok_or_else(|| null(index))
    }

    pub fn opt_str(&self, index: usize) -> Result<Option<&str>, IndexError> {
        Ok(match self {
            Row::Sqlite(row) => row.get_ref(index)?.as_str_or_null()?,
            Row::Postgres(row) => row.try_get(index)?,
        })
    }

    pub fn str(&self, index: usize) -> Result<&str, IndexError> {
        self.opt_str(index)?.ok_or_else(|| null(index))
    }

    pub fn opt_bytes(&self, index: usize) -> Result<Option<&[u8]>, IndexError> {
        Ok(match self {
            Row::Sqlite(row) => row.get_ref(index)?.as_bytes_or_null()?,
            Row::Postgres(row) => match *row.columns()[index].type_() {
                Type::BYTEA => row.try_get(index)?,
                _ => row.try_get::<_, Option<&str>>(index)?.map(str::as_bytes),
            },
        })
    }

    pub fn bytes(&self, index: usize) -> Result<&[u8], IndexError> {
        self.opt_bytes(index)?.ok_or_else(|| null(index))
    }
}

fn null(index: usize) -> IndexError {
    IndexError::Database(format!("unexpected NULL in column {index}"))
}

/// `?` and `?N` placeholders as PostgreSQL `$N`, outside string literals.
fn numbered(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len() + 16);
    let mut next = 0;
    let mut quoted = false;
    let mut chars = sql.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                quoted = !quoted;
                out.push(c);
            }
            '?' if !quoted => {
                out.push('$');
                if chars.peek().is_some_and(char::is_ascii_digit) {
                    while let Some(digit) = chars.next_if(char::is_ascii_digit) {
                        out.push(digit);
                    }
                } else {
                    next += 1;
                    out.push_str(&next.to_string());
                }
            }
            c => out.push(c),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placeholders() {
        assert_eq!(
            numbered("SELECT a FROM t WHERE b = ? AND c IN (?, ?) AND d = '?'"),
            "SELECT a FROM t WHERE b = $1 AND c IN ($2, $3) AND d = '?'"
        );
        assert_eq!(
            numbered("UPDATE t SET a = ?2 || '\\' WHERE pk = ?1 OR ?12 = 'it''s ?'"),
            "UPDATE t SET a = $2 || '\\' WHERE pk = $1 OR $12 = 'it''s ?'"
        );
    }

    #[test]
    fn patterns() {
        assert_eq!(Dialect::Sqlite.pattern("A[1]*?"), "A[[]1[]]*?");
        assert_eq!(Dialect::Postgres.pattern("A_1%\\*?"), "A\\_1\\%\\\\%_");
    }
}
