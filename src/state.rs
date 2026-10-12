//! Shared application state.

use std::{sync::Arc, time::Instant};

use arc_swap::ArcSwap;
use serde_json::Value;
use tokio::sync::Notify;
use tracing::warn;

use crate::{
    cache::{codec::Codec, singleflight::Singleflight},
    catalog::Catalog,
    config::Settings,
    http::geocoder::FetchResult,
    store::{Store, stats::StatsBuffer},
    upstream::Upstream,
};

pub type SharedState = Arc<AppState>;

/// Store key of the cached `/benchmarks` body.
pub const CATALOG_BENCHMARKS: &str = "benchmarks";

/// Store key of a cached `/vintages?benchmark={id}` body.
pub fn catalog_vintages_key(benchmark_id: u32) -> String {
    format!("vintages:{benchmark_id}")
}

pub struct AppState {
    /// Settings from defaults, TOML, env and flags (no runtime overrides).
    base: Settings,
    settings: ArcSwap<Settings>,
    pub store: Store,
    pub upstream: Arc<Upstream>,
    pub catalog: ArcSwap<Catalog>,
    pub singleflight: Arc<Singleflight<FetchResult>>,
    pub codec: &'static Codec,
    pub stats: Arc<StatsBuffer>,
    pub started_at: Instant,
    /// Wakes the job runner (new job, resume).
    pub jobs_notify: Notify,
    /// Serializes refresh-negatives creation (check for an active run, then create).
    pub refresh_lock: tokio::sync::Mutex<()>,
    /// Serializes runtime config changes (read-modify-write of the overrides table).
    pub config_lock: tokio::sync::Mutex<()>,
}

impl AppState {
    pub fn new(settings: Settings, store: Store, upstream: Upstream) -> SharedState {
        crate::http::metrics::install();
        let catalog = load_catalog(&store);
        let effective = with_stored_overrides(&settings, &store);
        if effective != settings {
            upstream.reconfigure(&effective.config);
        }
        Arc::new(Self {
            base: settings,
            settings: ArcSwap::from_pointee(effective),
            store,
            upstream: Arc::new(upstream),
            catalog: ArcSwap::from_pointee(catalog),
            singleflight: Arc::new(Singleflight::default()),
            codec: Codec::embedded(),
            stats: Arc::new(StatsBuffer::default()),
            started_at: Instant::now(),
            jobs_notify: Notify::new(),
            refresh_lock: tokio::sync::Mutex::new(()),
            config_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Snapshot of the current effective settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.load_full()
    }

    /// Settings before runtime overrides.
    pub fn base_settings(&self) -> &Settings {
        &self.base
    }

    /// Make `settings` effective now (hold `config_lock`): readers see them on their
    /// next `settings()` call and the throttle/limiter is rebuilt.
    pub fn apply_settings(&self, settings: Settings) {
        self.upstream.reconfigure(&settings.config);
        self.settings.store(Arc::new(settings));
    }
}

/// `base` with the store's runtime overrides applied. An override that no longer applies
/// (e.g. a key renamed in a newer build, or a value now invalid) is skipped with a warning
/// rather than keeping the server from starting.
fn with_stored_overrides(base: &Settings, store: &Store) -> Settings {
    let overrides = match store.overrides() {
        Ok(o) => o,
        Err(e) => {
            warn!(error = %e, "reading runtime config overrides failed; using base settings");
            return base.clone();
        }
    };
    let mut current = base.clone();
    for (path, json) in &overrides {
        match current.with_overrides([(path.as_str(), json.as_str())]) {
            Ok(next) => current = next,
            Err(e) => warn!(key = %path, error = %e, "ignoring stored runtime override"),
        }
    }
    current
}

/// The embedded snapshot, overlaid with any catalog bodies cached in the store
/// (whatever their age: they are newer than the snapshot).
pub fn load_catalog(store: &Store) -> Catalog {
    let mut catalog = Catalog::snapshot();
    let parse = |raw: &[u8]| serde_json::from_slice::<Value>(raw).ok();
    if let Ok(Some((raw, _))) = store.catalog_get(CATALOG_BENCHMARKS)
        && let Some(doc) = parse(&raw)
    {
        match catalog.clone().with_benchmarks_doc(doc) {
            Ok(c) => catalog = c,
            Err(e) => warn!(error = %e, "ignoring stored benchmarks"),
        }
    }
    let ids: Vec<u32> = catalog.benchmarks().iter().map(|b| b.id).collect();
    for id in ids {
        if let Ok(Some((raw, _))) = store.catalog_get(&catalog_vintages_key(id))
            && let Some(doc) = parse(&raw)
            && let Ok(c) = catalog.clone().with_vintages_doc(id, doc)
        {
            catalog = c;
        }
    }
    catalog
}
