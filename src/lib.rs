//! ucgeocoder: a caching, rate-limited, wire-compatible proxy for the US Census Geocoder.

pub mod batch;
pub mod cache;
pub mod catalog;
pub mod census;
pub mod cli;
pub mod config;
pub mod dict;
pub mod error;
pub mod http;
pub mod jobs;
pub mod state;
pub mod store;
pub mod telemetry;
pub mod upstream;

/// Crate version plus the git commit it was built from.
pub const VERSION: &str = concat!(env!("CARGO_PKG_VERSION"), " (", env!("UCG_GIT_SHA"), ")");
