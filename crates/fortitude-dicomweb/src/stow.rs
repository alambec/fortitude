//! STOW-RS: Store Instances (PS3.18 §10.5).
//!
//! Instances already stored are discarded and reported in ReferencedSOPSequence with the
//! Warning Reason [`DUPLICATE_DISCARDED`], which makes the response a 202.

use std::sync::Arc;

use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use dicom_core::VR;
use dicom_dictionary_std::tags;
use fortitude_core::json;
use fortitude_store::{StoreResult, Stored};
use tokio::sync::Semaphore;

use crate::media::MediaType;
use crate::multipart::MultipartReader;
use crate::{BaseUrl, DICOM_JSON, DicomWeb, error};

/// Implementation-specific Warning Reason: the instance was already stored, the received
/// copy was discarded.
pub const DUPLICATE_DISCARDED: u16 = 0xB111;
/// Failure Reason for parts that are not DICOM instances.
const CANNOT_UNDERSTAND: u16 = 0xC000;

pub async fn store_instances(
    state: State<DicomWeb>,
    base: BaseUrl,
    headers: HeaderMap,
    body: Body,
) -> Response {
    stow(state, base, None, headers, body).await
}

pub async fn store_study_instances(
    state: State<DicomWeb>,
    base: BaseUrl,
    Path(study): Path<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    stow(state, base, Some(study), headers, body).await
}

async fn stow(
    State(state): State<DicomWeb>,
    BaseUrl(base): BaseUrl,
    study: Option<String>,
    headers: HeaderMap,
    body: Body,
) -> Response {
    let Some(media_type) = headers
        .get(header::CONTENT_TYPE)
        .and_then(|v| v.to_str().ok())
        .and_then(MediaType::parse)
    else {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "expected multipart/related; type=\"application/dicom\"",
        );
    };
    if media_type.essence != "multipart/related"
        || media_type
            .param("type")
            .is_some_and(|t| !t.eq_ignore_ascii_case("application/dicom"))
    {
        return error(
            StatusCode::UNSUPPORTED_MEDIA_TYPE,
            "only multipart/related; type=\"application/dicom\" is supported",
        );
    }
    let Some(boundary) = media_type.param("boundary") else {
        return error(StatusCode::BAD_REQUEST, "missing multipart boundary");
    };

    let mut reader = MultipartReader::new(
        body.into_data_stream(),
        boundary,
        state.config.max_instance_size,
    );
    let permits = Arc::new(Semaphore::new(state.config.stow_concurrency.max(1)));
    let mut tasks = Vec::new();
    let mut other_failures = Vec::new();
    loop {
        match reader.next_part().await {
            Ok(Some(part)) => {
                if part
                    .content_type
                    .as_deref()
                    .and_then(MediaType::parse)
                    .is_some_and(|m| m.essence != "application/dicom")
                {
                    other_failures.push(CANNOT_UNDERSTAND);
                    continue;
                }
                let Ok(permit) = permits.clone().acquire_owned().await else {
                    break;
                };
                let store = state.store.clone();
                let study = study.clone();
                tasks.push(tokio::spawn(async move {
                    let result = store.store(part.body, study).await;
                    drop(permit);
                    result
                }));
            }
            Ok(None) => break,
            Err(e) if tasks.is_empty() && other_failures.is_empty() => {
                return error(StatusCode::BAD_REQUEST, e.to_string());
            }
            Err(e) => {
                tracing::warn!(error = %e, "STOW-RS body ended abnormally");
                other_failures.push(CANNOT_UNDERSTAND);
                break;
            }
        }
    }

    let mut results = Vec::with_capacity(tasks.len());
    for task in tasks {
        match task.await {
            Ok(result) => results.push(result),
            Err(e) => {
                tracing::error!(error = %e, "store task failed");
                other_failures.push(0x0110);
            }
        }
    }
    response(&base, study.as_deref(), &results, &other_failures)
}

fn response(
    base: &str,
    study: Option<&str>,
    results: &[StoreResult],
    other_failures: &[u16],
) -> Response {
    let mut referenced = Vec::new();
    let mut failed = Vec::new();
    let mut warnings = 0;
    for result in results {
        let mut members = Vec::new();
        if let Some(class) = &result.sop_class_uid {
            members.push(json::string_member(
                tags::REFERENCED_SOP_CLASS_UID,
                VR::UI,
                class,
            ));
        }
        if let Some(sop) = &result.sop_uid {
            members.push(json::string_member(
                tags::REFERENCED_SOP_INSTANCE_UID,
                VR::UI,
                sop,
            ));
        }
        match &result.outcome {
            Ok(stored) => {
                if let (Some(st), Some(se), Some(sop)) =
                    (&result.study_uid, &result.series_uid, &result.sop_uid)
                {
                    let url = format!("{base}/studies/{st}/series/{se}/instances/{sop}");
                    members.push(json::string_member(tags::RETRIEVE_URL, VR::UR, &url));
                }
                if *stored == Stored::Duplicate {
                    warnings += 1;
                    members.push(json::integer_member(
                        tags::WARNING_REASON,
                        VR::US,
                        i64::from(DUPLICATE_DISCARDED),
                    ));
                }
                referenced.push(format!("{{{}}}", members.join(",")));
            }
            Err(failure) => {
                tracing::info!(sop_uid = ?result.sop_uid, %failure, "instance not stored");
                members.push(json::integer_member(
                    tags::FAILURE_REASON,
                    VR::US,
                    i64::from(failure.failure_reason()),
                ));
                failed.push(format!("{{{}}}", members.join(",")));
            }
        }
    }

    let mut members = Vec::new();
    let studies: std::collections::BTreeSet<&str> = results
        .iter()
        .filter(|r| r.outcome.is_ok())
        .filter_map(|r| r.study_uid.as_deref())
        .collect();
    if let Some(study) =
        study.or_else(|| (studies.len() == 1).then(|| *studies.first().unwrap_or(&"")))
    {
        members.push(json::string_member(
            tags::RETRIEVE_URL,
            VR::UR,
            &format!("{base}/studies/{study}"),
        ));
    }
    if !failed.is_empty() {
        members.push(json::sequence_member(tags::FAILED_SOP_SEQUENCE, &failed));
    }
    if !referenced.is_empty() {
        members.push(json::sequence_member(
            tags::REFERENCED_SOP_SEQUENCE,
            &referenced,
        ));
    }
    if !other_failures.is_empty() {
        let items: Vec<String> = other_failures
            .iter()
            .map(|reason| {
                format!(
                    "{{{}}}",
                    json::integer_member(tags::FAILURE_REASON, VR::US, i64::from(*reason))
                )
            })
            .collect();
        members.push(json::sequence_member(tags::OTHER_FAILURES_SEQUENCE, &items));
    }

    let duplicates = warnings as u64;
    metrics::counter!("fortitude_stow_instances_total", "outcome" => "stored")
        .increment(referenced.len() as u64 - duplicates);
    metrics::counter!("fortitude_stow_instances_total", "outcome" => "duplicate")
        .increment(duplicates);
    metrics::counter!("fortitude_stow_instances_total", "outcome" => "failed")
        .increment((failed.len() + other_failures.len()) as u64);

    let status = if referenced.is_empty() {
        if failed.is_empty() && other_failures.is_empty() {
            StatusCode::BAD_REQUEST
        } else {
            StatusCode::CONFLICT
        }
    } else if failed.is_empty() && other_failures.is_empty() && warnings == 0 {
        StatusCode::OK
    } else {
        StatusCode::ACCEPTED
    };
    let body = format!("{{{}}}", members.join(","));
    let mut response = (status, body).into_response();
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(DICOM_JSON));
    response
}
