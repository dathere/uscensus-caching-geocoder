//! Shared application state.

use std::{sync::Arc, time::Instant};

use arc_swap::ArcSwap;

use crate::config::Settings;

pub type SharedState = Arc<AppState>;

pub struct AppState {
    settings: ArcSwap<Settings>,
    pub started_at: Instant,
}

impl AppState {
    pub fn new(settings: Settings) -> SharedState {
        Arc::new(Self {
            settings: ArcSwap::from_pointee(settings),
            started_at: Instant::now(),
        })
    }

    /// Snapshot of the current effective settings.
    pub fn settings(&self) -> Arc<Settings> {
        self.settings.load_full()
    }
}
