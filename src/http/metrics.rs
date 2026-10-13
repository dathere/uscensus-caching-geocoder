//! Prometheus metrics (PLAN.md §8): `GET /metrics`, token-protected unless
//! `admin.metrics_public`.
//!
//! One recorder per process (the `metrics` facade is global); every [`AppState`] in a
//! process shares it, so counters accumulate across states (tests assert `>=`).
//! Gauges are set from live state at scrape time.

use std::sync::OnceLock;

use axum::{
    extract::{MatchedPath, Request, State},
    http::{HeaderMap, HeaderValue, Method, header},
    middleware::Next,
    response::{IntoResponse, Response},
};
use metrics_exporter_prometheus::{PrometheusBuilder, PrometheusHandle};

use super::auth::authorized;
use crate::{error::AppError, state::SharedState};

static HANDLE: OnceLock<PrometheusHandle> = OnceLock::new();

/// Install the process-wide recorder (idempotent).
pub fn install() -> &'static PrometheusHandle {
    HANDLE.get_or_init(|| {
        let recorder = PrometheusBuilder::new().build_recorder();
        let handle = recorder.handle();
        // Fails only if another recorder was installed first; ours then just stays empty.
        let _ = metrics::set_global_recorder(recorder);
        handle
    })
}

/// `endpoint` label for a geocoder route.
fn endpoint(method: &Method, route: &str) -> &'static str {
    match (method, route) {
        (_, "/geocoder/benchmarks") => "benchmarks",
        (_, "/geocoder/vintages") => "vintages",
        (&Method::POST, _) => "batch",
        _ => "single",
    }
}

/// `cache` label: the response's `X-Cache` (HIT/MISS/STALE/BYPASS); a batch with some
/// misses is `MISS`; anything answered before a cache decision (e.g. a 400) is `NONE`.
fn cache_label(h: &HeaderMap) -> &'static str {
    match h.get("x-cache").and_then(|v| v.to_str().ok()) {
        Some("HIT") => "HIT",
        Some("MISS") => "MISS",
        Some("STALE") => "STALE",
        Some("BYPASS") => "BYPASS",
        _ if h.contains_key("x-ucg-cache-misses") => "MISS",
        _ => "NONE",
    }
}

/// Count `ucg_requests_total{endpoint,cache}` for the geocoder routes.
pub async fn count_requests(req: Request, next: Next) -> Response {
    let method = req.method().clone();
    let route = req
        .extensions()
        .get::<MatchedPath>()
        .map(|p| p.as_str().to_owned())
        .unwrap_or_default();
    let resp = next.run(req).await;
    metrics::counter!(
        "ucg_requests_total",
        "endpoint" => endpoint(&method, &route),
        "cache" => cache_label(resp.headers()),
    )
    .increment(1);
    resp
}

#[allow(clippy::cast_precision_loss)]
fn set_gauges(st: &SharedState) {
    let breaker = st.upstream.throttle.breaker_state();
    metrics::gauge!("ucg_breaker_open").set(if breaker.open { 1.0 } else { 0.0 });
    metrics::gauge!("ucg_breaker_consecutive_failures")
        .set(f64::from(breaker.consecutive_failures));
    let (single_waiting, batch_waiting) = st.upstream.throttle.sync_waiters();
    metrics::gauge!("ucg_upstream_waiters", "kind" => "single").set(single_waiting as f64);
    metrics::gauge!("ucg_upstream_waiters", "kind" => "batch").set(batch_waiting as f64);
    metrics::gauge!("ucg_singleflight_in_flight").set(st.singleflight.in_flight() as f64);
    if let Ok(n) = st.store.entry_count() {
        metrics::gauge!("ucg_cache_entries").set(n as f64);
    }
    if let Ok(n) = st.store.retry_len() {
        metrics::gauge!("ucg_retry_queue_len").set(n as f64);
    }
    if let Ok(n) = st.store.file_size() {
        metrics::gauge!("ucg_db_file_bytes").set(n as f64);
    }
}

pub async fn metrics(State(st): State<SharedState>, headers: HeaderMap) -> Response {
    let settings = st.settings();
    let admin = &settings.config.admin;
    let given = headers
        .get(header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok());
    if !admin.metrics_public && !authorized(admin.token.as_deref(), given) {
        return AppError::Unauthorized("A valid bearer token is required".into()).into_response();
    }
    set_gauges(&st);
    (
        [(
            header::CONTENT_TYPE,
            HeaderValue::from_static("text/plain; version=0.0.4"),
        )],
        install().render(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn labels() {
        assert_eq!(
            endpoint(&Method::GET, "/geocoder/{returntype}/{searchtype}"),
            "single"
        );
        assert_eq!(
            endpoint(&Method::POST, "/geocoder/{returntype}/addressbatch"),
            "batch"
        );
        assert_eq!(endpoint(&Method::GET, "/geocoder/vintages"), "vintages");
        let mut h = HeaderMap::new();
        assert_eq!(cache_label(&h), "NONE");
        h.insert("x-ucg-cache-misses", HeaderValue::from(2));
        assert_eq!(cache_label(&h), "MISS");
        h.insert("x-cache", HeaderValue::from_static("STALE"));
        assert_eq!(cache_label(&h), "STALE");
    }
}
