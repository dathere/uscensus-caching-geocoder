//! Shared application state.

use std::{sync::Arc, time::Instant};

use arc_swap::ArcSwap;

use crate::{
    config::Settings,
    store::{Store, stats::StatsBuffer},
    upstream::Upstream,
};

pub type SharedState = Arc<AppState>;

pub struct AppState {
    settings: ArcSwap<Settings>,
    pub store: Store,
    pub upstream: Arc<Upstream>,
    pub stats: Arc<StatsBuffer>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(settings: Settings, store: Store, upstream: Upstream) -> SharedState {
        Arc::new(Self {
            upstream: Arc::new(upstream),
            settings: ArcSwap::from_pointee(settings),
            store,
            stats: Arc::new(StatsBuffer::default()),
            started_at: Instant::now(),
        })
    }

    /// Snapshot of the current effective settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.load_full()
    }
}
