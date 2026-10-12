//! Refresh-negatives (PLAN.md §6.3.7): re-check batch/job NoMatch entries queued in
//! `RETRY_QUEUE`. A Match upgrades the entry and leaves the queue; a repeated NoMatch is
//! re-queued with one more attempt, and after `retry.negative_attempts` it is stored as
//! `Confirmed` (kept for `ttl.nomatch_single`) and leaves the queue.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    time::Duration,
};

use tracing::{info, warn};

use super::{
    JobKind, JobRecord, JobRow, RowInput, RowState,
    submit::{self, JobSpec, SubmitError, result_status},
    worker::{ChunkUpdate, ExecError, UploadRows, target},
};
use crate::{
    batch::{Answers, unix_now},
    cache::{
        envelope::{EntrySource, Status},
        key::{CacheKey, SearchType},
        policy::needs_negative_retry,
        views::decode,
    },
    census::{batch_csv::RowResult, types::ReturnType},
    state::{AppState, SharedState},
};

/// Distinct keys per automatically created refresh run.
pub const DEFAULT_LIMIT: usize = 10_000;

#[derive(Debug, Clone, Default)]
pub struct RefreshFilter {
    pub benchmark_id: Option<u32>,
    pub vintage_id: Option<u32>,
    pub limit: Option<usize>,
}

/// Rebuild a batch input from a key's canonical segment.
fn input_of(search_type: SearchType, canonical: &str) -> Option<RowInput> {
    match search_type {
        SearchType::Address => {
            let parts: Vec<&str> = canonical.split(';').collect();
            let [street, city, state, zip] = parts.as_slice() else {
                return None;
            };
            Some(RowInput::Address {
                street: (*street).to_owned(),
                city: (*city).to_owned(),
                state: (*state).to_owned(),
                zip: (*zip).to_owned(),
            })
        }
        SearchType::Coordinates => {
            let (x, y) = canonical.split_once(',')?;
            Some(RowInput::Coord {
                x: x.to_owned(),
                y: y.to_owned(),
            })
        }
        SearchType::Oneline | SearchType::AddressPR => None,
    }
}

/// Create one refresh job per `(search type, benchmark, vintage)` group of due keys
/// (deduplicated; a key keeps its highest attempt count). Refused while a refresh job
/// is still active: overlapping runs would re-check the same keys with the same attempt
/// counts, bypassing `retry.negative_delay` and undercounting attempts.
pub async fn create_jobs(
    st: &AppState,
    filter: RefreshFilter,
) -> Result<Vec<JobRecord>, SubmitError> {
    let _guard = st.refresh_lock.lock().await;
    let active = st
        .store
        .job_list()?
        .iter()
        .any(|j| j.kind == JobKind::RefreshNegatives && j.state.is_active());
    if active {
        return Err(SubmitError::Busy(
            "A refresh-negatives job is already active".into(),
        ));
    }
    let limit = filter.limit.unwrap_or(DEFAULT_LIMIT).max(1);
    let due = st.store.retry_due(unix_now(), usize::MAX)?;
    let mut attempts: HashMap<String, u8> = HashMap::new();
    let mut order: Vec<String> = Vec::new();
    for item in due {
        match attempts.get_mut(&item.key) {
            Some(a) => *a = (*a).max(item.attempts),
            None => {
                order.push(item.key.clone());
                attempts.insert(item.key, item.attempts);
            }
        }
    }
    let mut groups: BTreeMap<(SearchType, u32, u32), Vec<JobRow>> = BTreeMap::new();
    let mut taken = 0;
    for key in order {
        if taken == limit {
            break;
        }
        let Ok(parsed) = CacheKey::parse(&key) else {
            continue;
        };
        if filter
            .benchmark_id
            .is_some_and(|b| b != parsed.benchmark_id)
            || filter.vintage_id.is_some_and(|v| v != parsed.vintage_id)
        {
            continue;
        }
        let Some(input) = input_of(parsed.search_type, &parsed.canonical) else {
            continue;
        };
        taken += 1;
        groups
            .entry((parsed.search_type, parsed.benchmark_id, parsed.vintage_id))
            .or_default()
            .push(JobRow {
                user_id: key.clone(),
                input,
                attempts: attempts[&key],
                key,
                state: RowState::Pending,
                status: None,
            });
    }

    let catalog = st.catalog.load();
    let mut jobs = Vec::new();
    for ((search_type, b, v), rows) in groups {
        let (Some(bench), Some(vintage)) = (catalog.benchmark(b), catalog.vintage(b, v)) else {
            warn!(
                benchmark = b,
                vintage = v,
                "skipping queued keys of an unknown benchmark/vintage"
            );
            continue;
        };
        let spec = JobSpec {
            kind: JobKind::RefreshNegatives,
            return_type: ReturnType::Geographies,
            bench: bench.clone(),
            vintage: vintage.clone(),
            original_filename: None,
            priority: -1,
        };
        jobs.push(submit::create(st, spec, search_type, rows, false).await?);
    }
    Ok(jobs)
}

/// What to do with one refresh row, decided from the *current* entry and retry queue
/// just before uploading (the job's rows may be stale: an earlier or cancelled run can
/// have re-checked the key since this job was created).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum RowPlan {
    /// Nothing to re-check; `dequeue` drops the key's queue items.
    Settle {
        status: Option<Status>,
        dequeue: bool,
    },
    /// Re-check now; `attempts` already made, from the queue.
    Refresh { attempts: u8 },
}

pub(crate) struct RefreshPlan {
    rows: HashMap<u32, RowPlan>,
}

impl RefreshPlan {
    pub fn new(st: &AppState, rows: &[(u32, JobRow)]) -> Result<Self, ExecError> {
        let keys: Vec<&str> = rows.iter().map(|(_, r)| r.key.as_str()).collect();
        let stored = st.store.get_entries(&keys)?;
        let key_set: HashSet<String> = keys.iter().map(|k| (*k).to_owned()).collect();
        let queued = st.store.retry_items_for(&key_set)?;
        let now = unix_now();
        let mut plan = HashMap::new();
        for ((i, row), bytes) in rows.iter().zip(stored) {
            let current = bytes.and_then(|b| decode(&b, st.codec).ok());
            let p = match current {
                Some(s) if needs_negative_retry(s.entry.status, s.entry.source) => {
                    match queued.get(&row.key) {
                        // Due: re-check with the queue's own attempt count.
                        Some(items) if items.iter().any(|(due, _)| *due <= now) => {
                            RowPlan::Refresh {
                                attempts: items.iter().map(|(_, a)| *a).max().unwrap_or(0),
                            }
                        }
                        // Not due yet (re-checked since this job was created) or no
                        // longer queued: leave the queue to the run that owns it.
                        _ => RowPlan::Settle {
                            status: Some(s.entry.status),
                            dequeue: false,
                        },
                    }
                }
                Some(s) => RowPlan::Settle {
                    status: Some(s.entry.status),
                    dequeue: true,
                },
                None => RowPlan::Settle {
                    status: None,
                    dequeue: true,
                },
            };
            plan.insert(*i, p);
        }
        Ok(Self { rows: plan })
    }
}

pub(crate) async fn execute(
    st: &AppState,
    job: &JobRecord,
    rows: Vec<(u32, JobRow)>,
    plan: RefreshPlan,
) -> Result<ChunkUpdate, ExecError> {
    let mut update = ChunkUpdate::default();
    let mut todo = Vec::new();
    for (i, row) in &rows {
        match plan.rows.get(i) {
            Some(RowPlan::Refresh { attempts }) => {
                update.dequeue.insert(row.key.clone());
                todo.push((*i, row, *attempts));
            }
            Some(RowPlan::Settle { status, dequeue }) => {
                if *dequeue {
                    update.dequeue.insert(row.key.clone());
                }
                update.rows.push((*i, RowState::Cached, *status));
            }
            None => update.rows.push((*i, RowState::Cached, None)),
        }
    }
    if todo.is_empty() {
        return Ok(update);
    }

    let cfg = st.settings().config.clone();
    let max = u8::try_from(cfg.retry.negative_attempts).unwrap_or(u8::MAX);
    let next_attempts: HashMap<String, u8> = todo
        .iter()
        .map(|(_, r, a)| (r.key.clone(), a.saturating_add(1)))
        .collect();
    let finals: HashSet<String> = next_attempts
        .iter()
        .filter(|(_, a)| **a >= max)
        .map(|(k, _)| k.clone())
        .collect();

    let t = target(st, job)?;
    let Answers {
        results: fetched,
        entries,
    } = UploadRows::build(todo.iter().map(|(i, r, _)| (*i, &r.input, r.key.as_str())))
        .fetch(st, &t, |key, status| {
            if status == Status::NoMatch && finals.contains(key) {
                EntrySource::Confirmed
            } else {
                EntrySource::Job
            }
        })
        .await?;
    update.entries = entries;
    let due = unix_now() + cfg.retry.negative_delay.as_secs();
    let mut requeued = HashSet::new();
    for (i, row, _) in todo {
        let Some(result) = fetched.get(&row.key) else {
            return Err(ExecError::Other(format!("no answer for row {i}")));
        };
        if *result == RowResult::NoMatch
            && !finals.contains(&row.key)
            && requeued.insert(row.key.clone())
        {
            update
                .enqueue
                .push((row.key.clone(), due, next_attempts[&row.key]));
        }
        update
            .rows
            .push((i, RowState::Done, Some(result_status(result))));
    }
    Ok(update)
}

/// Create refresh jobs every `retry.auto_refresh_interval` (`0` = off; re-read each
/// round so runtime changes apply) when keys are due and no refresh job is active.
pub async fn scheduler(st: SharedState) {
    loop {
        let every = st.settings().config.retry.auto_refresh_interval;
        if every.is_zero() {
            tokio::time::sleep(Duration::from_secs(60)).await;
            continue;
        }
        tokio::time::sleep(every).await;
        // It may have been switched off (runtime override) while we slept.
        if st.settings().config.retry.auto_refresh_interval.is_zero() {
            continue;
        }
        match auto_refresh(&st).await {
            Ok(0) => {}
            Ok(n) => info!(jobs = n, "scheduled refresh-negatives"),
            Err(e) => warn!(error = %e, "scheduling refresh-negatives failed"),
        }
    }
}

async fn auto_refresh(st: &AppState) -> Result<usize, SubmitError> {
    if st.store.retry_due(unix_now(), 1)?.is_empty() {
        return Ok(0);
    }
    match create_jobs(st, RefreshFilter::default()).await {
        Ok(jobs) => Ok(jobs.len()),
        Err(SubmitError::Busy(_)) => Ok(0),
        Err(e) => Err(e),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inputs_from_canonical_keys() {
        assert_eq!(
            input_of(SearchType::Address, "4600 SILVER HILL RD;WASHINGTON;DC;"),
            Some(RowInput::Address {
                street: "4600 SILVER HILL RD".into(),
                city: "WASHINGTON".into(),
                state: "DC".into(),
                zip: String::new(),
            })
        );
        assert_eq!(
            input_of(SearchType::Coordinates, "-76.927440,38.846010"),
            Some(RowInput::Coord {
                x: "-76.927440".into(),
                y: "38.846010".into()
            })
        );
        assert_eq!(input_of(SearchType::Address, "a;b"), None);
        assert_eq!(input_of(SearchType::Oneline, "X"), None);
    }
}
