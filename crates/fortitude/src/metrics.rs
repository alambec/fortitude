//! Prometheus metrics: HTTP requests by route and status, plus the counters and histograms
//! recorded by the store (`fortitude_index_*`) and STOW-RS (`fortitude_stow_*`).

use std::time::Instant;

use axum::extract::{MatchedPath, Request, State};
use axum::http::header;
use axum::middleware::Next;
use axum::response::{IntoResponse, Response};
use metrics_exporter_prometheus::{
    Matcher, PrometheusBuilder, PrometheusHandle, PrometheusRecorder,
};

const SECONDS: &[f64] = &[
    0.0005, 0.001, 0.0025, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 10.0,
];
const BATCH: &[f64] = &[1.0, 2.0, 5.0, 10.0, 20.0, 50.0, 100.0, 200.0, 512.0];

pub fn recorder() -> PrometheusRecorder {
    PrometheusBuilder::new()
        .set_buckets_for_metric(Matcher::Suffix("_seconds".into()), SECONDS)
        .and_then(|b| {
            b.set_buckets_for_metric(Matcher::Full("fortitude_index_batch_size".into()), BATCH)
        })
        .expect("non-empty buckets")
        .build_recorder()
}

/// Record the method, route and status of each request, and the time to produce its
/// response headers (streamed bodies may take longer).
pub async fn track(request: Request, next: Next) -> Response {
    let started = Instant::now();
    let method = request.method().to_string();
    let route = request
        .extensions()
        .get::<MatchedPath>()
        .map_or_else(|| "unmatched".to_owned(), |p| p.as_str().to_owned());
    let response = next.run(request).await;
    let status = response.status().as_u16().to_string();
    metrics::counter!("fortitude_http_requests_total", "method" => method.clone(), "route" => route.clone(), "status" => status)
        .increment(1);
    metrics::histogram!("fortitude_http_request_duration_seconds", "method" => method, "route" => route)
        .record(started.elapsed().as_secs_f64());
    response
}

pub async fn render(State(handle): State<PrometheusHandle>) -> Response {
    (
        [(header::CONTENT_TYPE, "text/plain; version=0.0.4")],
        handle.render(),
    )
        .into_response()
}
