//! WADO-URI: Retrieve DICOM Objects using the URI service (PS3.18 §9).

use std::collections::HashMap;

use axum::extract::{RawQuery, State};
use axum::http::{StatusCode, header};
use axum::response::{IntoResponse, Response};
use fortitude_store::Store;
use fortitude_store::imaging::{ImageFormat, RenderOptions, can_transcode_to};
use fortitude_store::retrieve::Scope;

use crate::wado::{
    header_value, imaging_error, internal, not_acceptable, not_found, single_instance,
};
use crate::{DicomWeb, error};

enum Wanted {
    Dicom,
    Image(ImageFormat),
}

pub async fn retrieve(State(s): State<DicomWeb>, RawQuery(query): RawQuery) -> Response {
    let params: HashMap<String, String> =
        form_urlencoded::parse(query.unwrap_or_default().as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
    let param = |name: &str| {
        params
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    };
    if param("requestType") != Some("WADO") {
        return error(StatusCode::BAD_REQUEST, "requestType must be WADO");
    }
    let (Some(study), Some(series), Some(object)) =
        (param("studyUID"), param("seriesUID"), param("objectUID"))
    else {
        return error(
            StatusCode::BAD_REQUEST,
            "studyUID, seriesUID and objectUID are required",
        );
    };
    if param("anonymize") == Some("yes") {
        return error(StatusCode::BAD_REQUEST, "anonymization is not supported");
    }
    let scope = Scope::instance(study, series, object);
    let quality = match param("imageQuality").map(str::parse::<u8>) {
        None => 90,
        Some(Ok(q)) if (1..=100).contains(&q) => q,
        Some(_) => return error(StatusCode::BAD_REQUEST, "imageQuality must be 1 to 100"),
    };

    let wanted = match param("contentType") {
        Some(types) => match types.split(',').map(str::trim).find_map(|t| match t {
            "application/dicom" => Some(Wanted::Dicom),
            "image/jpeg" | "image/*" | "*/*" => Some(Wanted::Image(ImageFormat::Jpeg { quality })),
            "image/png" => Some(Wanted::Image(ImageFormat::Png)),
            _ => None,
        }) {
            Some(wanted) => wanted,
            None => return not_acceptable(),
        },
        // Default: JPEG for images, the DICOM object for anything else.
        None => match s.store.pixels(scope.clone()).await {
            Ok(Some(source)) if source.layout.is_some() => {
                Wanted::Image(ImageFormat::Jpeg { quality })
            }
            Ok(Some(_)) => Wanted::Dicom,
            Ok(None) => return not_found(),
            Err(e) => return internal(e),
        },
    };

    match wanted {
        Wanted::Dicom => {
            let instance = match s.store.instances(scope).await {
                Ok(mut found) if !found.is_empty() => found.swap_remove(0),
                Ok(_) => return not_found(),
                Err(e) => return internal(e),
            };
            match param("transferSyntax") {
                Some(ts) if ts != instance.transfer_syntax => {
                    if !can_transcode_to(ts) {
                        return not_acceptable();
                    }
                    match Store::transcoded(&instance.path, ts).await {
                        Ok(bytes) => {
                            ([(header::CONTENT_TYPE, "application/dicom")], bytes).into_response()
                        }
                        Err(e) => imaging_error(e),
                    }
                }
                _ => {
                    let mut response = single_instance(&instance).await;
                    response
                        .headers_mut()
                        .insert(header::CONTENT_TYPE, header_value("application/dicom"));
                    response
                }
            }
        }
        Wanted::Image(format) => {
            let frame = match param("frameNumber").map(str::parse::<usize>) {
                None => 0,
                Some(Ok(n)) if n >= 1 => n - 1,
                Some(_) => {
                    return error(
                        StatusCode::BAD_REQUEST,
                        "frameNumber must be a positive integer",
                    );
                }
            };
            let dimension = |name| {
                param(name)
                    .and_then(|v| v.parse::<u32>().ok())
                    .filter(|v| *v > 0)
            };
            let viewport = match (dimension("columns"), dimension("rows")) {
                (None, None) => None,
                (columns, rows) => {
                    Some((columns.or(rows).unwrap_or(1), rows.or(columns).unwrap_or(1)))
                }
            };
            let window = match (param("windowCenter"), param("windowWidth")) {
                (Some(center), Some(width)) => match (center.parse(), width.parse()) {
                    (Ok(center), Ok(width)) => Some((center, width)),
                    _ => {
                        return error(
                            StatusCode::BAD_REQUEST,
                            "invalid windowCenter or windowWidth",
                        );
                    }
                },
                _ => None,
            };
            match s
                .store
                .render(
                    scope,
                    RenderOptions {
                        frame,
                        window,
                        viewport,
                        format,
                    },
                )
                .await
            {
                Ok(image) => ([(header::CONTENT_TYPE, format.media_type())], image).into_response(),
                Err(e) => imaging_error(e),
            }
        }
    }
}
