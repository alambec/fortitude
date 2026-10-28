//! QIDO-RS searches over the index.
//!
//! Matching keys on indexed attributes become SQL conditions; keys on attributes of a
//! deeper level (e.g. Modality in a study search) become `IN` subqueries. Other keys are
//! evaluated against the stored metadata of each candidate's representative instance, in
//! which case pagination happens after that filtering.
//!
//! Responses are assembled from the attributes pre-encoded at ingest plus computed ones
//! (counts, modalities, retrieve URLs).

use std::sync::Arc;

use dicom_core::{Tag, VR};
use dicom_dictionary_std::tags;
use fortitude_core::attrs::{self, Kind, Level};
use fortitude_core::json;
use fortitude_core::qido::{self, Filter, MODALITIES_IN_STUDY, Pattern, Query};
use rusqlite::types::Value;

use crate::Store;
use crate::index::{IndexError, person_name_key};

#[derive(Debug, Clone)]
pub struct Search {
    pub level: Level,
    /// Study of the request path (`/studies/{study}/…`).
    pub study: Option<String>,
    /// Series of the request path (`/studies/{study}/series/{series}/instances`).
    pub series: Option<String>,
    pub query: Query,
    /// Maximum number of results returned, 0 for no limit.
    pub max_results: usize,
}

#[derive(Debug)]
pub struct SearchResult {
    /// JSON array of matches.
    pub body: Vec<u8>,
    pub count: usize,
    /// More results exist beyond the server's maximum.
    pub truncated: bool,
}

#[derive(Debug, thiserror::Error)]
pub enum SearchError {
    #[error(transparent)]
    Index(#[from] IndexError),
    #[error("{0}")]
    Invalid(String),
}

impl From<rusqlite::Error> for SearchError {
    fn from(e: rusqlite::Error) -> Self {
        SearchError::Index(e.into())
    }
}

impl From<rusqlite::types::FromSqlError> for SearchError {
    fn from(e: rusqlite::types::FromSqlError) -> Self {
        SearchError::Index(e.into())
    }
}

impl Store {
    pub async fn search(
        self: &Arc<Self>,
        search: Search,
        base: String,
    ) -> Result<SearchResult, SearchError> {
        let plan = Plan::new(&search)?;
        self.index()
            .readers()
            .run(move |conn| Ok(plan.execute(conn, &search, &base)))
            .await?
    }
}

fn alias(level: Level) -> &'static str {
    match level {
        Level::Study => "st",
        Level::Series => "s",
        Level::Instance => "i",
    }
}

struct Plan {
    sql: String,
    params: Vec<Value>,
    residual: Vec<Filter>,
    /// Attributes to return beyond the pre-encoded ones: `includefield` and matching keys.
    include: Vec<Tag>,
    limit: usize,
    /// The limit comes from the server maximum, not from the client.
    server_limited: bool,
}

impl Plan {
    fn new(search: &Search) -> Result<Self, SearchError> {
        let level = search.level;
        let query = &search.query;
        let mut conditions = Vec::new();
        let mut params = Vec::new();
        let mut residual = Vec::new();
        if let Some(study) = &search.study {
            conditions.push("st.study_uid = ?".to_owned());
            params.push(Value::Text(study.clone()));
        }
        if let Some(series) = &search.series {
            conditions.push("s.series_uid = ?".to_owned());
            params.push(Value::Text(series.clone()));
        }
        for filter in &query.filters {
            if filter.pattern == Pattern::Universal {
                continue;
            }
            let tag = filter.tag();
            if filter.path.len() > 1 {
                residual.push(filter.clone());
            } else if tag == MODALITIES_IN_STUDY {
                conditions.push(modalities_condition(&filter.pattern, &mut params)?);
            } else if let Some(attribute) = attrs::find_indexed(tag) {
                let column = match attribute.kind {
                    Kind::PersonName => format!("{}_key", attribute.column),
                    _ => attribute.column.to_owned(),
                };
                if attribute.level <= level {
                    let column = format!("{}.{column}", alias(attribute.level));
                    conditions.push(condition(
                        &column,
                        attribute.kind,
                        &filter.pattern,
                        query.fuzzy,
                        &mut params,
                    )?);
                } else {
                    // A non-correlated subquery is evaluated once, whatever its selectivity.
                    let (key, table, link) = match (level, attribute.level) {
                        (Level::Study, Level::Series) => ("st.pk", "series", "study_pk"),
                        (Level::Study, _) => ("st.pk", "instances", "study_pk"),
                        _ => ("s.pk", "instances", "series_pk"),
                    };
                    let condition = condition(
                        &format!("x.{column}"),
                        attribute.kind,
                        &filter.pattern,
                        query.fuzzy,
                        &mut params,
                    )?;
                    conditions.push(format!(
                        "{key} IN (SELECT x.{link} FROM {table} x WHERE {condition})"
                    ));
                }
            } else {
                residual.push(filter.clone());
            }
        }

        // Matching keys are returned too (PS3.18 §8.3.4.3); indexed ones already are.
        let include: Vec<Tag> = query
            .include
            .iter()
            .copied()
            .chain(residual.iter().map(Filter::tag))
            .filter(|tag| !returned_by_default(search, *tag))
            .collect();
        let needs_metadata = !residual.is_empty() || query.include_all || !include.is_empty();
        let metadata = if needs_metadata { "m.metadata" } else { "NULL" };
        let (select, from, order) = match level {
            Level::Study => (
                "st.study_uid, NULL, NULL, st.attrs, NULL, NULL, st.modalities, st.num_series, st.num_instances",
                "studies st",
                "st.pk",
            ),
            Level::Series => (
                "st.study_uid, s.series_uid, NULL, s.attrs, NULL, st.attrs, NULL, NULL, s.num_instances",
                "series s JOIN studies st ON st.pk = s.study_pk",
                if search.study.is_some() {
                    "s.series_number, s.pk"
                } else {
                    "s.pk"
                },
            ),
            Level::Instance => (
                "st.study_uid, s.series_uid, i.sop_uid, i.attrs, s.attrs, st.attrs, NULL, NULL, NULL",
                "instances i JOIN series s ON s.pk = i.series_pk JOIN studies st ON st.pk = i.study_pk",
                match (&search.study, &search.series) {
                    (_, Some(_)) => "i.instance_number, i.pk",
                    (Some(_), None) => "s.series_number, s.pk, i.instance_number, i.pk",
                    _ => "i.pk",
                },
            ),
        };
        let metadata_join = match (needs_metadata, level) {
            (false, _) => "",
            (true, Level::Study) => " LEFT JOIN instances m ON m.pk = st.rep_instance",
            (true, Level::Series) => " LEFT JOIN instances m ON m.pk = s.rep_instance",
            (true, Level::Instance) => " JOIN instances m ON m.pk = i.pk",
        };
        let max = if search.max_results == 0 {
            usize::MAX
        } else {
            search.max_results
        };
        let limit = query.limit.unwrap_or(max).min(max);
        let server_limited = query.limit.is_none_or(|l| l > max);

        let mut sql = format!("SELECT {select}, {metadata} FROM {from}{metadata_join}");
        if !conditions.is_empty() {
            sql.push_str(" WHERE ");
            sql.push_str(&conditions.join(" AND "));
        }
        sql.push_str(&format!(" ORDER BY {order}"));
        if residual.is_empty() {
            // One extra row tells whether results were truncated.
            sql.push_str(" LIMIT ? OFFSET ?");
            params.push(Value::Integer(
                limit.saturating_add(1).min(i64::MAX as usize) as i64,
            ));
            params.push(Value::Integer(query.offset.min(i64::MAX as usize) as i64));
        }
        Ok(Self {
            sql,
            params,
            residual,
            include,
            limit,
            server_limited,
        })
    }

    fn execute(
        &self,
        conn: &rusqlite::Connection,
        search: &Search,
        base: &str,
    ) -> Result<SearchResult, SearchError> {
        let query = &search.query;
        let mut statement = conn.prepare_cached(&self.sql)?;
        let mut rows = statement.query(rusqlite::params_from_iter(&self.params))?;
        let mut body = Vec::with_capacity(16 * 1024);
        body.push(b'[');
        let mut count = 0;
        let mut skipped = 0;
        let mut truncated = false;
        while let Some(row) = rows.next()? {
            let metadata = row.get_ref(9)?.as_blob_or_null()?;
            if !self.residual.is_empty() {
                if !residual_matches(&self.residual, metadata, query.fuzzy) {
                    continue;
                }
                if skipped < query.offset {
                    skipped += 1;
                    continue;
                }
            }
            if count == self.limit {
                truncated = self.server_limited;
                break;
            }
            if count > 0 {
                body.push(b',');
            }
            render_row(row, search, &self.include, metadata, base, &mut body)?;
            count += 1;
        }
        body.push(b']');
        Ok(SearchResult {
            body,
            count,
            truncated,
        })
    }
}

/// Whether `tag` is part of the pre-encoded or computed attributes of the results.
fn returned_by_default(search: &Search, tag: Tag) -> bool {
    let computed = [
        tags::INSTANCE_AVAILABILITY,
        tags::RETRIEVE_URL,
        MODALITIES_IN_STUDY,
        tags::NUMBER_OF_STUDY_RELATED_SERIES,
        tags::NUMBER_OF_STUDY_RELATED_INSTANCES,
        tags::NUMBER_OF_SERIES_RELATED_INSTANCES,
    ];
    computed.contains(&tag)
        || attrs::default_return(search.level).contains(&tag)
        || (search.level == Level::Instance
            && search.series.is_none()
            && attrs::SERIES_RETURN.contains(&tag))
        || (search.level != Level::Study
            && search.study.is_none()
            && attrs::STUDY_RETURN.contains(&tag))
}

/// SQL condition for one matching key on `column`.
fn condition(
    column: &str,
    kind: Kind,
    pattern: &Pattern,
    fuzzy: bool,
    params: &mut Vec<Value>,
) -> Result<String, SearchError> {
    let text = |value: &str| match kind {
        Kind::PersonName => person_name_key(value),
        _ => value.to_owned(),
    };
    let integer = |value: &str| {
        value
            .trim()
            .parse::<i64>()
            .map(Value::Integer)
            .map_err(|_| SearchError::Invalid(format!("{value:?} is not an integer")))
    };
    let placeholders = |n: usize| vec!["?"; n].join(", ");
    Ok(match (kind, pattern) {
        (_, Pattern::Universal) => "1".to_owned(),
        (Kind::Integer, Pattern::Exact(value)) => {
            params.push(integer(value)?);
            format!("{column} = ?")
        }
        (Kind::Integer, Pattern::List(values)) => {
            for value in values {
                params.push(integer(value)?);
            }
            format!("{column} IN ({})", placeholders(values.len()))
        }
        (Kind::Integer, _) => {
            return Err(SearchError::Invalid(
                "integers only support exact matching".into(),
            ));
        }
        (Kind::PersonName, Pattern::Exact(value) | Pattern::Wildcard(value)) if fuzzy => {
            let prefix = glob(&text(value.trim_end_matches('*')));
            params.push(Value::Text(format!("{prefix}*")));
            params.push(Value::Text(format!("*^{prefix}*")));
            params.push(Value::Text(format!("* {prefix}*")));
            format!("({column} GLOB ? OR {column} GLOB ? OR {column} GLOB ?)")
        }
        (Kind::Uid, Pattern::Wildcard(_)) => {
            return Err(SearchError::Invalid("UIDs do not support wildcards".into()));
        }
        (_, Pattern::Exact(value)) => {
            params.push(Value::Text(text(value)));
            format!("{column} = ?")
        }
        (_, Pattern::Wildcard(value)) => {
            params.push(Value::Text(glob(&text(value))));
            format!("{column} GLOB ?")
        }
        (_, Pattern::List(values)) => {
            params.extend(values.iter().map(|v| Value::Text(text(v))));
            format!("{column} IN ({})", placeholders(values.len()))
        }
        (Kind::Date | Kind::Time, Pattern::Range(low, high)) => {
            let mut parts = Vec::new();
            if let Some(low) = low {
                parts.push(format!("{column} >= ?"));
                params.push(Value::Text(low.clone()));
            }
            if let Some(high) = high {
                // A high bound matches values starting with it (e.g. a time with fraction).
                parts.push(format!("{column} <= ?"));
                params.push(Value::Text(format!("{high}\u{10FFFF}")));
            }
            if parts.is_empty() {
                "1".to_owned()
            } else {
                parts.join(" AND ")
            }
        }
        (_, Pattern::Range(..)) => {
            return Err(SearchError::Invalid(
                "range matching only applies to dates and times".into(),
            ));
        }
    })
}

/// ModalitiesInStudy: exact values are looked up in the study's denormalized list of
/// modalities, wildcards in its series.
fn modalities_condition(pattern: &Pattern, params: &mut Vec<Value>) -> Result<String, SearchError> {
    let values = match pattern {
        Pattern::Exact(value) => vec![value.clone()],
        Pattern::List(values) => values.clone(),
        _ => {
            let condition = condition("x.modality", Kind::Text, pattern, false, params)?;
            return Ok(format!(
                "st.pk IN (SELECT x.study_pk FROM series x WHERE {condition})"
            ));
        }
    };
    let checks: Vec<&str> = values
        .iter()
        .map(|value| {
            params.push(Value::Text(format!("\\{value}\\")));
            "instr('\\' || st.modalities || '\\', ?) > 0"
        })
        .collect();
    Ok(format!("({})", checks.join(" OR ")))
}

/// DICOM wildcard pattern as a SQLite GLOB pattern: `*` and `?` are the same, `[` and `]`
/// are escaped.
fn glob(pattern: &str) -> String {
    let mut out = String::with_capacity(pattern.len());
    for c in pattern.chars() {
        match c {
            '[' => out.push_str("[[]"),
            ']' => out.push_str("[]]"),
            c => out.push(c),
        }
    }
    out
}

fn residual_matches(filters: &[Filter], metadata: Option<&[u8]>, fuzzy: bool) -> bool {
    let Some(metadata) = metadata else {
        return false;
    };
    let mut text = b"{".to_vec();
    json::render(metadata, "", &mut text);
    text.push(b'}');
    let Ok(serde_json::Value::Object(object)) = serde_json::from_slice::<serde_json::Value>(&text)
    else {
        return false;
    };
    filters
        .iter()
        .all(|f| qido::json_matches(f, object.get(&json::tag_hex(f.tag())), fuzzy))
}

fn is_bulk(member: &[u8]) -> bool {
    memchr::memmem::find(member, b"\"BulkDataURI\"").is_some()
}

/// Render one result row as a JSON object.
fn render_row(
    row: &rusqlite::Row<'_>,
    search: &Search,
    include: &[Tag],
    metadata: Option<&[u8]>,
    base: &str,
    out: &mut Vec<u8>,
) -> rusqlite::Result<()> {
    let study_uid: String = row.get(0)?;
    let series_uid: Option<String> = row.get(1)?;
    let sop_uid: Option<String> = row.get(2)?;
    let own = row.get_ref(3)?.as_blob()?;
    let series_attrs = row.get_ref(4)?.as_blob_or_null()?;
    let study_attrs = row.get_ref(5)?.as_blob_or_null()?;

    let mut computed = vec![json::string_member(
        tags::INSTANCE_AVAILABILITY,
        VR::CS,
        "ONLINE",
    )];
    let mut url = format!("{base}/studies/{study_uid}");
    match search.level {
        Level::Study => {
            let modalities: String = row.get(6)?;
            let mut modalities: Vec<&str> =
                modalities.split('\\').filter(|m| !m.is_empty()).collect();
            modalities.sort_unstable();
            computed.push(json::strings_member(
                MODALITIES_IN_STUDY,
                VR::CS,
                modalities,
            ));
            computed.push(json::integer_member(
                tags::NUMBER_OF_STUDY_RELATED_SERIES,
                VR::IS,
                row.get(7)?,
            ));
            computed.push(json::integer_member(
                tags::NUMBER_OF_STUDY_RELATED_INSTANCES,
                VR::IS,
                row.get(8)?,
            ));
        }
        Level::Series => {
            url = format!("{url}/series/{}", series_uid.as_deref().unwrap_or_default());
            computed.push(json::integer_member(
                tags::NUMBER_OF_SERIES_RELATED_INSTANCES,
                VR::IS,
                row.get(8)?,
            ));
        }
        Level::Instance => {
            url = format!(
                "{url}/series/{}/instances/{}",
                series_uid.as_deref().unwrap_or_default(),
                sop_uid.as_deref().unwrap_or_default()
            );
        }
    }
    computed.push(json::string_member(tags::RETRIEVE_URL, VR::UR, &url));

    let mut members: Vec<(Tag, &[u8])> = json::members(own).collect();
    if search.series.is_none()
        && let Some(series_attrs) = series_attrs
    {
        members.extend(json::members(series_attrs));
    }
    if search.study.is_none()
        && let Some(study_attrs) = study_attrs
    {
        members.extend(json::members(study_attrs));
    }
    members.extend(computed.iter().flat_map(|m| json::members(m.as_bytes())));
    let include_all = search.query.include_all;
    if let Some(metadata) = metadata
        && (include_all || !include.is_empty())
    {
        members
            .extend(json::members(metadata).filter(|(tag, member)| {
                include.contains(tag) || (include_all && !is_bulk(member))
            }));
    }
    json::render_object(&mut members, base, out);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn glob_escapes_brackets() {
        assert_eq!(glob("A[1]*"), "A[[]1[]]*");
    }

    #[test]
    fn conditions() {
        let mut params = Vec::new();
        let range = Pattern::Range(Some("2020".into()), None);
        assert_eq!(
            condition("c", Kind::Date, &range, false, &mut params).unwrap(),
            "c >= ?"
        );
        assert!(condition("c", Kind::Text, &range, false, &mut params).is_err());
        assert!(
            condition(
                "c",
                Kind::Integer,
                &Pattern::Exact("x".into()),
                false,
                &mut params
            )
            .is_err()
        );
        let pn = condition(
            "c",
            Kind::PersonName,
            &Pattern::Exact("doe".into()),
            true,
            &mut params,
        )
        .unwrap();
        assert_eq!(pn, "(c GLOB ? OR c GLOB ? OR c GLOB ?)");
        assert_eq!(params.last(), Some(&Value::Text("* DOE*".into())));
    }
}
