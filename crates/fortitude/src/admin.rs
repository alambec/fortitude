//! Administration API.

use std::sync::Arc;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use fortitude_store::Store;

pub fn router(store: Arc<Store>) -> Router {
    Router::new()
        .route("/statistics", get(statistics))
        .with_state(store)
}

async fn statistics(State(store): State<Arc<Store>>) -> Response {
    match store.statistics().await {
        Ok(statistics) => Json(statistics).into_response(),
        Err(e) => {
            tracing::error!(error = %e, "cannot compute statistics");
            StatusCode::INTERNAL_SERVER_ERROR.into_response()
        }
    }
}
