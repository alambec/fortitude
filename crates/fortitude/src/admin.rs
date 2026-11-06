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
use crate::retention::Retention;

#[derive(Clone)]
pub struct Admin {
    pub store: Arc<Store>,
    pub integrity: Arc<Checker>,
    pub retention: Arc<Retention>,
}

pub fn router(admin: Admin) -> Router {
    Router::new()
        .route("/statistics", get(statistics))
        .route("/integrity", get(integrity_state).post(start_integrity))
        .route("/retention", get(retention_state).post(start_retention))
        .with_state(admin)
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
    /// Overrides `integrity.verify_checksums`; checks of a study or series verify them unless
    /// `false`.
    verify_checksums: Option<bool>,
    /// Check only this study, or this series of it.
    study: Option<String>,
    series: Option<String>,
}

/// Start a full check in the background: `202 Accepted`, or `409 Conflict` when one is
/// running. With `study` (and `series`), check them now, checksums included, and answer with
/// the report: `404` when they are not stored.
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
            let verify_checksums = params.verify_checksums.unwrap_or(true);
            return match admin
                .store
                .check_scope_integrity(scope, verify_checksums)
                .await
            {
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
    let status = if admin
        .integrity
        .start(params.remove_orphans, params.verify_checksums)
    {
        StatusCode::ACCEPTED
    } else {
        StatusCode::CONFLICT
    };
    (status, Json(admin.integrity.state())).into_response()
}

/// Whether a retention run is in progress, and the report of the last one.
async fn retention_state(State(admin): State<Admin>) -> Response {
    Json(admin.retention.state()).into_response()
}

#[derive(Debug, Deserialize)]
struct RetentionParams {
    /// Only list the studies that would be deleted.
    #[serde(default)]
    dry_run: bool,
    /// Overrides `retention.max_age_days`, in a dry run only.
    max_age_days: Option<u64>,
}

/// Start a retention run in the background: `202 Accepted`, `409 Conflict` when one is
/// running, `400` without a maximum age.
async fn start_retention(
    State(admin): State<Admin>,
    Query(params): Query<RetentionParams>,
) -> Response {
    if params.max_age_days.is_some() && !params.dry_run {
        return (
            StatusCode::BAD_REQUEST,
            "max_age_days only applies to dry runs: deletions use retention.max_age_days",
        )
            .into_response();
    }
    let status = match admin.retention.start(params.dry_run, params.max_age_days) {
        Ok(true) => StatusCode::ACCEPTED,
        Ok(false) => StatusCode::CONFLICT,
        Err(message) => return (StatusCode::BAD_REQUEST, message).into_response(),
    };
    (status, Json(admin.retention.state())).into_response()
}
