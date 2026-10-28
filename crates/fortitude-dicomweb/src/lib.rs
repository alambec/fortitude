//! DICOMweb HTTP services: STOW-RS, WADO-RS, QIDO-RS and WADO-URI.

mod delete;
pub mod media;
pub mod multipart;
pub mod negotiate;
mod qido;
mod rendered;
mod stow;
mod wado;
mod wado_uri;

use std::sync::Arc;

use axum::Router;
use axum::extract::FromRequestParts;
use axum::http::request::Parts;
use axum::http::{HeaderMap, HeaderValue, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use fortitude_store::Store;

pub const DICOM_JSON: &str = "application/dicom+json";

#[derive(Debug, Clone)]
pub struct Config {
    /// Path under which the services are mounted (e.g. `/dicom-web`).
    pub root: String,
    /// Public origin (e.g. `https://pacs.example.org`) used in returned URLs; derived from
    /// the request (`Host`, `X-Forwarded-*`) when unset.
    pub public_url: Option<String>,
    /// Instances of one STOW-RS request stored concurrently.
    pub stow_concurrency: usize,
    /// Largest accepted instance, in bytes.
    pub max_instance_size: usize,
    /// Maximum number of QIDO-RS results per request, 0 for no limit.
    pub qido_max_results: usize,
}

#[derive(Clone)]
pub struct DicomWeb {
    pub store: Arc<Store>,
    pub config: Arc<Config>,
}

/// Routes of the DICOMweb services, to be nested under [`Config::root`].
pub fn router(state: DicomWeb) -> Router {
    const INSTANCE: &str = "/studies/{study}/series/{series}/instances/{instance}";
    Router::new()
        .route("/studies", get(qido::studies).post(stow::store_instances))
        .route("/series", get(qido::series))
        .route("/instances", get(qido::instances))
        .route("/studies/{study}/series", get(qido::study_series))
        .route("/studies/{study}/instances", get(qido::study_instances))
        .route(
            "/studies/{study}/series/{series}/instances",
            get(qido::series_instances),
        )
        .route(
            "/studies/{study}",
            get(wado::study)
                .post(stow::store_study_instances)
                .delete(delete::study),
        )
        .route("/studies/{study}/metadata", get(wado::study_metadata))
        .route(
            "/studies/{study}/series/{series}",
            get(wado::series).delete(delete::series),
        )
        .route(
            "/studies/{study}/series/{series}/metadata",
            get(wado::series_metadata),
        )
        .route(INSTANCE, get(wado::instance).delete(delete::instance))
        .route(
            &format!("{INSTANCE}/metadata"),
            get(wado::instance_metadata),
        )
        .route(&format!("{INSTANCE}/frames/{{frames}}"), get(wado::frames))
        .route(&format!("{INSTANCE}/bulk/{{*path}}"), get(wado::bulk))
        .route(&format!("{INSTANCE}/rendered"), get(rendered::instance))
        .route(
            &format!("{INSTANCE}/frames/{{frames}}/rendered"),
            get(rendered::frames),
        )
        .route("/studies/{study}/thumbnail", get(rendered::study_thumbnail))
        .route(
            "/studies/{study}/series/{series}/thumbnail",
            get(rendered::series_thumbnail),
        )
        .route(
            &format!("{INSTANCE}/thumbnail"),
            get(rendered::instance_thumbnail),
        )
        .route(
            &format!("{INSTANCE}/frames/{{frames}}/thumbnail"),
            get(rendered::frame_thumbnail),
        )
        .with_state(state)
}

/// The WADO-URI service, at `path` (e.g. `/wado`) of the server root.
pub fn wado_uri_router(path: &str, state: DicomWeb) -> Router {
    Router::new()
        .route(path, get(wado_uri::retrieve))
        .with_state(state)
}

/// DICOMweb base URL of the current request, used in RetrieveURL and BulkDataURI.
pub struct BaseUrl(pub String);

impl FromRequestParts<DicomWeb> for BaseUrl {
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(
        parts: &mut Parts,
        state: &DicomWeb,
    ) -> Result<Self, Self::Rejection> {
        Ok(BaseUrl(base_url(&parts.headers, &state.config)))
    }
}

fn base_url(headers: &HeaderMap, config: &Config) -> String {
    let root = config.root.trim_end_matches('/');
    if let Some(public) = &config.public_url {
        return format!("{}{root}", public.trim_end_matches('/'));
    }
    let header = |name: &str| {
        headers
            .get(name)
            .and_then(|v| v.to_str().ok())
            .map(|v| v.split(',').next().unwrap_or(v).trim().to_owned())
    };
    let proto = header("x-forwarded-proto").unwrap_or_else(|| "http".into());
    let host = header("x-forwarded-host")
        .or_else(|| header(header::HOST.as_str()))
        .unwrap_or_else(|| "localhost".into());
    format!("{proto}://{host}{root}")
}

/// An error response with a plain text reason.
pub(crate) fn error(status: StatusCode, message: impl Into<String>) -> Response {
    let mut response = (status, message.into()).into_response();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain; charset=utf-8"),
    );
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(public_url: Option<&str>) -> Config {
        Config {
            root: "/dicom-web".into(),
            public_url: public_url.map(Into::into),
            stow_concurrency: 1,
            max_instance_size: 1,
            qido_max_results: 0,
        }
    }

    #[test]
    fn base_url_follows_proxies() {
        let mut headers = HeaderMap::new();
        headers.insert(header::HOST, HeaderValue::from_static("internal:8042"));
        assert_eq!(
            base_url(&headers, &config(None)),
            "http://internal:8042/dicom-web"
        );
        headers.insert("x-forwarded-proto", HeaderValue::from_static("https"));
        headers.insert(
            "x-forwarded-host",
            HeaderValue::from_static("pacs.example.org, proxy"),
        );
        assert_eq!(
            base_url(&headers, &config(None)),
            "https://pacs.example.org/dicom-web"
        );
        assert_eq!(
            base_url(&headers, &config(Some("https://x.org/"))),
            "https://x.org/dicom-web"
        );
    }
}
