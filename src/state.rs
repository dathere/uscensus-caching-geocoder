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
}

impl AppState {
    pub fn new(settings: Settings, store: Store, upstream: Upstream) -> SharedState {
        let catalog = load_catalog(&store);
        Arc::new(Self {
            settings: ArcSwap::from_pointee(settings),
            store,
            upstream: Arc::new(upstream),
            catalog: ArcSwap::from_pointee(catalog),
            singleflight: Arc::new(Singleflight::default()),
            codec: Codec::embedded(),
            stats: Arc::new(StatsBuffer::default()),
            started_at: Instant::now(),
            jobs_notify: Notify::new(),
            refresh_lock: tokio::sync::Mutex::new(()),
        })
    }

    /// Snapshot of the current effective settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.load_full()
    }
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
