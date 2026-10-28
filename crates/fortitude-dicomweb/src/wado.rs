//! WADO-RS: Retrieve Transaction (PS3.18 §10.4).

use std::io;
use std::sync::Arc;

use async_stream::try_stream;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use bytes::Bytes;
use fortitude_store::Store;
use fortitude_store::pixel::Range;
use fortitude_store::retrieve::{self, BulkData, PixelSource, Scope, StoredInstance};
use futures::{StreamExt, stream};
use tokio_util::io::ReaderStream;

use crate::negotiate::{self, InstanceFormat, Representation};
use crate::{BaseUrl, DICOM_JSON, DicomWeb, error, media};

/// Instance files up to this size are read in one operation, larger ones are streamed.
const WHOLE_FILE_MAX: u64 = 4 << 20;
/// Instance files read ahead of the one being sent.
const READ_AHEAD: usize = 4;

fn representations(headers: &HeaderMap) -> Vec<Representation> {
    negotiate::representations(&media::accept(
        headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()),
    ))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "request failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

fn not_acceptable() -> Response {
    error(
        StatusCode::NOT_ACCEPTABLE,
        "none of the accepted media types can be provided",
    )
}

pub async fn study(
    State(s): State<DicomWeb>,
    Path(study): Path<String>,
    headers: HeaderMap,
) -> Response {
    instances(&s.store, Scope::study(&study), &headers).await
}

pub async fn series(
    State(s): State<DicomWeb>,
    Path((study, series)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    instances(&s.store, Scope::series(&study, &series), &headers).await
}

pub async fn instance(
    State(s): State<DicomWeb>,
    Path((study, series, instance)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    instances(
        &s.store,
        Scope::instance(&study, &series, &instance),
        &headers,
    )
    .await
}

pub async fn study_metadata(
    State(s): State<DicomWeb>,
    BaseUrl(base): BaseUrl,
    Path(study): Path<String>,
    headers: HeaderMap,
) -> Response {
    metadata(&s.store, Scope::study(&study), base, &headers).await
}

pub async fn series_metadata(
    State(s): State<DicomWeb>,
    BaseUrl(base): BaseUrl,
    Path((study, series)): Path<(String, String)>,
    headers: HeaderMap,
) -> Response {
    metadata(&s.store, Scope::series(&study, &series), base, &headers).await
}

pub async fn instance_metadata(
    State(s): State<DicomWeb>,
    BaseUrl(base): BaseUrl,
    Path((study, series, instance)): Path<(String, String, String)>,
    headers: HeaderMap,
) -> Response {
    metadata(
        &s.store,
        Scope::instance(&study, &series, &instance),
        base,
        &headers,
    )
    .await
}

async fn metadata(store: &Arc<Store>, scope: Scope, base: String, headers: &HeaderMap) -> Response {
    if !negotiate::json(&representations(headers)) {
        return not_acceptable();
    }
    match store.metadata(scope, base).await {
        Ok(Some(body)) => ([(header::CONTENT_TYPE, DICOM_JSON)], body).into_response(),
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

async fn instances(store: &Arc<Store>, scope: Scope, headers: &HeaderMap) -> Response {
    let instances = match store.instances(scope).await {
        Ok(found) if found.is_empty() => return not_found(),
        Ok(found) => found,
        Err(e) => return internal(e),
    };
    let stored: Vec<&str> = instances
        .iter()
        .map(|i| i.transfer_syntax.as_str())
        .collect();
    match negotiate::instances(&representations(headers), &stored, instances.len()) {
        None => not_acceptable(),
        Some(InstanceFormat::Single) => single_instance(&instances[0]).await,
        Some(InstanceFormat::Multipart) => multipart_instances(instances),
    }
}

async fn single_instance(instance: &StoredInstance) -> Response {
    let file = match tokio::fs::File::open(&instance.path).await {
        Ok(file) => file,
        Err(e) => return internal(e),
    };
    let content_type = format!(
        "application/dicom; transfer-syntax={}",
        instance.transfer_syntax
    );
    let mut response =
        Body::from_stream(ReaderStream::with_capacity(file, 256 * 1024)).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(&content_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(instance.size));
    response
}

fn boundary() -> String {
    format!("fortitude-{}", uuid::Uuid::new_v4().simple())
}

fn multipart_content_type(part_type: &str, boundary: &str) -> HeaderValue {
    header_value(&format!(
        "multipart/related; type=\"{part_type}\"; boundary={boundary}"
    ))
}

fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

enum PartBody {
    Whole(Bytes),
    File(std::path::PathBuf),
}

/// Stream instance files as `multipart/related; type="application/dicom"`.
fn multipart_instances(instances: Vec<StoredInstance>) -> Response {
    let boundary = boundary();
    let headers: Vec<String> = instances
        .iter()
        .map(|i| {
            format!(
                "--{boundary}\r\nContent-Type: application/dicom; transfer-syntax={}\r\n\r\n",
                i.transfer_syntax
            )
        })
        .collect();
    let closing = format!("--{boundary}--\r\n");
    let length = headers.iter().map(|h| h.len() as u64 + 2).sum::<u64>()
        + instances.iter().map(|i| i.size).sum::<u64>()
        + closing.len() as u64;

    let parts = stream::iter(instances.into_iter().zip(headers))
        .map(|(instance, header)| async move {
            let body = if instance.size <= WHOLE_FILE_MAX {
                PartBody::Whole(Bytes::from(tokio::fs::read(&instance.path).await?))
            } else {
                PartBody::File(instance.path)
            };
            Ok::<_, io::Error>((header, body))
        })
        .buffered(READ_AHEAD);
    let body = try_stream! {
        futures::pin_mut!(parts);
        while let Some(part) = parts.next().await {
            let (header, body) = part?;
            yield Bytes::from(header);
            match body {
                PartBody::Whole(bytes) => yield bytes,
                PartBody::File(path) => {
                    let file = tokio::fs::File::open(&path).await?;
                    let mut chunks = ReaderStream::with_capacity(file, 512 * 1024);
                    while let Some(chunk) = chunks.next().await {
                        yield chunk?;
                    }
                }
            }
            yield Bytes::from_static(b"\r\n");
        }
        yield Bytes::from(closing);
    };
    let body: std::pin::Pin<Box<dyn futures::Stream<Item = io::Result<Bytes>> + Send>> =
        Box::pin(body);
    let mut response = Body::from_stream(body).into_response();
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        multipart_content_type("application/dicom", &boundary),
    );
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    response
}

pub async fn frames(
    State(s): State<DicomWeb>,
    Path((study, series, instance, list)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    let Some(numbers) = parse_frame_list(&list) else {
        return error(StatusCode::BAD_REQUEST, "invalid frame list");
    };
    let source = match s
        .store
        .pixels(Scope::instance(&study, &series, &instance))
        .await
    {
        Ok(Some(source)) => source,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let Some(layout) = &source.layout else {
        return error(StatusCode::NOT_FOUND, "instance has no pixel data");
    };
    let mut ranges = Vec::with_capacity(numbers.len());
    for number in numbers {
        match layout.frame(number - 1) {
            Some(frame) => ranges.push(frame),
            None if layout.frame_count() == 0 => {
                return error(
                    StatusCode::NOT_FOUND,
                    "frames of this instance cannot be located",
                );
            }
            None => {
                return error(
                    StatusCode::NOT_FOUND,
                    format!("frame {number} does not exist"),
                );
            }
        }
    }
    pixel_parts(&source, ranges, &headers).await
}

/// Frame numbers (1-based) of a comma-separated list.
fn parse_frame_list(list: &str) -> Option<Vec<usize>> {
    let numbers: Option<Vec<usize>> = list
        .split(',')
        .map(|n| n.trim().parse().ok().filter(|n| *n > 0))
        .collect();
    numbers.filter(|n| !n.is_empty())
}

/// Return pixel data parts (one per entry of `parts`) read from the stored file.
async fn pixel_parts(
    source: &PixelSource,
    parts: Vec<Vec<Range>>,
    headers: &HeaderMap,
) -> Response {
    let Some((multipart, content_type)) =
        negotiate::pixels(&representations(headers), &source.transfer_syntax)
    else {
        return not_acceptable();
    };
    if !multipart && parts.len() != 1 {
        return not_acceptable();
    }
    let path = source.path.clone();
    let read = tokio::task::spawn_blocking(move || {
        parts
            .iter()
            .map(|ranges| retrieve::read_ranges(&path, ranges))
            .collect::<io::Result<Vec<_>>>()
    })
    .await;
    let data = match read {
        Ok(Ok(data)) => data,
        Ok(Err(e)) => return internal(e),
        Err(e) => return internal(e),
    };
    if !multipart {
        let body = data.into_iter().next().unwrap_or_default();
        return ([(header::CONTENT_TYPE, header_value(&content_type))], body).into_response();
    }
    multipart_bytes(&content_type, data)
}

fn multipart_bytes(content_type: &str, parts: Vec<Vec<u8>>) -> Response {
    let boundary = boundary();
    let part_header = format!("--{boundary}\r\nContent-Type: {content_type}\r\n\r\n");
    let mut body = Vec::with_capacity(
        parts
            .iter()
            .map(|p| p.len() + part_header.len() + 2)
            .sum::<usize>()
            + 64,
    );
    for part in parts {
        body.extend_from_slice(part_header.as_bytes());
        body.extend_from_slice(&part);
        body.extend_from_slice(b"\r\n");
    }
    body.extend_from_slice(format!("--{boundary}--\r\n").as_bytes());
    let part_type = content_type.split(';').next().unwrap_or(content_type);
    (
        [(
            header::CONTENT_TYPE,
            multipart_content_type(part_type, &boundary),
        )],
        body,
    )
        .into_response()
}

pub async fn bulk(
    State(s): State<DicomWeb>,
    Path((study, series, instance, path)): Path<(String, String, String, String)>,
    headers: HeaderMap,
) -> Response {
    match s
        .store
        .bulk_data(Scope::instance(&study, &series, &instance), &path)
        .await
    {
        Ok(Some(BulkData::Pixels(source))) => {
            let Some(layout) = &source.layout else {
                return not_found();
            };
            let parts = if layout.is_encapsulated() {
                (0..layout.frame_count())
                    .filter_map(|i| layout.frame(i))
                    .collect()
            } else {
                vec![layout.all()]
            };
            if parts.is_empty() {
                return error(
                    StatusCode::NOT_FOUND,
                    "frames of this instance cannot be located",
                );
            }
            pixel_parts(&source, parts, &headers).await
        }
        Ok(Some(BulkData::Bytes(bytes))) => {
            let accepted = representations(&headers).into_iter().find_map(|r| {
                match (r.multipart, r.media.as_str()) {
                    (true, "application/octet-stream" | "*/*") => Some(true),
                    (false, "application/octet-stream") => Some(false),
                    (false, "*/*" | "application/*") => Some(true),
                    _ => None,
                }
            });
            match accepted {
                Some(true) => multipart_bytes("application/octet-stream", vec![bytes]),
                Some(false) => {
                    ([(header::CONTENT_TYPE, "application/octet-stream")], bytes).into_response()
                }
                None => not_acceptable(),
            }
        }
        Ok(None) => not_found(),
        Err(e) => internal(e),
    }
}

#[cfg(test)]
mod tests {
    use super::parse_frame_list;

    #[test]
    fn frame_lists() {
        assert_eq!(parse_frame_list("1"), Some(vec![1]));
        assert_eq!(parse_frame_list("3, 1,2"), Some(vec![3, 1, 2]));
        assert_eq!(parse_frame_list("0"), None);
        assert_eq!(parse_frame_list("1,a"), None);
        assert_eq!(parse_frame_list(""), None);
    }
}
