//! Query service (PS3.4 Annex C): C-FIND on the Patient Root and Study Root models.
//!
//! The identifier becomes a QIDO-RS query: every key with a value is a matching key, every
//! key is a return key. Matching follows QIDO-RS (indexed attributes in SQL, others against
//! the stored metadata) and is relational: keys of any level can be combined. The patient
//! level is built from the study level, grouping studies by Patient ID and Issuer.

use std::collections::HashMap;

use dicom_core::header::Header;
use dicom_core::value::{DataSetSequence, Value as DicomValue};
use dicom_core::{DataElement, PrimitiveValue, Tag, VR};
use dicom_dictionary_std::tags;
use dicom_encoding::TransferSyntaxIndex;
use dicom_object::InMemDicomObject;
use dicom_transfer_syntax_registry::TransferSyntaxRegistry;
use fortitude_core::attrs::Level;
use fortitude_core::json;
use fortitude_core::qido::{self, Filter, Pattern, Query};
use fortitude_store::search::{Search, SearchError};
use serde_json::{Map, Value};

use crate::association::{Connection, Error, Message};
use crate::command::status;
use crate::store::truncate;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum QueryLevel {
    Patient,
    Study,
    Series,
    Image,
}

impl QueryLevel {
    fn parse(value: &str, patient_root: bool) -> Option<Self> {
        match value {
            "PATIENT" if patient_root => Some(Self::Patient),
            "STUDY" => Some(Self::Study),
            "SERIES" => Some(Self::Series),
            "IMAGE" => Some(Self::Image),
            _ => None,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            Self::Patient => "PATIENT",
            Self::Study => "STUDY",
            Self::Series => "SERIES",
            Self::Image => "IMAGE",
        }
    }
}

/// Read the identifier of a C-FIND or C-MOVE request and its Query/Retrieve Level.
pub(crate) fn identifier(
    data: Option<&[u8]>,
    transfer_syntax: &str,
    patient_root: bool,
) -> Result<(InMemDicomObject, QueryLevel), String> {
    let data = data.ok_or("no identifier")?;
    let ts = TransferSyntaxRegistry
        .get(transfer_syntax)
        .ok_or_else(|| format!("unknown transfer syntax {transfer_syntax}"))?;
    let object = InMemDicomObject::read_dataset_with_ts(data, ts)
        .map_err(|e| format!("cannot read identifier: {e}"))?;
    let level = string(&object, tags::QUERY_RETRIEVE_LEVEL).unwrap_or_default();
    let level = QueryLevel::parse(&level, patient_root)
        .ok_or_else(|| format!("invalid QueryRetrieveLevel {level:?}"))?;
    Ok((object, level))
}

/// Text value of an element, multiple values separated by `\`, without padding.
pub(crate) fn string(object: &InMemDicomObject, tag: Tag) -> Option<String> {
    let value = object.get(tag)?.to_str().ok()?;
    let value = value.trim_matches([' ', '\0']);
    (!value.is_empty()).then(|| value.to_owned())
}

/// Return keys that are computed or describe the service, never matched.
const RETURN_ONLY: &[Tag] = &[
    tags::RETRIEVE_AE_TITLE,
    tags::INSTANCE_AVAILABILITY,
    tags::NUMBER_OF_PATIENT_RELATED_STUDIES,
    tags::NUMBER_OF_PATIENT_RELATED_SERIES,
    tags::NUMBER_OF_PATIENT_RELATED_INSTANCES,
    tags::NUMBER_OF_STUDY_RELATED_SERIES,
    tags::NUMBER_OF_STUDY_RELATED_INSTANCES,
    tags::NUMBER_OF_SERIES_RELATED_INSTANCES,
];

pub(crate) struct Request {
    level: QueryLevel,
    /// Requested keys, in identifier order.
    keys: Vec<(Tag, VR)>,
    query: Query,
}

impl Request {
    pub(crate) fn new(identifier: &InMemDicomObject, level: QueryLevel) -> Self {
        let mut keys = Vec::new();
        let mut query = Query::default();
        for element in identifier.iter() {
            let tag = element.tag();
            if tag == tags::QUERY_RETRIEVE_LEVEL
                || tag == tags::SPECIFIC_CHARACTER_SET
                || tag.element() == 0x0000
            {
                continue;
            }
            let vr = element.vr();
            keys.push((tag, vr));
            query.include.push(tag);
            if vr == VR::SQ || RETURN_ONLY.contains(&tag) {
                continue;
            }
            let value = string(identifier, tag).unwrap_or_default();
            let pattern = qido::pattern(vr, &value);
            if pattern != Pattern::Universal {
                query.filters.push(Filter {
                    path: vec![tag],
                    vr,
                    pattern,
                });
            }
        }
        Self { level, keys, query }
    }
}

/// Matches of a query, as DICOM JSON objects.
struct Matches {
    objects: Vec<Map<String, Value>>,
    truncated: bool,
}

async fn search(dimse: &crate::Dimse, request: &Request) -> Result<Matches, SearchError> {
    let level = match request.level {
        QueryLevel::Patient | QueryLevel::Study => Level::Study,
        QueryLevel::Series => Level::Series,
        QueryLevel::Image => Level::Instance,
    };
    let result = dimse
        .store
        .search(
            Search {
                level,
                study: None,
                series: None,
                query: request.query.clone(),
                max_results: dimse.config.max_find_results,
            },
            String::new(),
        )
        .await?;
    let objects = match serde_json::from_slice(&result.body) {
        Ok(Value::Array(items)) => items
            .into_iter()
            .filter_map(|item| match item {
                Value::Object(object) => Some(object),
                _ => None,
            })
            .collect(),
        _ => return Err(SearchError::Invalid("unreadable search result".into())),
    };
    let objects = if request.level == QueryLevel::Patient {
        patients(objects)
    } else {
        objects
    };
    Ok(Matches {
        objects,
        truncated: result.truncated,
    })
}

/// Group study matches by patient (Patient ID and Issuer of Patient ID), with the patient
/// related counts.
fn patients(studies: Vec<Map<String, Value>>) -> Vec<Map<String, Value>> {
    let text = |object: &Map<String, Value>, tag: Tag| {
        object
            .get(&json::tag_hex(tag))
            .and_then(|a| a.get("Value"))
            .and_then(|v| v.get(0))
            .map(|v| v.to_string())
            .unwrap_or_default()
    };
    let number = |object: &Map<String, Value>, tag: Tag| {
        object
            .get(&json::tag_hex(tag))
            .and_then(|a| a.get("Value"))
            .and_then(|v| v.get(0))
            .and_then(Value::as_u64)
            .unwrap_or(0)
    };
    let mut index = HashMap::new();
    let mut patients: Vec<(Map<String, Value>, [u64; 3])> = Vec::new();
    for study in studies {
        let key = (
            text(&study, tags::PATIENT_ID),
            text(&study, tags::ISSUER_OF_PATIENT_ID),
        );
        let counts = [
            1,
            number(&study, tags::NUMBER_OF_STUDY_RELATED_SERIES),
            number(&study, tags::NUMBER_OF_STUDY_RELATED_INSTANCES),
        ];
        match index.get(&key) {
            Some(&i) => {
                let (_, total): &mut (Map<String, Value>, [u64; 3]) = &mut patients[i];
                for (t, c) in total.iter_mut().zip(counts) {
                    *t += c;
                }
            }
            None => {
                index.insert(key, patients.len());
                patients.push((study, counts));
            }
        }
    }
    patients
        .into_iter()
        .map(|(mut patient, [studies, series, instances])| {
            for (tag, count) in [
                (tags::NUMBER_OF_PATIENT_RELATED_STUDIES, studies),
                (tags::NUMBER_OF_PATIENT_RELATED_SERIES, series),
                (tags::NUMBER_OF_PATIENT_RELATED_INSTANCES, instances),
            ] {
                patient.insert(
                    json::tag_hex(tag),
                    serde_json::json!({"vr": "IS", "Value": [count]}),
                );
            }
            patient
        })
        .collect()
}

/// The response identifier of one match: the requested keys, empty when the match has no
/// value for them.
fn response(found: &Map<String, Value>, request: &Request, ae_title: &str) -> InMemDicomObject {
    let mut object = InMemDicomObject::from_element_iter([
        DataElement::new(
            tags::SPECIFIC_CHARACTER_SET,
            VR::CS,
            PrimitiveValue::from("ISO_IR 192"),
        ),
        DataElement::new(
            tags::QUERY_RETRIEVE_LEVEL,
            VR::CS,
            PrimitiveValue::from(request.level.as_str()),
        ),
    ]);
    for &(tag, vr) in &request.keys {
        if tag == tags::RETRIEVE_AE_TITLE {
            object.put(DataElement::new(
                tag,
                VR::AE,
                PrimitiveValue::from(ae_title),
            ));
            continue;
        }
        let key = json::tag_hex(tag);
        let element = found
            .get(&key)
            .filter(|member| member.get("BulkDataURI").is_none())
            .and_then(|member| {
                let single = Value::Object(Map::from_iter([(key, member.clone())]));
                dicom_json::from_value::<InMemDicomObject>(single).ok()
            })
            .and_then(|o| o.into_iter().next());
        object.put(element.unwrap_or_else(|| empty(tag, vr)));
    }
    object
}

fn empty(tag: Tag, vr: VR) -> DataElement<InMemDicomObject> {
    if vr == VR::SQ {
        DataElement::new(tag, VR::SQ, DicomValue::from(DataSetSequence::empty()))
    } else {
        DataElement::new(tag, vr, PrimitiveValue::Empty)
    }
}

pub(crate) async fn handle(
    dimse: &crate::Dimse,
    connection: &mut Connection,
    message: Message,
) -> Result<u16, Error> {
    let Message {
        context,
        command,
        data,
    } = message;
    let (abstract_syntax, transfer_syntax) = connection
        .context(context)
        .map(|(a, t)| (a.to_owned(), t.to_owned()))
        .unwrap_or_default();
    let message_id = command.message_id().unwrap_or_default();
    let fail = |status: u16, comment: &str| {
        command
            .response(status, false)
            .with_text(tags::ERROR_COMMENT, truncate(comment, 64))
    };

    let patient_root = abstract_syntax == crate::PATIENT_ROOT_FIND;
    let request = match identifier(data.as_deref(), &transfer_syntax, patient_root) {
        Ok((identifier, level)) => Request::new(&identifier, level),
        Err(comment) => {
            connection
                .send(context, &fail(status::IDENTIFIER_MISMATCH, &comment), None)
                .await?;
            return Ok(status::IDENTIFIER_MISMATCH);
        }
    };
    let matches = match search(dimse, &request).await {
        Ok(matches) => matches,
        Err(SearchError::Invalid(comment)) => {
            connection
                .send(context, &fail(status::IDENTIFIER_MISMATCH, &comment), None)
                .await?;
            return Ok(status::IDENTIFIER_MISMATCH);
        }
        Err(e) => {
            tracing::warn!(error = %e, "C-FIND failed");
            connection
                .send(
                    context,
                    &fail(status::UNABLE_TO_PROCESS, &e.to_string()),
                    None,
                )
                .await?;
            return Ok(status::UNABLE_TO_PROCESS);
        }
    };

    let ts = TransferSyntaxRegistry
        .get(&transfer_syntax)
        .expect("negotiated transfer syntaxes are known");
    let pending = command.response(status::PENDING, true);
    let mut encoded = Vec::new();
    for found in &matches.objects {
        if connection.cancelled(message_id) {
            let response = command.response(status::CANCEL, false);
            connection.send(context, &response, None).await?;
            return Ok(status::CANCEL);
        }
        encoded.clear();
        let object = response(found, &request, &dimse.config.ae_title);
        if let Err(e) = object.write_dataset_with_ts(&mut encoded, ts) {
            tracing::warn!(error = %e, "C-FIND: cannot encode a match");
            continue;
        }
        connection.send(context, &pending, Some(&encoded)).await?;
    }
    if matches.truncated {
        let comment = format!(
            "more than {} matches, refine the query",
            dimse.config.max_find_results
        );
        connection
            .send(context, &fail(status::OUT_OF_RESOURCES, &comment), None)
            .await?;
        return Ok(status::OUT_OF_RESOURCES);
    }
    let done = command.response(status::SUCCESS, false);
    connection.send(context, &done, None).await?;
    Ok(status::SUCCESS)
}
