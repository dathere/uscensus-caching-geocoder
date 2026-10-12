//! HTTP surface: router assembly and the server loop.

pub mod api_admin;
pub mod api_cache;
pub mod api_jobs;
pub mod auth;
pub mod batch;
pub mod geocoder;
pub mod metrics;
pub mod openapi;

use std::{sync::Arc, time::Duration};

use anyhow::Context;
use axum::{
    Json, Router,
    extract::{DefaultBodyLimit, State},
    http::{HeaderValue, header},
    middleware::from_fn_with_state,
    response::IntoResponse,
    routing::get,
};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::{
    VERSION,
    config::Settings,
    state::{AppState, SharedState},
    store::{Store, StoreOptions, stats::spawn_stats_flusher},
    upstream::{Upstream, throttle::BreakerState},
};

pub fn router(state: SharedState) -> Router {
    let max_body = usize::try_from(state.settings().config.server.max_body_mb)
        .unwrap_or(usize::MAX)
        .saturating_mul(1024 * 1024);
    let (api, spec) = openapi::api();
    let api = api
        .layer(DefaultBodyLimit::max(max_body))
        .route_layer(from_fn_with_state(state.clone(), auth::require_token));
    let spec: Arc<str> = spec
        .to_pretty_json()
        .expect("the OpenAPI document serializes")
        .into();
    Router::new()
        .route("/healthz", get(healthz))
        .route(
            openapi::SPEC_PATH,
            get(move || {
                let spec = Arc::clone(&spec);
                async move {
                    (
                        [(
                            header::CONTENT_TYPE,
                            HeaderValue::from_static("application/json"),
                        )],
                        spec.to_string(),
                    )
                        .into_response()
                }
            }),
        )
        .route("/metrics", get(metrics::metrics))
        .merge(
            geocoder::routes()
                .merge(batch::routes(max_body))
                .route_layer(axum::middleware::from_fn(metrics::count_requests)),
        )
        .merge(api)
        .with_state(state)
}

/// `/healthz` body.
#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub breaker: BreakerState,
    pub db_ok: Option<bool>,
    pub upstream_last_ok_at: Option<u64>,
}

async fn healthz(State(state): State<SharedState>) -> Json<Health> {
    let db_ok = state.store.is_healthy();
    let breaker = state.upstream.throttle.breaker_state();
    Json(Health {
        status: if !db_ok {
            "degraded"
        } else if breaker.open {
            "upstream_unavailable"
        } else {
            "ok"
        },
        version: VERSION,
        uptime_secs: state.started_at.elapsed().as_secs(),
        breaker,
        db_ok: Some(db_ok),
        upstream_last_ok_at: state.upstream.last_ok_at(),
    })
}

/// Bind, log the bound address, and serve until SIGINT/SIGTERM.
pub async fn serve(settings: Settings) -> anyhow::Result<()> {
    let cfg = settings.config.clone();
    if cfg.admin.token.is_none() {
        warn!(
            "UCG_ADMIN_TOKEN is not set: admin routes are open to anyone who can reach this server"
        );
    }
    let store = Store::open(&StoreOptions::from(&cfg.store))?;
    info!(path = %store.path().display(), entries = store.entry_count()?, "store opened");
    let upstream = Upstream::new(&cfg)?;
    let state = AppState::new(settings, store, upstream);
    tokio::spawn(geocoder::refresh_catalog(state.clone()));
    let flusher = spawn_stats_flusher(
        state.store.clone(),
        state.stats.clone(),
        Duration::from_secs(cfg.store.hitcount_flush_secs),
    );
    let jobs = crate::jobs::worker::spawn(state.clone());
    let listener = TcpListener::bind(cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    let addr = listener.local_addr()?;
    info!(%addr, version = VERSION, "listening");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
    // The job tasks hold the state: stop them so the final drop below is the last.
    jobs.shutdown().await;
    flusher.abort();
    if let Err(e) = state.store.flush_stats(&state.stats).await {
        warn!(error = %e, "final stats flush failed");
    }
    // Dropping the last Store clone drains the writer and commits durably.
    drop(state);
    info!("shut down");
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let term = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut s) => {
                s.recv().await;
            }
            Err(_) => std::future::pending().await,
        }
    };
    #[cfg(not(unix))]
    let term = std::future::pending::<()>();
    tokio::select! {
        () = ctrl_c => {},
        () = term => {},
    }
}
