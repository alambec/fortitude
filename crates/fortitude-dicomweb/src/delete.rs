//! Deletion of studies, series and instances (`DELETE` on their WADO-RS resources).
//!
//! PS3.18 defines no deletion transaction; this follows the convention of other archives.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use fortitude_store::retrieve::Scope;

use crate::DicomWeb;
use crate::wado::{internal, not_found};

pub async fn study(State(s): State<DicomWeb>, Path(study): Path<String>) -> Response {
    delete(&s, Scope::study(&study)).await
}

pub async fn series(
    State(s): State<DicomWeb>,
    Path((study, series)): Path<(String, String)>,
) -> Response {
    delete(&s, Scope::series(&study, &series)).await
}

pub async fn instance(
    State(s): State<DicomWeb>,
    Path((study, series, instance)): Path<(String, String, String)>,
) -> Response {
    delete(&s, Scope::instance(&study, &series, &instance)).await
}

async fn delete(s: &DicomWeb, scope: Scope) -> Response {
    match s.store.delete(scope.clone()).await {
        Ok(0) => not_found(),
        Ok(count) => {
            tracing::info!(study = %scope.study, series = ?scope.series, instance = ?scope.instance, count, "deleted");
            StatusCode::NO_CONTENT.into_response()
        }
        Err(e) => internal(e),
    }
}
