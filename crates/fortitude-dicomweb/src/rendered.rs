//! WADO-RS rendered resources and thumbnails (PS3.18 §10.4.1.1.3, §10.4.1.1.4).

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fortitude_store::imaging::RenderOptions;
use fortitude_store::retrieve::Scope;

use crate::wado::{
    header_value, imaging_error, internal, multipart_bytes, not_acceptable, not_found,
    parse_frame_list, representations,
};
use crate::{DicomWeb, error, negotiate};

/// Thumbnails fit in this many pixels unless a viewport is requested.
const THUMBNAIL_SIZE: u32 = 128;
const DEFAULT_QUALITY: u8 = 90;

/// Query parameters of rendered resources.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RenderParams {
    pub window: Option<(f64, f64)>,
    pub viewport: Option<(u32, u32)>,
    pub quality: Option<u8>,
}

pub(crate) fn parse_params(raw: Option<&str>) -> Result<RenderParams, String> {
    let mut params = RenderParams::default();
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let numbers = |n: usize| -> Option<Vec<f64>> {
            let values: Option<Vec<f64>> = value
                .split(',')
                .take(n)
                .map(|v| v.trim().parse().ok())
                .collect();
            values.filter(|v| v.len() == n)
        };
        match key.as_ref() {
            "window" => {
                let v = numbers(2).ok_or("window must be center,width[,function]")?;
                params.window = Some((v[0], v[1]));
            }
            "viewport" => {
                let v = numbers(2)
                    .filter(|v| v[0] >= 1.0 && v[1] >= 1.0)
                    .ok_or("viewport must be width,height")?;
                params.viewport = Some((v[0] as u32, v[1] as u32));
            }
            "quality" => {
                let q: u8 = value
                    .parse()
                    .ok()
                    .filter(|q| (1..=100).contains(q))
                    .ok_or("quality must be 1 to 100")?;
                params.quality = Some(q);
            }
            // Annotations and character sets do not apply to rendered images.
            _ => {}
        }
    }
    Ok(params)
}

pub async fn instance(
    State(s): State<DicomWeb>,
    Path((study, series, instance)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    render(
        &s,
        Scope::instance(&study, &series, &instance),
        vec![0],
        query,
        &headers,
        None,
    )
    .await
}

pub async fn frames(
    State(s): State<DicomWeb>,
    Path((study, series, instance, list)): Path<(String, String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let Some(numbers) = parse_frame_list(&list) else {
        return error(StatusCode::BAD_REQUEST, "invalid frame list");
    };
    let frames = numbers.iter().map(|n| n - 1).collect();
    render(
        &s,
        Scope::instance(&study, &series, &instance),
        frames,
        query,
        &headers,
        None,
    )
    .await
}

pub async fn study_thumbnail(
    State(s): State<DicomWeb>,
    Path(study): Path<String>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    thumbnail(&s, Scope::study(&study), 0, query, &headers).await
}

pub async fn series_thumbnail(
    State(s): State<DicomWeb>,
    Path((study, series)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    thumbnail(&s, Scope::series(&study, &series), 0, query, &headers).await
}

pub async fn instance_thumbnail(
    State(s): State<DicomWeb>,
    Path((study, series, instance)): Path<(String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    thumbnail(
        &s,
        Scope::instance(&study, &series, &instance),
        0,
        query,
        &headers,
    )
    .await
}

pub async fn frame_thumbnail(
    State(s): State<DicomWeb>,
    Path((study, series, instance, frame)): Path<(String, String, String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    match parse_frame_list(&frame).as_deref() {
        Some([number]) => {
            thumbnail(
                &s,
                Scope::instance(&study, &series, &instance),
                number - 1,
                query,
                &headers,
            )
            .await
        }
        _ => error(StatusCode::BAD_REQUEST, "a thumbnail is for a single frame"),
    }
}

async fn thumbnail(
    s: &DicomWeb,
    scope: Scope,
    frame: usize,
    query: Option<String>,
    headers: &HeaderMap,
) -> Response {
    let instance = match s.store.representative(scope).await {
        Ok(Some(instance)) => instance,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    render(
        s,
        instance,
        vec![frame],
        query,
        headers,
        Some((THUMBNAIL_SIZE, THUMBNAIL_SIZE)),
    )
    .await
}

/// Render `frames` of an instance: one image, or a multipart body for several frames.
async fn render(
    s: &DicomWeb,
    scope: Scope,
    frames: Vec<usize>,
    query: Option<String>,
    headers: &HeaderMap,
    default_viewport: Option<(u32, u32)>,
) -> Response {
    let params = match parse_params(query.as_deref()) {
        Ok(params) => params,
        Err(message) => return error(StatusCode::BAD_REQUEST, message),
    };
    let quality = params.quality.unwrap_or(DEFAULT_QUALITY);
    let Some((_, format)) = negotiate::rendered(&representations(headers), quality) else {
        return not_acceptable();
    };
    let mut images = Vec::with_capacity(frames.len());
    for frame in frames {
        let options = RenderOptions {
            frame,
            window: params.window,
            viewport: params.viewport.or(default_viewport),
            format,
        };
        match s.store.render(scope.clone(), options).await {
            Ok(image) => images.push(image),
            Err(e) => return imaging_error(e),
        }
    }
    if images.len() == 1 {
        let image = images.pop().unwrap_or_default();
        return (
            [(header::CONTENT_TYPE, header_value(format.media_type()))],
            image,
        )
            .into_response();
    }
    multipart_bytes(format.media_type(), images)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_render_parameters() {
        assert_eq!(
            parse_params(Some(
                "window=40,400,linear&viewport=64,32&quality=70&annotation=patient"
            )),
            Ok(RenderParams {
                window: Some((40.0, 400.0)),
                viewport: Some((64, 32)),
                quality: Some(70)
            })
        );
        assert!(parse_params(Some("window=40")).is_err());
        assert!(parse_params(Some("viewport=0,10")).is_err());
        assert!(parse_params(Some("quality=101")).is_err());
        assert_eq!(parse_params(None), Ok(RenderParams::default()));
    }
}
