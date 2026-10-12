//! Upstream Census/TIGERweb client: throttle + retry/backoff + circuit breaker.

pub mod backoff;
pub mod batch;
pub mod catalog;
pub mod single;
pub mod throttle;

use std::{
    collections::VecDeque,
    sync::{
        Mutex, PoisonError,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use arc_swap::ArcSwap;
use bytes::Bytes;
use reqwest::{
    Client, RequestBuilder, StatusCode,
    header::{CONTENT_TYPE, USER_AGENT},
};
use serde::Serialize;
use tracing::{debug, warn};

use crate::config::Config;
use throttle::{BreakerOpen, Priority, Throttle};

/// How many recent upstream errors `/api/v1/upstream` can show.
const ERROR_RING: usize = 20;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Single,
    Batch,
}

#[derive(Debug, Clone)]
pub struct UpstreamResponse {
    pub status: StatusCode,
    pub content_type: Option<String>,
    pub body: Bytes,
}

#[derive(Debug, thiserror::Error)]
pub enum UpstreamError {
    #[error(transparent)]
    BreakerOpen(#[from] BreakerOpen),
    /// A non-retryable answer (e.g. 400 with a Census error body), to forward verbatim.
    #[error("upstream rejected the request with HTTP {}", .0.status)]
    Rejected(UpstreamResponse),
    #[error("upstream failed after {attempts} attempt(s): {last}")]
    Exhausted { attempts: u32, last: String },
}

#[derive(Debug, Clone, Serialize)]
pub struct ErrorRecord {
    /// Unix seconds.
    pub at: u64,
    pub message: String,
}

#[derive(Debug, Clone)]
struct Settings {
    base_url: String,
    tigerweb_url: String,
    user_agent: String,
    timeout: Duration,
    batch_timeout: Duration,
    max_retries: u32,
    retry_base: Duration,
    retry_cap: Duration,
}

impl From<&Config> for Settings {
    fn from(c: &Config) -> Self {
        Self {
            base_url: c.upstream.url.trim_end_matches('/').to_owned(),
            tigerweb_url: c.upstream.tigerweb_url.trim_end_matches('/').to_owned(),
            user_agent: c.upstream.user_agent.clone(),
            timeout: Duration::from_secs(c.upstream.timeout_secs),
            batch_timeout: Duration::from_secs(c.upstream.batch_timeout_secs),
            max_retries: c.throttle.max_retries,
            retry_base: Duration::from_millis(c.throttle.retry_base_ms),
            retry_cap: Duration::from_secs(c.throttle.retry_max_secs),
        }
    }
}

pub struct Upstream {
    http: Client,
    settings: ArcSwap<Settings>,
    pub throttle: Throttle,
    last_ok_at: AtomicU64,
    errors: Mutex<VecDeque<ErrorRecord>>,
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

impl Upstream {
    pub fn new(cfg: &Config) -> anyhow::Result<Self> {
        // User-Agent is set per request from the live settings (see `execute`).
        let http = Client::builder().build()?;
        Ok(Self {
            http,
            settings: ArcSwap::from_pointee(Settings::from(cfg)),
            throttle: Throttle::new(&cfg.throttle),
            last_ok_at: AtomicU64::new(0),
            errors: Mutex::new(VecDeque::with_capacity(ERROR_RING)),
        })
    }

    /// Apply changed upstream/throttle settings at runtime.
    pub fn reconfigure(&self, cfg: &Config) {
        self.settings
            .store(std::sync::Arc::new(Settings::from(cfg)));
        self.throttle.reconfigure(&cfg.throttle);
    }

    pub fn base_url(&self) -> String {
        self.settings.load().base_url.clone()
    }

    pub fn tigerweb_url(&self) -> String {
        self.settings.load().tigerweb_url.clone()
    }

    /// Unix seconds of the last non-transient upstream answer, if any.
    pub fn last_ok_at(&self) -> Option<u64> {
        Some(self.last_ok_at.load(Ordering::Relaxed)).filter(|t| *t > 0)
    }

    pub fn recent_errors(&self) -> Vec<ErrorRecord> {
        self.errors
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .cloned()
            .collect()
    }

    fn record_error(&self, message: String) {
        let mut ring = self.errors.lock().unwrap_or_else(PoisonError::into_inner);
        if ring.len() == ERROR_RING {
            ring.pop_front();
        }
        ring.push_back(ErrorRecord {
            at: unix_now(),
            message,
        });
    }

    /// Run a request through the throttle with retries. `build` is called once per
    /// attempt (multipart bodies cannot be cloned). The throttle is re-acquired for
    /// every attempt and released while backing off.
    pub async fn execute(
        &self,
        kind: Kind,
        prio: Priority,
        build: impl Fn(&Client) -> RequestBuilder,
    ) -> Result<UpstreamResponse, UpstreamError> {
        let s = self.settings.load_full();
        let timeout = match kind {
            Kind::Single => s.timeout,
            Kind::Batch => s.batch_timeout,
        };
        let mut attempt = 0;
        loop {
            let permit = match kind {
                Kind::Single => self.throttle.acquire_single(prio).await?,
                Kind::Batch => self.throttle.acquire_batch(prio).await?,
            };
            let req = build(&self.http)
                .timeout(timeout)
                .header(USER_AGENT, s.user_agent.as_str());
            let (msg, retry_after) = match req.send().await {
                Ok(resp) if !backoff::is_retryable(resp.status()) => {
                    let status = resp.status();
                    let content_type = resp
                        .headers()
                        .get(CONTENT_TYPE)
                        .and_then(|v| v.to_str().ok())
                        .map(str::to_owned);
                    match resp.bytes().await {
                        Ok(body) => {
                            drop(permit);
                            self.throttle.record_success();
                            self.last_ok_at.store(unix_now(), Ordering::Relaxed);
                            let r = UpstreamResponse {
                                status,
                                content_type,
                                body,
                            };
                            return if status.is_success() {
                                Ok(r)
                            } else {
                                Err(UpstreamError::Rejected(r))
                            };
                        }
                        Err(e) => (format!("reading body: {e}"), None),
                    }
                }
                Ok(resp) => (
                    format!("HTTP {}", resp.status()),
                    backoff::retry_after(resp.headers(), SystemTime::now()),
                ),
                Err(e) => (e.to_string(), None),
            };
            // Count the failure before freeing the permit so queued callers see an
            // opened breaker.
            self.record_error(msg.clone());
            if self.throttle.record_failure() {
                warn!(error = %msg, "upstream circuit breaker opened");
            }
            drop(permit);
            if attempt >= s.max_retries {
                return Err(UpstreamError::Exhausted {
                    attempts: attempt + 1,
                    last: msg,
                });
            }
            let wait = backoff::delay(attempt, s.retry_base, s.retry_cap, retry_after);
            debug!(error = %msg, attempt, ?wait, "transient upstream failure; retrying");
            tokio::time::sleep(wait).await;
            attempt += 1;
        }
    }
}
