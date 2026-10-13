//! The job runner (PLAN.md §6.3.2–8): one task that takes the next pending chunk of the
//! highest-priority, oldest runnable job, answers its rows (cache first, then upstream
//! at `Background` priority), and commits cache entries + retry-queue changes + rows +
//! chunk + totals in one transaction, so an interrupted chunk leaves nothing behind and
//! is simply re-run.

use std::{
    cmp::Reverse,
    collections::HashSet,
    sync::{Arc, Mutex, PoisonError},
    time::{Duration, SystemTime},
};

use redb::ReadableTable;
use tokio::task::JoinHandle;
use tracing::{info, warn};

use super::{
    ChunkState, ChunkStatus, JobKind, JobRecord, JobRow, JobState, RowInput, RowState,
    refresh::{self, RefreshPlan},
    submit::result_status,
};
use crate::{
    batch::{self, Answers, BatchTarget, ChunkError, Row, fetch_answers, unix_now},
    cache::{
        envelope::{Entry, EntrySource, Status},
        key::{CacheKey, SearchType},
        views::{BatchLookup, Lookup, classify, merge_batch},
    },
    census::{
        batch_csv::{AddressRow, CoordRow},
        types::ReturnType,
    },
    http::{
        batch::address_plan,
        geocoder::{FetchFailure, FetchJob},
    },
    state::{AppState, SharedState},
    store::StoreError,
    upstream::{batch::BatchEndpoint, throttle::Priority},
};

/// Idle poll interval (a submit or state change wakes the runner sooner).
const TICK: Duration = Duration::from_secs(5);

/// The runner and scheduler tasks. They hold the app state, so stop them before
/// dropping the last state reference (the store drains on its last drop).
pub struct JobsHandle {
    tasks: Vec<JoinHandle<()>>,
}

impl JobsHandle {
    pub async fn shutdown(self) {
        for t in &self.tasks {
            t.abort();
        }
        for t in self.tasks {
            let _ = t.await;
        }
    }
}

/// Start the runner (after resetting chunks left in flight by a previous process) and
/// the refresh-negatives scheduler.
pub fn spawn(st: SharedState) -> JobsHandle {
    JobsHandle {
        tasks: vec![
            tokio::spawn(run(st.clone())),
            tokio::spawn(refresh::scheduler(st)),
        ],
    }
}

async fn run(st: SharedState) {
    match reset_in_flight(&st).await {
        Ok(0) => {}
        Ok(n) => info!(chunks = n, "re-queued job chunks left in flight"),
        Err(e) => warn!(error = %e, "resetting in-flight job chunks failed"),
    }
    loop {
        match next_work(&st).await {
            Ok(Some(w)) => {
                process(&st, w).await;
                continue;
            }
            Ok(None) => {}
            Err(e) => warn!(error = %e, "job runner could not read jobs"),
        }
        tokio::select! {
            () = st.jobs_notify.notified() => {}
            () = tokio::time::sleep(TICK) => {}
        }
    }
}

/// Only one process can open the store (redb lock), so at startup every `InFlight`
/// chunk belongs to a process that is gone, whatever its lease says.
async fn reset_in_flight(st: &AppState) -> Result<usize, StoreError> {
    let mut stuck: Vec<(String, u32)> = Vec::new();
    for job in st.store.job_list()? {
        for (idx, c) in st.store.job_chunks(&job.id)? {
            if c.state == ChunkStatus::InFlight {
                stuck.push((job.id.clone(), idx));
            }
        }
    }
    let n = stuck.len();
    if n > 0 {
        st.store
            .write(Box::new(move |ctx| {
                for (id, idx) in &stuck {
                    if let Some(mut c) = ctx.job_chunk(id, *idx)?
                        && c.state == ChunkStatus::InFlight
                    {
                        c.state = ChunkStatus::Pending;
                        c.lease_until = None;
                        ctx.put_job_chunk(id, *idx, &c)?;
                    }
                }
                Ok(())
            }))
            .await?;
    }
    Ok(n)
}

pub(crate) struct Work {
    job: JobRecord,
    chunk_idx: u32,
    chunk: ChunkState,
}

fn claimable(c: &ChunkState, now: u64) -> bool {
    c.state == ChunkStatus::Pending
        || (c.state == ChunkStatus::InFlight && c.lease_until.is_some_and(|l| l <= now))
}

async fn next_work(st: &AppState) -> Result<Option<Work>, StoreError> {
    let now = unix_now();
    let mut jobs: Vec<JobRecord> = st
        .store
        .job_list()?
        .into_iter()
        .filter(|j| j.state.is_runnable())
        .collect();
    jobs.sort_by(|a, b| (Reverse(a.priority), &a.id).cmp(&(Reverse(b.priority), &b.id)));
    for job in jobs {
        let chunks = st.store.job_chunks(&job.id)?;
        if let Some((idx, _)) = chunks.iter().find(|(_, c)| claimable(c, now)) {
            if let Some(w) = claim(st, &job.id, *idx, now).await? {
                return Ok(Some(w));
            }
        } else if chunks
            .iter()
            .all(|(_, c)| matches!(c.state, ChunkStatus::Done | ChunkStatus::Failed))
        {
            finish(st, &job.id).await?;
        }
    }
    Ok(None)
}

async fn claim(st: &AppState, id: &str, idx: u32, now: u64) -> Result<Option<Work>, StoreError> {
    let lease = st.settings().config.jobs.lease_secs;
    let out = Arc::new(Mutex::new(None));
    let slot = Arc::clone(&out);
    let id = id.to_owned();
    st.store
        .write(Box::new(move |ctx| {
            let mut claimed = None;
            if let Some(mut job) = ctx.job_record(&id)?
                && job.state.is_runnable()
                && let Some(mut chunk) = ctx.job_chunk(&id, idx)?
                && claimable(&chunk, now)
            {
                chunk.state = ChunkStatus::InFlight;
                chunk.lease_until = Some(now + lease);
                job.state = JobState::Running;
                job.started_at.get_or_insert(now);
                job.updated_at = now;
                ctx.put_job_chunk(&id, idx, &chunk)?;
                ctx.put_job_record(&job)?;
                claimed = Some(Work {
                    job,
                    chunk_idx: idx,
                    chunk,
                });
            }
            *slot.lock().unwrap_or_else(PoisonError::into_inner) = claimed;
            Ok(())
        }))
        .await?;
    Ok(out.lock().unwrap_or_else(PoisonError::into_inner).take())
}

/// No chunk left to run: `Completed`, or `Failed` when every chunk failed.
async fn finish(st: &AppState, id: &str) -> Result<(), StoreError> {
    let id = id.to_owned();
    let now = unix_now();
    st.store
        .write(Box::new(move |ctx| {
            let Some(mut job) = ctx.job_record(&id)? else {
                return Ok(());
            };
            if !job.state.is_runnable() {
                return Ok(());
            }
            let mut states = Vec::new();
            for item in ctx
                .job_chunks()?
                .range((id.as_str(), 0)..=(id.as_str(), u32::MAX))?
            {
                let (_, v) = item?;
                states.push(crate::store::jobs::dec::<ChunkState>(v.value())?.state);
            }
            if states
                .iter()
                .any(|s| matches!(s, ChunkStatus::Pending | ChunkStatus::InFlight))
            {
                return Ok(());
            }
            let all_failed = !states.is_empty() && states.iter().all(|s| *s == ChunkStatus::Failed);
            job.state = if all_failed {
                job.error = Some("every chunk failed".into());
                JobState::Failed
            } else {
                JobState::Completed
            };
            job.finished_at = Some(now);
            job.updated_at = now;
            ctx.put_job_record(&job)
        }))
        .await
}

/// What answering one chunk changes.
#[derive(Default)]
pub(crate) struct ChunkUpdate {
    /// Upstream answers to store (merged with [`merge_batch`] inside the commit).
    pub entries: Vec<(String, Entry)>,
    /// `(row idx, new state, status)` for rows that were `Pending`.
    pub rows: Vec<(u32, RowState, Option<Status>)>,
    /// Retry-queue keys whose every item is removed.
    pub dequeue: HashSet<String>,
    /// Retry-queue items `(key, due, attempts)` to add (after the removals).
    pub enqueue: Vec<(String, u64, u8)>,
}

#[derive(Debug, thiserror::Error)]
pub(crate) enum ExecError {
    #[error(transparent)]
    Chunk(#[from] ChunkError),
    #[error("{0}")]
    Other(String),
}

impl From<StoreError> for ExecError {
    fn from(e: StoreError) -> Self {
        Self::Other(e.to_string())
    }
}

impl From<anyhow::Error> for ExecError {
    fn from(e: anyhow::Error) -> Self {
        Self::Other(format!("{e:#}"))
    }
}

async fn process(st: &SharedState, w: Work) {
    let result = match execute(st, &w).await {
        Ok(update) => commit(st, &w, update).await,
        Err(e) => {
            warn!(job = %w.job.id, chunk = w.chunk_idx, error = %e, "job chunk failed");
            fail_chunk(st, &w, e.to_string()).await
        }
    };
    if let Err(e) = result {
        warn!(job = %w.job.id, chunk = w.chunk_idx, error = %e, "recording job chunk failed");
    }
}

async fn execute(st: &SharedState, w: &Work) -> Result<ChunkUpdate, ExecError> {
    let rows: Vec<(u32, JobRow)> = st
        .store
        .job_rows(&w.job.id, w.chunk.first_row, w.chunk.last_row)?
        .into_iter()
        .filter(|(_, r)| r.state == RowState::Pending)
        .collect();
    if rows.is_empty() {
        return Ok(ChunkUpdate::default());
    }
    match (w.job.kind, w.job.search_type) {
        (JobKind::RefreshNegatives, _) => {
            let plan = RefreshPlan::new(st, &rows)?;
            refresh::execute(st, &w.job, rows, plan).await
        }
        (_, SearchType::Oneline) => oneline(st, &w.job, rows).await,
        _ => geocode_batch(st, &w.job, rows).await,
    }
}

/// The upstream target for a job's batch uploads.
pub(crate) fn target(st: &AppState, job: &JobRecord) -> Result<BatchTarget, ExecError> {
    let catalog = st.catalog.load();
    let bench = catalog
        .benchmark(job.benchmark_id)
        .cloned()
        .ok_or_else(|| ExecError::Other(format!("unknown benchmark {}", job.benchmark_id)))?;
    let vintage = catalog
        .vintage(job.benchmark_id, job.vintage_id)
        .cloned()
        .ok_or_else(|| ExecError::Other(format!("unknown vintage {}", job.vintage_id)))?;
    let (endpoint, upstream_vintage) = match job.search_type {
        SearchType::Coordinates => (BatchEndpoint::Coordinates, true),
        _ if job.kind == JobKind::RefreshNegatives => {
            (BatchEndpoint::Address { geographies: true }, true)
        }
        _ => {
            let (_, endpoint, v) = address_plan(
                job.return_type,
                st.settings().config.cache.fetch_geographies_always,
            );
            (endpoint, v)
        }
    };
    Ok(BatchTarget {
        class: catalog.class(bench.id),
        bench,
        vintage,
        endpoint,
        upstream_vintage,
    })
}

/// Batch row objects for upload; the upstream id is the job row index + 1.
pub(crate) enum UploadRows {
    Address(Vec<(AddressRow, String)>),
    Coord(Vec<(CoordRow, String)>),
}

impl UploadRows {
    pub fn build<'a>(rows: impl IntoIterator<Item = (u32, &'a RowInput, &'a str)>) -> Self {
        let mut addr = Vec::new();
        let mut coord = Vec::new();
        for (i, input, key) in rows {
            let idx = i + 1;
            match input {
                RowInput::Address {
                    street,
                    city,
                    state,
                    zip,
                } => addr.push((
                    AddressRow {
                        idx,
                        user_id: String::new(),
                        street: street.clone(),
                        city: city.clone(),
                        state: state.clone(),
                        zip: zip.clone(),
                    },
                    key.to_owned(),
                )),
                RowInput::Coord { x, y } => {
                    if let Some((xf, yf)) = super::parse_coords(x, y) {
                        coord.push((
                            CoordRow {
                                idx,
                                user_id: String::new(),
                                x_raw: x.clone(),
                                y_raw: y.clone(),
                                x: xf,
                                y: yf,
                            },
                            key.to_owned(),
                        ));
                    }
                }
                RowInput::Oneline { .. } => {}
            }
        }
        if coord.is_empty() {
            Self::Address(addr)
        } else {
            Self::Coord(coord)
        }
    }

    /// Upload (deduplicated by key) and return the answers (see [`fetch_answers`]);
    /// nothing is stored until the chunk commits.
    pub async fn fetch(
        &self,
        st: &AppState,
        t: &BatchTarget,
        source: impl Fn(&str, Status) -> EntrySource,
    ) -> Result<Answers, ChunkError> {
        async fn go<R: Row>(
            st: &AppState,
            t: &BatchTarget,
            rows: &[(R, String)],
            source: impl Fn(&str, Status) -> EntrySource,
        ) -> Result<Answers, ChunkError> {
            let mut seen = HashSet::new();
            let unique: Vec<(&R, &str)> = rows
                .iter()
                .filter(|(_, k)| seen.insert(k.as_str()))
                .map(|(r, k)| (r, k.as_str()))
                .collect();
            if unique.is_empty() {
                return Ok(Answers::default());
            }
            fetch_answers(st, t, &unique, Priority::Background, source).await
        }
        match self {
            Self::Address(rows) => go(st, t, rows, source).await,
            Self::Coord(rows) => go(st, t, rows, source).await,
        }
    }
}

/// Address and coordinate rows: re-check the cache (a previous attempt may have
/// stored answers before it died), then upload the remaining misses as one batch.
/// The answers, their NoMatch re-checks and the row states commit together.
async fn geocode_batch(
    st: &AppState,
    job: &JobRecord,
    rows: Vec<(u32, JobRow)>,
) -> Result<ChunkUpdate, ExecError> {
    let t = target(st, job)?;
    let keys = rows.iter().map(|(_, r)| r.key.clone()).collect();
    let echoes = rows.iter().map(|(_, r)| r.input.echo()).collect();
    let lookups = batch::lookup(st, keys, echoes, job.batch_need(), t.class).await?;

    let mut update = ChunkUpdate::default();
    let mut misses = Vec::new();
    for ((i, row), l) in rows.iter().zip(&lookups) {
        match l {
            BatchLookup::Hit { result, .. } => {
                update
                    .rows
                    .push((*i, RowState::Cached, Some(result_status(result))));
            }
            BatchLookup::Miss => misses.push((*i, &row.input, row.key.as_str())),
        }
    }
    let Answers {
        results: fetched,
        entries,
    } = UploadRows::build(misses.iter().copied())
        .fetch(st, &t, |_, _| EntrySource::Job)
        .await?;
    let due = unix_now() + st.settings().config.retry.negative_delay.as_secs();
    update.enqueue = batch::negatives(&fetched, due);
    update.entries = entries;
    for (i, _, key) in misses {
        let Some(result) = fetched.get(key) else {
            return Err(ExecError::Other(format!("no answer for row {i}")));
        };
        update
            .rows
            .push((i, RowState::Done, Some(result_status(result))));
    }
    Ok(update)
}

/// Oneline rows have no batch endpoint: single-record lookups one by one. A 4xx for a
/// row fails that row only; transient failures fail the chunk (rows already answered
/// are in the cache, so the retry finds them).
async fn oneline(
    st: &SharedState,
    job: &JobRecord,
    rows: Vec<(u32, JobRow)>,
) -> Result<ChunkUpdate, ExecError> {
    let need = job.need();
    let class = st.catalog.load().class(job.benchmark_id);
    let fetch_geo = st.settings().config.cache.fetch_geographies_always
        || job.return_type == ReturnType::Geographies;
    let mut update = ChunkUpdate::default();
    for (i, row) in rows {
        let Some(input) = row.input.key_input() else {
            update.rows.push((i, RowState::Failed, None));
            continue;
        };
        let key = CacheKey::new(job.benchmark_id, job.vintage_id, &input);
        if let Ok(Some(bytes)) = st.store.get_entry(key.as_str()) {
            let cfg = st.settings();
            if let Lookup::Hit(s) = classify(
                Some(&bytes),
                st.codec,
                &cfg.config,
                class,
                need,
                SystemTime::now(),
            ) {
                update
                    .rows
                    .push((i, RowState::Cached, Some(s.entry.status)));
                continue;
            }
        }
        let fetch = FetchJob {
            state: Arc::clone(st),
            key,
            input,
            benchmark_id: job.benchmark_id,
            vintage_id: job.vintage_id,
            fetch_geo,
            need,
            class,
            prio: Priority::Background,
        };
        match fetch.run().await {
            Ok(s) => update.rows.push((i, RowState::Done, Some(s.entry.status))),
            // Census refused this row (4xx): retrying will not help.
            Err(FetchFailure::Forward(r)) if r.status.is_client_error() => {
                warn!(job = %job.id, row = i, status = %r.status, "upstream refused job row");
                update.rows.push((i, RowState::Failed, None));
            }
            // An unparseable 200 (or other non-4xx answer) may be transient: retry the chunk.
            Err(FetchFailure::Forward(r)) => {
                return Err(ExecError::Other(format!(
                    "unusable upstream answer (HTTP {}) for row {i}",
                    r.status
                )));
            }
            Err(FetchFailure::Exhausted(msg)) => return Err(ExecError::Other(msg)),
            Err(FetchFailure::BreakerOpen(d)) => {
                return Err(ExecError::Other(format!("breaker open for {d:?}")));
            }
        }
    }
    Ok(update)
}

/// Rows, chunk and totals (and retry-queue changes) in one transaction.
async fn commit(st: &AppState, w: &Work, update: ChunkUpdate) -> Result<(), StoreError> {
    let (id, idx) = (w.job.id.clone(), w.chunk_idx);
    let now = unix_now();
    let (codec, ttl_cfg) = (st.codec, st.settings().config.ttl.clone());
    let class = st.catalog.load().class(w.job.benchmark_id);
    let stamp = SystemTime::now();
    st.store
        .write(Box::new(move |ctx| {
            // Answers and retry progress belong to the keys: kept even if the job was
            // deleted meanwhile.
            let entries = ctx.entries()?;
            for (key, entry) in &update.entries {
                let existing = entries.get(key.as_str())?.map(|g| g.value().to_vec());
                if let Some(v) =
                    merge_batch(existing.as_deref(), entry, codec, &ttl_cfg, class, stamp)
                {
                    entries.insert(key.as_str(), v.as_slice())?;
                }
            }
            if !update.dequeue.is_empty() {
                ctx.retry_queue()?
                    .retain(|(_, key), _| !update.dequeue.contains(key))?;
            }
            let q = ctx.retry_queue()?;
            for (key, due, attempts) in &update.enqueue {
                q.insert((*due, key.as_str()), *attempts)?;
            }

            let Some(mut job) = ctx.job_record(&id)? else {
                return Ok(());
            };
            let Some(mut chunk) = ctx.job_chunk(&id, idx)? else {
                return Ok(());
            };
            if chunk.state != ChunkStatus::InFlight {
                return Ok(());
            }
            for (ri, state, status) in &update.rows {
                let Some(old) = ctx.job_row(&id, *ri)? else {
                    continue;
                };
                if old.state != RowState::Pending {
                    continue;
                }
                let new = JobRow {
                    state: *state,
                    status: *status,
                    ..old.clone()
                };
                job.totals.tally(&old, false);
                job.totals.tally(&new, true);
                ctx.put_job_row(&id, *ri, &new)?;
            }
            chunk.state = ChunkStatus::Done;
            chunk.lease_until = None;
            ctx.put_job_chunk(&id, idx, &chunk)?;
            job.updated_at = now;
            ctx.put_job_record(&job)
        }))
        .await
}

/// One more failed attempt; after `jobs.chunk_max_attempts` the chunk and its pending
/// rows are `Failed`.
async fn fail_chunk(st: &AppState, w: &Work, error: String) -> Result<(), StoreError> {
    let (id, idx) = (w.job.id.clone(), w.chunk_idx);
    let max = st.settings().config.jobs.chunk_max_attempts.max(1);
    let now = unix_now();
    st.store
        .write(Box::new(move |ctx| {
            let Some(mut job) = ctx.job_record(&id)? else {
                return Ok(());
            };
            let Some(mut chunk) = ctx.job_chunk(&id, idx)? else {
                return Ok(());
            };
            if chunk.state != ChunkStatus::InFlight {
                return Ok(());
            }
            chunk.attempts += 1;
            chunk.last_error = Some(error.clone());
            chunk.lease_until = None;
            if chunk.attempts >= max {
                chunk.state = ChunkStatus::Failed;
                for ri in chunk.first_row..=chunk.last_row {
                    if let Some(old) = ctx.job_row(&id, ri)?
                        && old.state == RowState::Pending
                    {
                        let new = JobRow {
                            state: RowState::Failed,
                            ..old.clone()
                        };
                        job.totals.tally(&old, false);
                        job.totals.tally(&new, true);
                        ctx.put_job_row(&id, ri, &new)?;
                    }
                }
            } else {
                chunk.state = ChunkStatus::Pending;
            }
            ctx.put_job_chunk(&id, idx, &chunk)?;
            job.error = Some(error.clone());
            job.updated_at = now;
            ctx.put_job_record(&job)
        }))
        .await
}
