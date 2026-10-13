//! Durable async jobs (PLAN.md §5.6, §6.3): submit, a single background worker that
//! drains chunks through the throttle at `Background` priority, result downloads, and
//! the refresh-negatives job with its scheduler.

pub mod refresh;
pub mod results;
pub mod submit;
pub mod worker;

use serde::{Deserialize, Serialize};

use crate::{
    cache::{
        envelope::Status,
        key::{KeyInput, SearchType},
        views::{BatchNeed, Need},
    },
    census::types::ReturnType,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    Geocode,
    RefreshNegatives,
    /// Cache warming from an import (PLAN.md §8, Phase 8).
    Warm,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum JobState {
    Queued,
    Running,
    Paused,
    Cancelled,
    Completed,
    Failed,
}

impl JobState {
    /// The worker may take chunks from it.
    pub fn is_runnable(self) -> bool {
        matches!(self, Self::Queued | Self::Running)
    }

    /// Not yet finished (it may still be resumed).
    pub fn is_active(self) -> bool {
        matches!(self, Self::Queued | Self::Running | Self::Paused)
    }
}

/// Row tallies. `total = matched + no_match + tie + pending + failed`; `cached` counts
/// the answered rows that came from the cache rather than this job's upstream calls.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct Counts {
    pub total: u64,
    pub cached: u64,
    pub matched: u64,
    pub no_match: u64,
    pub tie: u64,
    pub pending: u64,
    pub failed: u64,
}

impl Counts {
    /// Add (`add = true`) or remove one row's contribution (`total` is not touched).
    pub fn tally(&mut self, row: &JobRow, add: bool) {
        let bump = |c: &mut u64| {
            *c = if add {
                c.saturating_add(1)
            } else {
                c.saturating_sub(1)
            };
        };
        match row.state {
            RowState::Pending => bump(&mut self.pending),
            RowState::Failed => bump(&mut self.failed),
            RowState::Cached | RowState::Done => {
                if row.state == RowState::Cached {
                    bump(&mut self.cached);
                }
                match row.status {
                    Some(Status::Match) => bump(&mut self.matched),
                    Some(Status::Tie) => bump(&mut self.tie),
                    Some(Status::NoMatch) | None => bump(&mut self.no_match),
                }
            }
        }
    }

    pub fn of<'a>(rows: impl IntoIterator<Item = &'a JobRow>) -> Self {
        let mut c = Self::default();
        for r in rows {
            c.total += 1;
            c.tally(r, true);
        }
        c
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
pub struct JobRecord {
    /// UUIDv7: ids sort by creation time.
    pub id: String,
    pub kind: JobKind,
    pub state: JobState,
    pub return_type: ReturnType,
    pub search_type: SearchType,
    pub benchmark_id: u32,
    /// The vintage in the cache keys (the benchmark default for `locations`).
    pub vintage_id: u32,
    pub created_at: u64,
    pub updated_at: u64,
    pub started_at: Option<u64>,
    pub finished_at: Option<u64>,
    pub totals: Counts,
    pub chunk_size: u32,
    pub chunks_total: u32,
    pub original_filename: Option<String>,
    pub error: Option<String>,
    /// Higher runs first; ties run oldest first.
    pub priority: i32,
}

impl JobRecord {
    pub fn batch_need(&self) -> BatchNeed {
        match (self.search_type, self.return_type) {
            (SearchType::Coordinates, _) => BatchNeed::Coordinates,
            (_, ReturnType::Locations) => BatchNeed::Locations,
            (_, ReturnType::Geographies) => BatchNeed::Geographies,
        }
    }

    pub fn need(&self) -> Need {
        match (self.search_type, self.return_type) {
            (SearchType::Coordinates, _) => Need::Coordinates,
            (_, ReturnType::Locations) => Need::Locations,
            (_, ReturnType::Geographies) => Need::Geographies,
        }
    }
}

/// One job input row, as submitted (coordinates keep their original text).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RowInput {
    Address {
        street: String,
        city: String,
        state: String,
        zip: String,
    },
    Oneline {
        address: String,
    },
    Coord {
        x: String,
        y: String,
    },
}

impl RowInput {
    pub fn search_type(&self) -> SearchType {
        match self {
            Self::Address { .. } => SearchType::Address,
            Self::Oneline { .. } => SearchType::Oneline,
            Self::Coord { .. } => SearchType::Coordinates,
        }
    }

    /// `None` for coordinates that are not finite numbers.
    pub fn key_input(&self) -> Option<KeyInput> {
        Some(match self {
            Self::Address {
                street,
                city,
                state,
                zip,
            } => KeyInput::Address {
                street: street.clone(),
                city: city.clone(),
                state: state.clone(),
                zip: zip.clone(),
            },
            Self::Oneline { address } => KeyInput::Oneline {
                address: address.clone(),
            },
            Self::Coord { x, y } => {
                let (x, y) = parse_coords(x, y)?;
                KeyInput::Coordinates { x, y }
            }
        })
    }

    /// The `input` column of an address-style result row.
    pub fn echo(&self) -> String {
        match self {
            Self::Address {
                street,
                city,
                state,
                zip,
            } => [street, city, state, zip]
                .iter()
                .map(|s| s.as_str())
                .collect::<Vec<_>>()
                .join(", "),
            Self::Oneline { address } => address.clone(),
            Self::Coord { .. } => String::new(),
        }
    }
}

pub fn parse_coords(x: &str, y: &str) -> Option<(f64, f64)> {
    let (x, y) = (x.trim().parse::<f64>().ok()?, y.trim().parse::<f64>().ok()?);
    (x.is_finite() && y.is_finite()).then_some((x, y))
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum RowState {
    Pending,
    /// Answered from the cache (at submit or when its chunk ran).
    Cached,
    /// Answered by this job's upstream call.
    Done,
    Failed,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct JobRow {
    pub user_id: String,
    pub input: RowInput,
    pub key: String,
    pub state: RowState,
    pub status: Option<Status>,
    /// Refresh-negatives rows: retry attempts already made for this key.
    pub attempts: u8,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, utoipa::ToSchema)]
#[serde(rename_all = "snake_case")]
pub enum ChunkStatus {
    Pending,
    InFlight,
    Done,
    Failed,
}

/// A contiguous range of row indexes, processed as one upstream unit.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ChunkState {
    pub first_row: u32,
    /// Inclusive.
    pub last_row: u32,
    pub state: ChunkStatus,
    pub lease_until: Option<u64>,
    pub attempts: u32,
    pub last_error: Option<String>,
}
