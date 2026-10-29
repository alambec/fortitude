//! Web explorer: a static single-page application over the DICOMweb services.
//!
//! The assets are embedded in the binary. Besides them, the only server-side code is a
//! dictionary lookup, so that the page can name attributes and SOP classes.

use axum::Router;
use axum::extract::RawQuery;
use axum::http::header;
use axum::response::{IntoResponse, Json, Redirect, Response};
use axum::routing::get;
use dicom_core::Tag;
use dicom_core::dictionary::{DataDictionary, DataDictionaryEntry, UidDictionary};
use dicom_dictionary_std::{StandardDataDictionary, StandardSopClassDictionary};
use serde_json::{Map, Value};

const INDEX: &str = include_str!("../ui/index.html");
const SCRIPT: &str = include_str!("../ui/app.js");
const STYLE: &str = include_str!("../ui/style.css");

/// At most this many names per lookup.
const MAX_NAMES: usize = 2048;

/// Routes of the explorer, at `/ui`; `/` redirects to it.
pub fn router(dicomweb_root: &str, wado_root: &str) -> Router {
    let index = INDEX
        .replace("{{DICOMWEB_ROOT}}", &escape(dicomweb_root))
        .replace("{{WADO_ROOT}}", &escape(wado_root));
    Router::new()
        .route("/", get(|| async { Redirect::temporary("/ui/") }))
        .route("/ui", get(|| async { Redirect::permanent("/ui/") }))
        .route(
            "/ui/",
            get(move || async move { asset("text/html", index) }),
        )
        .route(
            "/ui/app.js",
            get(|| async { asset("text/javascript", SCRIPT) }),
        )
        .route("/ui/style.css", get(|| async { asset("text/css", STYLE) }))
        .route("/ui/api/names", get(names))
}

fn asset(content_type: &'static str, body: impl Into<String>) -> Response {
    (
        [
            (
                header::CONTENT_TYPE,
                format!("{content_type}; charset=utf-8"),
            ),
            (header::CACHE_CONTROL, "no-cache".to_owned()),
        ],
        body.into(),
    )
        .into_response()
}

/// `?tags=00100010,00080060&uids=1.2.840.10008.5.1.4.1.1.2`: keywords of attributes and
/// names of SOP classes, for those the standard dictionary knows.
async fn names(RawQuery(raw): RawQuery) -> Json<Value> {
    let mut tags = Map::new();
    let mut uids = Map::new();
    for (key, value) in form_urlencoded::parse(raw.unwrap_or_default().as_bytes()) {
        let values = value.split(',').map(str::trim).filter(|v| !v.is_empty());
        match key.as_ref() {
            "tags" => {
                for id in values.take(MAX_NAMES) {
                    let name = u32::from_str_radix(id, 16)
                        .ok()
                        .filter(|_| id.len() == 8)
                        .and_then(|v| {
                            StandardDataDictionary.by_tag(Tag((v >> 16) as u16, v as u16))
                        });
                    if let Some(entry) = name {
                        tags.insert(id.to_owned(), entry.alias().into());
                    }
                }
            }
            "uids" => {
                for uid in values.take(MAX_NAMES) {
                    if let Some(entry) = StandardSopClassDictionary.by_uid(uid) {
                        uids.insert(uid.to_owned(), entry.name.into());
                    }
                }
            }
            _ => {}
        }
    }
    Json(serde_json::json!({ "tags": tags, "uids": uids }))
}

/// For an HTML attribute value.
fn escape(value: &str) -> String {
    value
        .replace('&', "&amp;")
        .replace('"', "&quot;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}
