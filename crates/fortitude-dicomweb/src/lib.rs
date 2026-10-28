//! DICOMweb HTTP services: STOW-RS, WADO-RS, QIDO-RS and WADO-URI.

use axum::Router;

/// Routes of the DICOMweb services, meant to be nested under the configured base path.
pub fn router<S>() -> Router<S>
where
    S: Clone + Send + Sync + 'static,
{
    Router::new()
}
