//! QIDO-RS query model (PS3.18 §8.3.4) and DICOM attribute matching (PS3.4 §C.2.2.2).
//!
//! Matching keys on indexed attributes are turned into SQL by the store; the others are
//! evaluated here against DICOM JSON values.

use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry};
use dicom_core::{Tag, VR};
use dicom_dictionary_std::StandardDataDictionary;
use dicom_dictionary_std::tags;
use serde_json::Value;

#[derive(Debug, Clone, PartialEq)]
pub struct Filter {
    /// Attribute path: one tag, or sequence tags followed by the matched attribute.
    pub path: Vec<Tag>,
    pub vr: VR,
    pub pattern: Pattern,
}

impl Filter {
    pub fn tag(&self) -> Tag {
        self.path[0]
    }
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct Query {
    pub filters: Vec<Filter>,
    /// Additional attributes to return.
    pub include: Vec<Tag>,
    /// `includefield=all`.
    pub include_all: bool,
    pub fuzzy: bool,
    pub limit: Option<usize>,
    pub offset: usize,
}

/// Value of a matching key.
#[derive(Debug, Clone, PartialEq)]
pub enum Pattern {
    /// Empty value: matches anything, the attribute is returned.
    Universal,
    Exact(String),
    /// Contains `*` or `?`.
    Wildcard(String),
    /// Any of these values (UID lists, or backslash-separated values).
    List(Vec<String>),
    /// Inclusive range of DA, TM or DT values; a missing bound is open.
    Range(Option<String>, Option<String>),
}

#[derive(Debug, thiserror::Error, PartialEq)]
pub enum QueryError {
    #[error("unknown attribute {0:?}")]
    UnknownAttribute(String),
    #[error("invalid value for {0}: {1:?}")]
    InvalidValue(String, String),
}

/// Resolve an attribute ID: a keyword (`PatientName`) or a tag (`00100010`).
pub fn resolve_attribute(id: &str) -> Option<(Tag, VR)> {
    if id.len() == 8
        && let Ok(value) = u32::from_str_radix(id, 16)
    {
        let tag = Tag((value >> 16) as u16, value as u16);
        let vr = StandardDataDictionary
            .by_tag(tag)
            .map_or(VR::UN, |e| e.vr().relaxed());
        return Some((tag, vr));
    }
    let entry = StandardDataDictionary.by_name(id)?;
    Some((entry.tag_range().inner(), entry.vr().relaxed()))
}

/// Parse QIDO-RS query parameters.
pub fn parse(params: &[(String, String)]) -> Result<Query, QueryError> {
    let mut query = Query::default();
    for (key, value) in params {
        match key.as_str() {
            "limit" => {
                query.limit = Some(value.parse().map_err(|_| invalid("limit", value))?);
            }
            "offset" => query.offset = value.parse().map_err(|_| invalid("offset", value))?,
            "fuzzymatching" => {
                query.fuzzy = match value.as_str() {
                    "true" => true,
                    "false" => false,
                    _ => return Err(invalid("fuzzymatching", value)),
                }
            }
            "includefield" => {
                for field in value.split(',').map(str::trim).filter(|f| !f.is_empty()) {
                    if field == "all" {
                        query.include_all = true;
                    } else {
                        let (tag, _) = resolve_attribute(field)
                            .ok_or_else(|| QueryError::UnknownAttribute(field.to_owned()))?;
                        query.include.push(tag);
                    }
                }
            }
            key => {
                let mut path = Vec::new();
                let mut vr = VR::UN;
                for id in key.split('.') {
                    let (tag, id_vr) = resolve_attribute(id)
                        .ok_or_else(|| QueryError::UnknownAttribute(key.to_owned()))?;
                    path.push(tag);
                    vr = id_vr;
                }
                if path.len() == 1 && value.is_empty() {
                    query.include.push(path[0]);
                }
                query.filters.push(Filter {
                    pattern: pattern(vr, value),
                    path,
                    vr,
                });
            }
        }
    }
    Ok(query)
}

fn invalid(name: &str, value: &str) -> QueryError {
    QueryError::InvalidValue(name.to_owned(), value.to_owned())
}

/// Classify the value of a matching key according to the attribute VR.
pub fn pattern(vr: VR, value: &str) -> Pattern {
    if value.is_empty() || value == "*" {
        return Pattern::Universal;
    }
    match vr {
        // Wildcards are not allowed on UIDs; kept as such to be rejected.
        VR::UI if value.contains(['*', '?']) => Pattern::Wildcard(value.to_owned()),
        VR::UI => {
            let values: Vec<String> = value
                .split([',', '\\'])
                .map(str::trim)
                .filter(|v| !v.is_empty())
                .map(Into::into)
                .collect();
            if values.len() == 1 {
                Pattern::Exact(values[0].clone())
            } else {
                Pattern::List(values)
            }
        }
        VR::DA | VR::TM | VR::DT if value.contains('-') => match range(vr, value) {
            Some((low, high)) => Pattern::Range(low, high),
            None => Pattern::Exact(value.to_owned()),
        },
        _ if value.contains(['*', '?']) => Pattern::Wildcard(value.to_owned()),
        _ if value.contains('\\') => Pattern::List(value.split('\\').map(Into::into).collect()),
        _ => Pattern::Exact(value.to_owned()),
    }
}

/// Split a range on its hyphen; DT values may contain a `-` timezone offset, so for DT the
/// separator is the hyphen that splits the value into two plausible date-times.
fn range(vr: VR, value: &str) -> Option<(Option<String>, Option<String>)> {
    let bound = |s: &str| (!s.is_empty()).then(|| s.to_owned());
    if vr != VR::DT {
        let (low, high) = value.split_once('-')?;
        return Some((bound(low), bound(high)));
    }
    value
        .match_indices('-')
        .map(|(i, _)| (&value[..i], &value[i + 1..]))
        .find(|(low, high)| plausible_date_time(low) && plausible_date_time(high))
        .map(|(low, high)| (bound(low), bound(high)))
}

/// Empty, or digits (with an optional fraction) of at least a year, then an optional
/// `±HHMM` offset.
fn plausible_date_time(s: &str) -> bool {
    if s.is_empty() {
        return true;
    }
    let (main, offset) = match s.rfind(['+', '-']) {
        Some(i) => (&s[..i], Some(&s[i + 1..])),
        None => (s, None),
    };
    main.len() >= 4
        && main.bytes().all(|b| b.is_ascii_digit() || b == b'.')
        && offset.is_none_or(|o| o.len() == 4 && o.bytes().all(|b| b.is_ascii_digit()))
}

/// Whether `text` matches a DICOM wildcard pattern (`*` any sequence, `?` one character).
pub fn wildcard_match(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut pi, mut ti) = (0, 0);
    let (mut star, mut mark) = (None, 0);
    while ti < t.len() {
        if pi < p.len() && (p[pi] == '?' || p[pi] == t[ti]) {
            pi += 1;
            ti += 1;
        } else if pi < p.len() && p[pi] == '*' {
            star = Some(pi);
            pi += 1;
            mark = ti;
        } else if let Some(s) = star {
            pi = s + 1;
            mark += 1;
            ti = mark;
        } else {
            return false;
        }
    }
    p[pi..].iter().all(|c| *c == '*')
}

/// Whether `value` (a single attribute value as text) matches `pattern`.
pub fn value_matches(pattern: &Pattern, vr: VR, value: &str, fuzzy: bool) -> bool {
    let case_insensitive = vr == VR::PN;
    let fold = |s: &str| {
        if case_insensitive {
            s.to_uppercase()
        } else {
            s.to_owned()
        }
    };
    let value = fold(value.trim());
    match pattern {
        Pattern::Universal => true,
        Pattern::Exact(p) if vr == VR::PN && fuzzy => person_name_fuzzy(&fold(p), &value),
        Pattern::Exact(p) => fold(p) == value,
        Pattern::Wildcard(p) if vr == VR::PN && fuzzy => person_name_fuzzy(&fold(p), &value),
        Pattern::Wildcard(p) => wildcard_match(&fold(p), &value),
        Pattern::List(values) => values.iter().any(|p| fold(p) == value),
        Pattern::Range(low, high) => {
            low.as_deref().is_none_or(|l| value.as_str() >= l)
                && high.as_deref().is_none_or(|h| value.as_str() <= h)
        }
    }
}

/// Fuzzy person name matching: the pattern matches the beginning of any name component.
fn person_name_fuzzy(pattern: &str, name: &str) -> bool {
    let prefix = format!("{}*", pattern.trim_end_matches('*'));
    name.split(['^', '=', ' '])
        .any(|component| wildcard_match(&prefix, component))
}

/// Whether a DICOM JSON attribute (the object holding `vr` and `Value`, or `None` when the
/// attribute is absent) matches `filter`, following its path into sequences.
pub fn json_matches(filter: &Filter, attribute: Option<&Value>, fuzzy: bool) -> bool {
    json_path_matches(&filter.path[1..], filter, attribute, fuzzy)
}

fn json_path_matches(
    rest: &[Tag],
    filter: &Filter,
    attribute: Option<&Value>,
    fuzzy: bool,
) -> bool {
    if filter.pattern == Pattern::Universal {
        return true;
    }
    let values = attribute
        .and_then(|a| a.get("Value"))
        .and_then(Value::as_array);
    let Some(values) = values else { return false };
    match rest.split_first() {
        Some((tag, rest)) => values.iter().any(|item| {
            let key = crate::json::tag_hex(*tag);
            json_path_matches(rest, filter, item.get(&key), fuzzy)
        }),
        None => values.iter().any(|v| {
            let text = match v {
                Value::String(s) => s.clone(),
                Value::Number(n) => n.to_string(),
                Value::Object(pn) => pn
                    .get("Alphabetic")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_owned(),
                _ => return false,
            };
            value_matches(&filter.pattern, filter.vr, &text, fuzzy)
        }),
    }
}

/// ModalitiesInStudy, matched against the modalities of the study's series.
pub const MODALITIES_IN_STUDY: Tag = tags::MODALITIES_IN_STUDY;

#[cfg(test)]
mod tests {
    use super::*;

    fn params(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
        pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect()
    }

    #[test]
    fn parses_query_parameters() {
        let query = parse(&params(&[
            ("PatientName", "DOE*"),
            ("00080060", "CT\\MR"),
            ("StudyDate", "20200101-"),
            ("StudyInstanceUID", "1.2,1.3"),
            ("AccessionNumber", ""),
            ("includefield", "00081030,PatientAge"),
            ("includefield", "all"),
            ("limit", "10"),
            ("offset", "5"),
            ("fuzzymatching", "true"),
            ("RequestAttributesSequence.ScheduledProcedureStepID", "S1"),
        ]))
        .unwrap();
        assert_eq!(query.filters[0].pattern, Pattern::Wildcard("DOE*".into()));
        assert_eq!(query.filters[0].vr, VR::PN);
        assert_eq!(
            query.filters[1].pattern,
            Pattern::List(vec!["CT".into(), "MR".into()])
        );
        assert_eq!(
            query.filters[2].pattern,
            Pattern::Range(Some("20200101".into()), None)
        );
        assert_eq!(
            query.filters[3].pattern,
            Pattern::List(vec!["1.2".into(), "1.3".into()])
        );
        assert_eq!(query.filters[4].pattern, Pattern::Universal);
        assert_eq!(
            query.filters[5].path,
            vec![
                tags::REQUEST_ATTRIBUTES_SEQUENCE,
                tags::SCHEDULED_PROCEDURE_STEP_ID
            ]
        );
        assert_eq!(
            query.include,
            vec![
                tags::ACCESSION_NUMBER,
                tags::STUDY_DESCRIPTION,
                tags::PATIENT_AGE
            ]
        );
        assert!(query.include_all && query.fuzzy);
        assert_eq!((query.limit, query.offset), (Some(10), 5));
    }

    #[test]
    fn rejects_invalid_parameters() {
        assert!(matches!(
            parse(&params(&[("NotAKeyword", "x")])),
            Err(QueryError::UnknownAttribute(_))
        ));
        assert!(matches!(
            parse(&params(&[("limit", "-1")])),
            Err(QueryError::InvalidValue(..))
        ));
        assert!(matches!(
            parse(&params(&[("includefield", "Nope")])),
            Err(QueryError::UnknownAttribute(_))
        ));
    }

    #[test]
    fn date_time_ranges() {
        assert_eq!(
            pattern(VR::DA, "-20201231"),
            Pattern::Range(None, Some("20201231".into()))
        );
        assert_eq!(
            pattern(VR::DT, "20200101120000-0500-20200102"),
            Pattern::Range(Some("20200101120000-0500".into()), Some("20200102".into()))
        );
    }

    #[test]
    fn wildcards() {
        assert!(wildcard_match("DOE*", "DOE^JOHN"));
        assert!(wildcard_match("*^J?HN", "DOE^JOHN"));
        assert!(wildcard_match("*", ""));
        assert!(!wildcard_match("D?E", "DOOE"));
        assert!(!wildcard_match("DOE", "DOE^JOHN"));
    }

    #[test]
    fn values() {
        assert!(value_matches(
            &Pattern::Exact("doe^john".into()),
            VR::PN,
            "DOE^JOHN",
            false
        ));
        assert!(!value_matches(
            &Pattern::Exact("ct".into()),
            VR::CS,
            "CT",
            false
        ));
        assert!(value_matches(
            &Pattern::Exact("jo".into()),
            VR::PN,
            "DOE^JOHN",
            true
        ));
        assert!(!value_matches(
            &Pattern::Exact("jo".into()),
            VR::PN,
            "DOE^JOHN",
            false
        ));
        assert!(value_matches(
            &Pattern::Range(Some("2020".into()), Some("2021".into())),
            VR::DA,
            "20201010",
            false
        ));
    }

    #[test]
    fn json_attributes_and_sequences() {
        let json: Value = serde_json::json!({
            "00400275": {"vr": "SQ", "Value": [
                {"00400009": {"vr": "SH", "Value": ["S0"]}},
                {"00400009": {"vr": "SH", "Value": ["S1"]}}
            ]},
            "00100010": {"vr": "PN", "Value": [{"Alphabetic": "Doe^Jane"}]},
            "00201208": {"vr": "IS", "Value": [12]}
        });
        let query = parse(&params(&[
            ("RequestAttributesSequence.ScheduledProcedureStepID", "S1"),
            ("PatientName", "DOE*"),
            ("NumberOfStudyRelatedInstances", "12"),
            ("PatientID", "X"),
        ]))
        .unwrap();
        let attribute = |f: &Filter| json.get(crate::json::tag_hex(f.tag()));
        assert!(json_matches(
            &query.filters[0],
            attribute(&query.filters[0]),
            false
        ));
        assert!(json_matches(
            &query.filters[1],
            attribute(&query.filters[1]),
            false
        ));
        assert!(json_matches(
            &query.filters[2],
            attribute(&query.filters[2]),
            false
        ));
        assert!(!json_matches(
            &query.filters[3],
            attribute(&query.filters[3]),
            false
        ));
    }
}
