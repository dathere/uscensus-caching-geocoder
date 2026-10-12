//! HTTP surface: router assembly and the server loop.

use anyhow::Context;
use axum::{Json, Router, extract::State, routing::get};
use serde::Serialize;
use tokio::net::TcpListener;
use tracing::{info, warn};

use crate::{
    VERSION,
    config::Settings,
    state::{AppState, SharedState},
};

pub fn router(state: SharedState) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .with_state(state)
}

/// `/healthz` body. `breaker`, `db_ok` and `upstream_last_ok_at` are placeholders
/// (`"closed"`, `null`, `null`) until the store (Phase 3) and upstream (Phase 4) exist.
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
    Json(Health {
        status: "ok",
        version: VERSION,
        uptime_secs: state.started_at.elapsed().as_secs(),
        breaker: "closed",
        db_ok: None,
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
    let state = AppState::new(settings);
    let listener = TcpListener::bind(cfg.server.bind)
        .await
        .with_context(|| format!("binding {}", cfg.server.bind))?;
    let addr = listener.local_addr()?;
    info!(%addr, version = VERSION, "listening");
    axum::serve(listener, router(state))
        .with_graceful_shutdown(shutdown_signal())
        .await
        .context("server error")?;
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
