//! `/api/v1/catalog`, `/api/v1/config`, `/api/v1/upstream` (PLAN.md §8).

use std::collections::{BTreeMap, HashMap};

use axum::{
    Json,
    extract::{Path, State},
    response::{IntoResponse, Response},
};
use serde::Serialize;
use serde_json::Value;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use super::geocoder::refresh_catalog;
use crate::{
    cache::policy::BenchmarkClass,
    config::{FieldReport, is_overridable},
    error::AppError,
    state::SharedState,
    upstream::{ErrorRecord, throttle::BreakerState},
};

pub const TAG: &str = "admin";

pub fn router() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(get_catalog))
        .routes(routes!(refresh))
        .routes(routes!(get_config, put_config))
        .routes(routes!(delete_config))
        .routes(routes!(get_upstream))
        .routes(routes!(reset_breaker))
}

// ------------------------------------------------------------------ catalog

#[derive(Debug, Serialize, ToSchema)]
pub struct VintageView {
    pub id: u32,
    pub name: String,
    pub is_default: bool,
    /// Layers returned when a request omits `layers` (absent: all stored layers).
    pub default_layers: Option<Vec<String>>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BenchmarkView {
    pub id: u32,
    pub name: String,
    pub is_default: bool,
    /// `current` (data drifts behind the id) or `fixed`.
    pub class: String,
    pub vintages: Vec<VintageView>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CatalogView {
    pub benchmarks: Vec<BenchmarkView>,
}

pub(crate) fn catalog_view(st: &SharedState) -> CatalogView {
    let c = st.catalog.load();
    CatalogView {
        benchmarks: c
            .benchmarks()
            .iter()
            .map(|b| BenchmarkView {
                id: b.id,
                name: b.name.clone(),
                is_default: b.is_default,
                class: match c.class(b.id) {
                    BenchmarkClass::Current => "current",
                    BenchmarkClass::Fixed => "fixed",
                }
                .into(),
                vintages: c
                    .vintages(b.id)
                    .iter()
                    .map(|v| VintageView {
                        id: v.id,
                        name: v.name.clone(),
                        is_default: v.is_default,
                        default_layers: c.default_layers(&v.name).map(<[String]>::to_vec),
                    })
                    .collect(),
            })
            .collect(),
    }
}

/// Benchmarks and vintages in effect (with default layer lists).
#[utoipa::path(get, path = "/api/v1/catalog", tag = TAG, responses((status = 200, body = CatalogView)))]
async fn get_catalog(State(st): State<SharedState>) -> Json<CatalogView> {
    Json(catalog_view(&st))
}

/// Refetch benchmarks and vintages from upstream (background priority). 502 if any
/// part failed (the previous catalog stays in effect for it).
#[utoipa::path(
    post,
    path = "/api/v1/catalog/refresh",
    tag = TAG,
    responses(
        (status = 200, body = CatalogView),
        (status = 502, description = "Upstream refresh failed (partly or entirely)")
    )
)]
async fn refresh(State(st): State<SharedState>) -> Response {
    let failures = refresh_catalog(st.clone()).await;
    if failures.is_empty() {
        Json(catalog_view(&st)).into_response()
    } else {
        AppError::BadGateway(format!("Catalog refresh failed: {}", failures.join("; ")))
            .into_response()
    }
}

// ------------------------------------------------------------------- config

#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigField {
    #[serde(flatten)]
    pub field: FieldReport,
    /// Can be changed with `PUT /api/v1/config`.
    pub overridable: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ConfigView {
    pub fields: Vec<ConfigField>,
}

fn config_view(st: &SharedState) -> ConfigView {
    ConfigView {
        fields: st
            .settings()
            .describe()
            .into_iter()
            .map(|field| ConfigField {
                overridable: is_overridable(field.path),
                field,
            })
            .collect(),
    }
}

/// Effective configuration with the source of every value (secrets redacted).
#[utoipa::path(get, path = "/api/v1/config", tag = TAG, responses((status = 200, body = ConfigView)))]
async fn get_config(State(st): State<SharedState>) -> Json<ConfigView> {
    Json(config_view(&st))
}

fn stored_overrides(st: &SharedState) -> Result<BTreeMap<String, String>, Box<Response>> {
    st.store
        .overrides()
        .map(|o| o.into_iter().collect())
        .map_err(|e| Box::new(AppError::Internal(e.into()).into_response()))
}

/// Set runtime overrides (`{"throttle.single_rps": 2, "ttl.tie": "7d"}`), persisted and
/// applied immediately (the upstream limiter is rebuilt). Only the `cache`, `ttl`,
/// `retry`, `batch`, `jobs` and `throttle` sections can be overridden.
#[utoipa::path(
    put,
    path = "/api/v1/config",
    tag = TAG,
    request_body(content = HashMap<String, Value>, content_type = "application/json"),
    responses(
        (status = 200, body = ConfigView),
        (status = 400, description = "Unknown, fixed or invalid key/value")
    )
)]
async fn put_config(
    State(st): State<SharedState>,
    Json(body): Json<HashMap<String, Value>>,
) -> Response {
    let _guard = st.config_lock.lock().await;
    let mut all = match stored_overrides(&st) {
        Ok(o) => o,
        Err(r) => return *r,
    };
    let mut changed = Vec::with_capacity(body.len());
    for (path, value) in body {
        let json = value.to_string();
        all.insert(path.clone(), json.clone());
        changed.push((path, json));
    }
    let next = match st
        .base_settings()
        .with_overrides(all.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    {
        Ok(s) => s,
        Err(e) => return AppError::BadRequest(e.to_string()).into_response(),
    };
    if let Err(e) = st.store.set_overrides(changed).await {
        return AppError::Internal(e.into()).into_response();
    }
    st.apply_settings(next);
    Json(config_view(&st)).into_response()
}

/// Remove one runtime override; the key falls back to its startup value.
#[utoipa::path(
    delete,
    path = "/api/v1/config/{path}",
    tag = TAG,
    params(("path" = String, Path, description = "Dotted key, e.g. `throttle.single_rps`")),
    responses(
        (status = 200, body = ConfigView),
        (status = 404, description = "No override for that key")
    )
)]
async fn delete_config(State(st): State<SharedState>, Path(path): Path<String>) -> Response {
    let _guard = st.config_lock.lock().await;
    let mut all = match stored_overrides(&st) {
        Ok(o) => o,
        Err(r) => return *r,
    };
    if all.remove(&path).is_none() {
        return AppError::NotFound(format!("No runtime override for `{path}`")).into_response();
    }
    let next = match st
        .base_settings()
        .with_overrides(all.iter().map(|(k, v)| (k.as_str(), v.as_str())))
    {
        Ok(s) => s,
        Err(e) => return AppError::BadRequest(e.to_string()).into_response(),
    };
    if let Err(e) = st.store.delete_override(path).await {
        return AppError::Internal(e.into()).into_response();
    }
    st.apply_settings(next);
    Json(config_view(&st)).into_response()
}

// ----------------------------------------------------------------- upstream

#[derive(Debug, Serialize, ToSchema)]
pub struct LaneView {
    /// Configured concurrent requests.
    pub concurrency: usize,
    /// Permits free right now.
    pub available: usize,
    /// Live (`Sync`) callers queued for a permit.
    pub sync_waiters: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct UpstreamView {
    pub breaker: BreakerState,
    pub single: LaneView,
    pub batch: LaneView,
    pub single_rps: u32,
    pub single_burst: u32,
    pub batch_min_interval_secs: u64,
    /// Distinct upstream fetches currently coalescing live requests.
    pub singleflight_in_flight: usize,
    /// Unix seconds of the last non-transient upstream answer.
    pub last_ok_at: Option<u64>,
    pub recent_errors: Vec<ErrorRecord>,
}

fn upstream_view(st: &SharedState) -> UpstreamView {
    let cfg = st.settings();
    let t = &cfg.config.throttle;
    let throttle = &st.upstream.throttle;
    let (single_free, batch_free) = throttle.available_permits();
    let (single_waiting, batch_waiting) = throttle.sync_waiters();
    UpstreamView {
        breaker: throttle.breaker_state(),
        single: LaneView {
            concurrency: t.single_concurrency,
            available: single_free,
            sync_waiters: single_waiting,
        },
        batch: LaneView {
            concurrency: t.batch_concurrency,
            available: batch_free,
            sync_waiters: batch_waiting,
        },
        single_rps: t.single_rps,
        single_burst: t.single_burst,
        batch_min_interval_secs: t.batch_min_interval_secs,
        singleflight_in_flight: st.singleflight.in_flight(),
        last_ok_at: st.upstream.last_ok_at(),
        recent_errors: st.upstream.recent_errors(),
    }
}

/// Limiter, in-flight requests, circuit breaker and recent upstream errors.
#[utoipa::path(get, path = "/api/v1/upstream", tag = TAG, responses((status = 200, body = UpstreamView)))]
async fn get_upstream(State(st): State<SharedState>) -> Json<UpstreamView> {
    Json(upstream_view(&st))
}

/// Close the circuit breaker now.
#[utoipa::path(post, path = "/api/v1/upstream/breaker/reset", tag = TAG, responses((status = 200, body = UpstreamView)))]
async fn reset_breaker(State(st): State<SharedState>) -> Json<UpstreamView> {
    st.upstream.throttle.reset_breaker();
    Json(upstream_view(&st))
}
