//! Batch engine shared by the batch endpoints (PLAN.md §6.2) and async jobs (§6.3):
//! cache lookup of many rows, and upload of one chunk of misses with strict parsing,
//! completeness checking and merge-inside-the-write-transaction storage.

use std::{
    collections::HashMap,
    time::{SystemTime, UNIX_EPOCH},
};

use tracing::warn;

use crate::{
    cache::{
        envelope::{BatchView, Entry, EntryPayload, EntrySource, Status, Views},
        key::KeyInput,
        policy::BenchmarkClass,
        views::{BatchLookup, BatchNeed, classify_batch, merge_batch},
    },
    catalog::{Benchmark, Vintage},
    census::batch_csv::{
        AddressRow, CoordRow, RowResult, UpstreamRow, parse_upstream_addresses,
        parse_upstream_coordinates, upload_addresses, upload_coordinates, write_address_row,
        write_coord_row,
    },
    state::AppState,
    store::{StoreError, stats::Counter},
    upstream::{UpstreamError, batch::BatchEndpoint, throttle::Priority},
};

/// One input row kind (addresses or coordinates).
pub trait Row: Send + Sync + 'static {
    /// The id sent upstream; unique within one upload.
    fn idx(&self) -> u32;
    fn key_input(&self) -> KeyInput;
    /// `street, city, state, zip` for the match-type heuristic (empty for coordinates).
    fn echo(&self) -> String;
    fn upload<'a>(rows: impl IntoIterator<Item = &'a Self>) -> Vec<u8>
    where
        Self: 'a;
    /// `geographies`: the upload went to the geographies endpoint (wider rows).
    fn parse_upstream(body: &[u8], geographies: bool) -> Result<Vec<UpstreamRow>, String>;
    fn write(
        &self,
        w: &mut csv::Writer<Vec<u8>>,
        result: &RowResult,
        geographies: bool,
    ) -> csv::Result<()>;
}

impl Row for AddressRow {
    fn idx(&self) -> u32 {
        self.idx
    }
    fn key_input(&self) -> KeyInput {
        KeyInput::Address {
            street: self.street.clone(),
            city: self.city.clone(),
            state: self.state.clone(),
            zip: self.zip.clone(),
        }
    }
    fn echo(&self) -> String {
        AddressRow::echo(self)
    }
    fn upload<'a>(rows: impl IntoIterator<Item = &'a Self>) -> Vec<u8> {
        upload_addresses(rows)
    }
    fn parse_upstream(body: &[u8], geographies: bool) -> Result<Vec<UpstreamRow>, String> {
        parse_upstream_addresses(body, geographies)
    }
    fn write(
        &self,
        w: &mut csv::Writer<Vec<u8>>,
        result: &RowResult,
        geographies: bool,
    ) -> csv::Result<()> {
        write_address_row(w, &self.user_id, &self.echo(), result, geographies)
    }
}

impl Row for CoordRow {
    fn idx(&self) -> u32 {
        self.idx
    }
    fn key_input(&self) -> KeyInput {
        KeyInput::Coordinates {
            x: self.x,
            y: self.y,
        }
    }
    fn echo(&self) -> String {
        String::new()
    }
    fn upload<'a>(rows: impl IntoIterator<Item = &'a Self>) -> Vec<u8> {
        upload_coordinates(rows)
    }
    fn parse_upstream(body: &[u8], _geographies: bool) -> Result<Vec<UpstreamRow>, String> {
        parse_upstream_coordinates(body)
    }
    fn write(
        &self,
        w: &mut csv::Writer<Vec<u8>>,
        result: &RowResult,
        _geographies: bool,
    ) -> csv::Result<()> {
        write_coord_row(w, self, result)
    }
}

/// Where one batch's misses go and how its entries are keyed.
#[derive(Debug, Clone)]
pub struct BatchTarget {
    pub bench: Benchmark,
    /// The vintage in the cache keys (the default vintage for `locations`).
    pub vintage: Vintage,
    pub endpoint: BatchEndpoint,
    /// Send `vintage` upstream (geographies endpoints only).
    pub upstream_vintage: bool,
    pub class: BenchmarkClass,
}

impl BatchTarget {
    fn upstream_geographies(&self) -> bool {
        !matches!(self.endpoint, BatchEndpoint::Address { geographies: false })
    }
}

/// Why one chunk upload produced no usable answer.
#[derive(Debug, thiserror::Error)]
pub enum ChunkError {
    #[error(transparent)]
    Upstream(#[from] UpstreamError),
    #[error("upstream batch response was not understood: {0}")]
    Unparseable(String),
    #[error("upstream batch response omitted {missing} of {submitted} rows")]
    Incomplete { missing: usize, submitted: usize },
}

pub fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

/// Classify `keys` (with their rows' heuristic echoes) against the store, off the
/// async workers: decoding thousands of entries is CPU work.
pub async fn lookup(
    st: &AppState,
    keys: Vec<String>,
    echoes: Vec<String>,
    need: BatchNeed,
    class: BenchmarkClass,
) -> anyhow::Result<Vec<BatchLookup>> {
    let (store, codec, cfg) = (st.store.clone(), st.codec, st.settings().config.clone());
    let out = tokio::task::spawn_blocking(move || {
        let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
        let stored = store.get_entries(&refs)?;
        let now = SystemTime::now();
        Ok::<_, StoreError>(
            stored
                .iter()
                .zip(&echoes)
                .map(|(bytes, echo)| {
                    classify_batch(bytes.as_deref(), codec, &cfg, class, need, now, echo)
                })
                .collect(),
        )
    })
    .await
    .map_err(|e| anyhow::anyhow!("batch lookup task: {e}"))??;
    Ok(out)
}

/// Upload one chunk of rows (`(row, key)`, distinct keys), store every answer with
/// `source(key, status)` as its provenance, and return the answers by key.
///
/// Upstream output is parsed strictly: an unparseable record or a submitted row missing
/// from the response fails the whole chunk (never a fabricated `No_Match`). Storage
/// failures are logged and the answers still returned.
pub async fn fetch_chunk<R: Row>(
    st: &AppState,
    t: &BatchTarget,
    rows: &[(&R, &str)],
    prio: Priority,
    source: impl Fn(&str, Status) -> EntrySource,
) -> Result<HashMap<String, RowResult>, ChunkError> {
    let csv = R::upload(rows.iter().map(|(r, _)| *r));
    let vintage_param = t.upstream_vintage.then_some(t.vintage.id);
    let resp = match st
        .upstream
        .fetch_batch(prio, t.endpoint, t.bench.id, vintage_param, csv)
        .await
    {
        Ok(r) => r,
        Err(e) => {
            if matches!(e, UpstreamError::Exhausted { .. }) {
                st.stats.record(Counter::UpstreamErr);
            }
            return Err(e.into());
        }
    };
    st.stats.record(Counter::UpstreamOk);
    let parsed = R::parse_upstream(&resp.body, t.upstream_geographies()).map_err(|e| {
        warn!(error = %e, "upstream batch output not understood");
        st.stats.record(Counter::UpstreamErr);
        ChunkError::Unparseable(e)
    })?;
    let by_idx: HashMap<u32, UpstreamRow> = parsed.into_iter().map(|u| (u.idx, u)).collect();
    // Every submitted row must be answered: an absent line is an upstream failure,
    // never evidence of No_Match.
    let missing = rows
        .iter()
        .filter(|(r, _)| !by_idx.contains_key(&r.idx()))
        .count();
    if missing > 0 {
        warn!(
            missing,
            submitted = rows.len(),
            "upstream batch response omitted rows"
        );
        st.stats.record(Counter::UpstreamErr);
        return Err(ChunkError::Incomplete {
            missing,
            submitted: rows.len(),
        });
    }

    let now = SystemTime::now();
    let verified_at = unix_now();
    let mut results = HashMap::with_capacity(rows.len());
    let mut to_store: Vec<(String, Entry)> = Vec::with_capacity(rows.len());
    for (row, key) in rows {
        let Some(u) = by_idx.get(&row.idx()) else {
            continue;
        };
        let result = match u.status {
            Status::Match => RowResult::Match(Box::new(u.view.clone())),
            Status::NoMatch => RowResult::NoMatch,
            Status::Tie => RowResult::Tie,
        };
        to_store.push((
            (*key).to_owned(),
            Entry {
                status: u.status,
                source: source(key, u.status),
                views: Views::BATCH,
                verified_at,
                payload: EntryPayload {
                    json: None,
                    batch: Some(if u.status == Status::Match {
                        u.view.clone()
                    } else {
                        BatchView::default()
                    }),
                    benchmark_name: t.bench.name.clone(),
                    vintage_name: t.vintage.name.clone(),
                },
            },
        ));
        results.insert((*key).to_owned(), result);
    }
    let (codec, ttl_cfg, class) = (st.codec, st.settings().config.ttl.clone(), t.class);
    if let Err(e) = st
        .store
        .put_entries_with(to_store, move |existing, entry| {
            merge_batch(existing, entry, codec, &ttl_cfg, class, now)
        })
        .await
    {
        warn!(error = %e, "storing batch results failed; serving uncached");
    }
    Ok(results)
}

/// Queue batch/job NoMatch keys for a later re-check (`retry.negative_delay`).
pub async fn enqueue_negatives(st: &AppState, keys: Vec<String>) {
    let due = unix_now() + st.settings().config.retry.negative_delay.as_secs();
    let items = keys.into_iter().map(|k| (k, due, 0)).collect();
    if let Err(e) = st.store.retry_enqueue_many(items).await {
        warn!(error = %e, "enqueueing No_Match retries failed");
    }
}
