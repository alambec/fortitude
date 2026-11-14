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
use fortitude_store::imaging::ImagingError;
use fortitude_store::retrieve::{BulkData, PixelSource, Scope, StoredInstance};
use futures::{StreamExt, stream};

use crate::negotiate::{
    self, DEFLATED_EXPLICIT_VR_LE, EXPLICIT_VR_LE, IMPLICIT_VR_LE, InstanceFormat, PixelPlan,
    Representation,
};
use crate::{BaseUrl, DICOM_JSON, DicomWeb, error, media};

/// Instance files up to this size are read in one operation, larger ones are streamed.
const WHOLE_FILE_MAX: u64 = 4 << 20;
/// Instance files read (or transcoded) ahead of the one being sent.
const READ_AHEAD: usize = 4;

pub(crate) fn representations(headers: &HeaderMap) -> Vec<Representation> {
    negotiate::representations(&media::accept(
        headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()),
    ))
}

pub(crate) fn internal(e: impl std::fmt::Display) -> Response {
    tracing::error!(error = %e, "request failed");
    error(StatusCode::INTERNAL_SERVER_ERROR, "internal error")
}

pub(crate) fn not_found() -> Response {
    error(StatusCode::NOT_FOUND, "not found")
}

pub(crate) fn not_acceptable() -> Response {
    error(
        StatusCode::NOT_ACCEPTABLE,
        "none of the accepted media types can be provided",
    )
}

pub(crate) fn imaging_error(e: ImagingError) -> Response {
    match e {
        ImagingError::NotFound => not_found(),
        e @ (ImagingError::NoPixelData | ImagingError::NoFrame(_)) => {
            error(StatusCode::NOT_FOUND, e.to_string())
        }
        ImagingError::Unsupported(message) => error(StatusCode::NOT_ACCEPTABLE, message),
        ImagingError::Index(e) => internal(e),
        ImagingError::Failed(e) => internal(e),
    }
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
        Some((InstanceFormat::Single, None)) => single_instance(store, &instances[0]).await,
        Some((InstanceFormat::Single, Some(ts))) => {
            match store.transcoded(&instances[0].path, &ts).await {
                Ok(bytes) => {
                    let content_type = format!("application/dicom; transfer-syntax={ts}");
                    ([(header::CONTENT_TYPE, header_value(&content_type))], bytes).into_response()
                }
                Err(e) => imaging_error(e),
            }
        }
        Some((InstanceFormat::Multipart, target)) => {
            multipart_instances(store, instances, target).await
        }
    }
}

pub(crate) async fn single_instance(store: &Store, instance: &StoredInstance) -> Response {
    let file = match store.files().stream(&instance.path).await {
        Ok(file) => file,
        Err(e) => return internal(e),
    };
    let content_type = format!(
        "application/dicom; transfer-syntax={}",
        instance.transfer_syntax
    );
    let mut response = Body::from_stream(file).into_response();
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, header_value(&content_type));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(instance.size));
    response
}

pub(crate) fn boundary() -> String {
    format!("fortitude-{}", uuid::Uuid::new_v4().simple())
}

pub(crate) fn multipart_content_type(part_type: &str, boundary: &str) -> HeaderValue {
    header_value(&format!(
        "multipart/related; type=\"{part_type}\"; boundary={boundary}"
    ))
}

pub(crate) fn header_value(value: &str) -> HeaderValue {
    HeaderValue::from_str(value)
        .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"))
}

enum PartBody {
    Whole(Bytes),
    /// Key of a file to stream.
    File(String),
}

/// Stream instances as `multipart/related; type="application/dicom"`, transcoding to
/// `target` those stored in another transfer syntax.
async fn multipart_instances(
    store: &Arc<Store>,
    instances: Vec<StoredInstance>,
    target: Option<String>,
) -> Response {
    let needs_transcoding =
        |i: &StoredInstance| target.as_deref().is_some_and(|ts| ts != i.transfer_syntax);
    // Transcode the first instance that needs it before answering, so that an unsupported
    // conversion is reported with a status rather than as a truncated body.
    let mut first_transcoded = None;
    if let (Some(ts), Some(index)) = (&target, instances.iter().position(needs_transcoding)) {
        match store.transcoded(&instances[index].path, ts).await {
            Ok(bytes) => first_transcoded = Some((index, Bytes::from(bytes))),
            Err(e) => return imaging_error(e),
        }
    }

    let boundary = boundary();
    let part_header = |ts: &str| {
        format!("--{boundary}\r\nContent-Type: application/dicom; transfer-syntax={ts}\r\n\r\n")
    };
    let closing = format!("--{boundary}--\r\n");
    let length = first_transcoded.is_none().then(|| {
        instances
            .iter()
            .map(|i| part_header(&i.transfer_syntax).len() as u64 + 2 + i.size)
            .sum::<u64>()
            + closing.len() as u64
    });

    let parts: Vec<_> = instances
        .into_iter()
        .enumerate()
        .map(|(index, instance)| {
            let transcode = target.clone().filter(|ts| *ts != instance.transfer_syntax);
            let header = part_header(transcode.as_deref().unwrap_or(&instance.transfer_syntax));
            let ready = match &first_transcoded {
                Some((i, bytes)) if *i == index => Some(bytes.clone()),
                _ => None,
            };
            (instance, header, transcode, ready)
        })
        .collect();
    let owned = store.clone();
    let parts = stream::iter(parts)
        .map(move |(instance, header, transcode, ready)| {
            let store = owned.clone();
            async move {
                let body = match (ready, transcode) {
                    (Some(bytes), _) => PartBody::Whole(bytes),
                    (None, Some(ts)) => PartBody::Whole(Bytes::from(
                        store
                            .transcoded(&instance.path, &ts)
                            .await
                            .map_err(io::Error::other)?,
                    )),
                    (None, None) if instance.size <= WHOLE_FILE_MAX => {
                        PartBody::Whole(store.files().read(&instance.path).await?)
                    }
                    (None, None) => PartBody::File(instance.path),
                };
                Ok::<_, io::Error>((header, body))
            }
        })
        .buffered(READ_AHEAD);
    let files = store.files().clone();
    let body = try_stream! {
        futures::pin_mut!(parts);
        while let Some(part) = parts.next().await {
            let (header, body) = part?;
            yield Bytes::from(header);
            match body {
                PartBody::Whole(bytes) => yield bytes,
                PartBody::File(path) => {
                    let mut chunks = files.stream(&path).await?;
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
    if let Some(length) = length {
        headers.insert(header::CONTENT_LENGTH, HeaderValue::from(length));
    }
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
    let scope = Scope::instance(&study, &series, &instance);
    let source = match s.store.pixels(scope.clone()).await {
        Ok(Some(source)) => source,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let Some(plan) = pixel_plan(&source, &headers) else {
        return not_acceptable();
    };
    if !plan.multipart && numbers.len() != 1 {
        return not_acceptable();
    }
    let indexes: Vec<usize> = numbers.iter().map(|n| n - 1).collect();
    // Native little endian frames that cannot be located by byte ranges (1-bit frames that do
    // not start on a byte) are the same once decoded.
    let unlocated = source.layout.as_ref().is_some_and(|layout| {
        !layout.is_encapsulated()
            && layout.frame_count() == 0
            && [EXPLICIT_VR_LE, IMPLICIT_VR_LE].contains(&source.transfer_syntax.as_str())
    });
    let data = if plan.decode || unlocated {
        match s.store.decoded_frames(&source, indexes).await {
            Ok(data) => data,
            Err(e) => return imaging_error(e),
        }
    } else {
        let Some(layout) = &source.layout else {
            // Pixel data of deflated data sets is not located at ingest: only decoded frames.
            if source.transfer_syntax == DEFLATED_EXPLICIT_VR_LE {
                return error(
                    StatusCode::NOT_FOUND,
                    "frames of this instance cannot be located",
                );
            }
            return imaging_error(ImagingError::NoPixelData);
        };
        let mut ranges = Vec::with_capacity(indexes.len());
        for index in indexes {
            match layout.frame(index) {
                Some(frame) => ranges.push(frame),
                None if layout.frame_count() == 0 => {
                    return error(
                        StatusCode::NOT_FOUND,
                        "frames of this instance cannot be located",
                    );
                }
                None => return imaging_error(ImagingError::NoFrame(index + 1)),
            }
        }
        match s.store.files().read_parts(&source.path, ranges).await {
            Ok(data) => data,
            Err(e) => return internal(e),
        }
    };
    pixel_response(&plan, data)
}

fn pixel_plan(source: &PixelSource, headers: &HeaderMap) -> Option<PixelPlan> {
    negotiate::pixels(&representations(headers), &source.transfer_syntax)
}

/// Frame numbers (1-based) of a comma-separated list.
pub(crate) fn parse_frame_list(list: &str) -> Option<Vec<usize>> {
    let numbers: Option<Vec<usize>> = list
        .split(',')
        .map(|n| n.trim().parse().ok().filter(|n| *n > 0))
        .collect();
    numbers.filter(|n| !n.is_empty())
}

fn pixel_response(plan: &PixelPlan, data: Vec<Vec<u8>>) -> Response {
    if plan.multipart {
        return multipart_bytes(&plan.content_type, data);
    }
    let body = data.into_iter().next().unwrap_or_default();
    (
        [(header::CONTENT_TYPE, header_value(&plan.content_type))],
        body,
    )
        .into_response()
}

pub(crate) fn multipart_bytes(content_type: &str, parts: Vec<Vec<u8>>) -> Response {
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
    let scope = Scope::instance(&study, &series, &instance);
    match s.store.bulk_data(scope.clone(), &path).await {
        Ok(Some(BulkData::Pixels(source))) => {
            let Some(layout) = &source.layout else {
                return not_found();
            };
            let Some(plan) = pixel_plan(&source, &headers) else {
                return not_acceptable();
            };
            let data = if plan.decode {
                let frames = (0..layout.frame_count().max(1)).collect();
                match s.store.decoded_frames(&source, frames).await {
                    Ok(frames) => vec![frames.concat()],
                    Err(e) => return imaging_error(e),
                }
            } else {
                let parts: Vec<_> = if layout.is_encapsulated() {
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
                match s.store.files().read_parts(&source.path, parts).await {
                    Ok(data) => data,
                    Err(e) => return internal(e),
                }
            };
            if !plan.multipart && data.len() != 1 {
                return not_acceptable();
            }
            pixel_response(&plan, data)
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
