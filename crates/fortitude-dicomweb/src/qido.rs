//! QIDO-RS: Search Transaction (PS3.18 §10.6).

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use fortitude_core::attrs::Level;
use fortitude_core::qido;
use fortitude_store::search::{Search, SearchError};

use crate::{BaseUrl, DICOM_JSON, DicomWeb, error, media, negotiate, wado::internal};

/// PS3.18 §8.3.4.6.2: the result set was truncated at the server's maximum.
const TRUNCATED_WARNING: &str = "299 fortitude: \"The number of results exceeded the maximum supported by the server. Additional results can be requested.\"";

pub async fn studies(
    state: State<DicomWeb>,
    base: BaseUrl,
    query: RawQuery,
    headers: HeaderMap,
) -> Response {
    search(state, base, query, headers, Level::Study, None, None).await
}

pub async fn series(
    state: State<DicomWeb>,
    base: BaseUrl,
    query: RawQuery,
    headers: HeaderMap,
) -> Response {
    search(state, base, query, headers, Level::Series, None, None).await
}

pub async fn instances(
    state: State<DicomWeb>,
    base: BaseUrl,
    query: RawQuery,
    headers: HeaderMap,
) -> Response {
    search(state, base, query, headers, Level::Instance, None, None).await
}

pub async fn study_series(
    state: State<DicomWeb>,
    base: BaseUrl,
    Path(study): Path<String>,
    query: RawQuery,
    headers: HeaderMap,
) -> Response {
    search(
        state,
        base,
        query,
        headers,
        Level::Series,
        Some(study),
        None,
    )
    .await
}

pub async fn study_instances(
    state: State<DicomWeb>,
    base: BaseUrl,
    Path(study): Path<String>,
    query: RawQuery,
    headers: HeaderMap,
) -> Response {
    search(
        state,
        base,
        query,
        headers,
        Level::Instance,
        Some(study),
        None,
    )
    .await
}

pub async fn series_instances(
    state: State<DicomWeb>,
    base: BaseUrl,
    Path((study, series)): Path<(String, String)>,
    query: RawQuery,
    headers: HeaderMap,
) -> Response {
    search(
        state,
        base,
        query,
        headers,
        Level::Instance,
        Some(study),
        Some(series),
    )
    .await
}

async fn search(
    State(state): State<DicomWeb>,
    BaseUrl(base): BaseUrl,
    RawQuery(raw): RawQuery,
    headers: HeaderMap,
    level: Level,
    study: Option<String>,
    series: Option<String>,
) -> Response {
    let accept = media::accept(headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()));
    if !negotiate::json(&negotiate::representations(&accept)) {
        return error(
            StatusCode::NOT_ACCEPTABLE,
            "only application/dicom+json is supported",
        );
    }
    let params: Vec<(String, String)> = form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    let query = match qido::parse(&params) {
        Ok(query) => query,
        Err(e) => return error(StatusCode::BAD_REQUEST, e.to_string()),
    };
    let search = Search {
        level,
        study,
        series,
        query,
        max_results: state.config.qido_max_results,
    };
    match state.store.search(search, base).await {
        Ok(result) => {
            let mut response = ([(header::CONTENT_TYPE, DICOM_JSON)], result.body).into_response();
            if result.truncated {
                response
                    .headers_mut()
                    .insert(header::WARNING, HeaderValue::from_static(TRUNCATED_WARNING));
            }
            response
        }
        Err(SearchError::Invalid(message)) => error(StatusCode::BAD_REQUEST, message),
        Err(SearchError::Index(e)) => internal(e),
    }
}
