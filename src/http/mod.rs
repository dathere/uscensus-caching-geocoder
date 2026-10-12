//! HTTP surface: router assembly and the server loop.

use std::time::Duration;

use anyhow::Context;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::{
    VERSION,
    config::Settings,
    state::{AppState, SharedState},
    store::{Store, StoreOptions, stats::spawn_stats_flusher},
};

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// `/healthz` body. `breaker` and `upstream_last_ok_at` are placeholders (`"closed"`,
/// `null`) until the upstream client exists (Phase 4).
#[derive(Debug, Serialize)]
pub struct Health {
    pub status: &'static str,
    pub version: &'static str,
    pub uptime_secs: u64,
    pub breaker: &'static str,
    pub db_ok: Option<bool>,
    pub upstream_last_ok_at: Option<u64>,
}

async fn healthz(State(state): State<SharedState>) -> Json<Health> {
    let db_ok = state.store.is_healthy();
    Json(Health {
        status: if db_ok { "ok" } else { "degraded" },
        version: VERSION,
        uptime_secs: state.started_at.elapsed().as_secs(),
        breaker: "closed",
        db_ok: Some(db_ok),
        upstream_last_ok_at: None,
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
    let state = AppState::new(settings, store);
    let flusher = spawn_stats_flusher(
        state.store.clone(),
        state.stats.clone(),
        Duration::from_secs(cfg.store.hitcount_flush_secs),
    );
    let listener = TcpListener::bind(cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    let addr = listener.local_addr()?;
    info!(%addr, version = VERSION, "listening");
    axum::serve(listener, router(state.clone()))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
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
