//! Administration API.

use std::sync::Arc;

use axum::extract::{Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use fortitude_store::Store;
use fortitude_store::retrieve::Scope;
use serde::Deserialize;

use crate::integrity::Checker;

#[derive(Clone)]
struct Admin {
    store: Arc<Store>,
    integrity: Arc<Checker>,
}

pub fn router(store: Arc<Store>, integrity: Arc<Checker>) -> Router {
    Router::new()
        .route("/statistics", get(statistics))
        .route("/integrity", get(integrity_state).post(start_integrity))
        .with_state(Admin { store, integrity })
}

async fn statistics(State(admin): State<Admin>) -> Response {
    match admin.store.statistics().await {
        Ok(statistics) => Json(statistics).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "cannot compute statistics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}

/// Whether a check is running, and the report of the last one.
async fn integrity_state(State(admin): State<Admin>) -> Response {
    Json(admin.integrity.state()).into_response()
}

#[derive(Debug, Deserialize)]
struct StartParams {
    /// Overrides `integrity.remove_orphans` (`false` for a dry run).
    remove_orphans: Option<bool>,
    /// Check only this study, or this series of it.
    study: Option<String>,
    series: Option<String>,
}

/// Start a full check in the background: `202 Accepted`, or `409 Conflict` when one is
/// running. With `study` (and `series`), check them now and answer with the report: `404`
/// when they are not stored.
async fn start_integrity(
    State(admin): State<Admin>,
    Query(params): Query<StartParams>,
) -> Response {
    match (params.study, params.series) {
        (Some(study), series) => {
            let scope = match &series {
                Some(series) => Scope::series(&study, series),
                None => Scope::study(&study),
            };
            return match admin.store.check_scope_integrity(scope).await {
                Ok(Some(report)) => Json(report).into_response(),
                Ok(None) => StatusCode::NOT_FOUND.into_response(),
                Err(e) => {
                    tracing::error!(error = %e, "cannot check integrity");
                    StatusCode::INTERNAL_SERVER_ERROR.into_response()
                }
            };
        }
        (None, Some(_)) => {
            return (StatusCode::BAD_REQUEST, "series requires study").into_response();
        }
        (None, None) => {}
    }
    let status = if admin.integrity.start(params.remove_orphans) {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(admin.integrity.state())).into_response()
}
