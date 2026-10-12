//! Cache administration shared by the admin API (`http/api_cache.rs`) and the offline
//! CLI: stats, purge scope parsing, export paging, import and compaction. Blocking
//! functions here are called from `spawn_blocking` by the API.

use std::{
    collections::BTreeMap,
    time::{SystemTime, UNIX_EPOCH},
};

use serde::Serialize;
use tracing::warn;
use utoipa::ToSchema;

use super::{
    codec::Codec,
    envelope::{HEADER_LEN, Header, Status, decode_stored},
    key::CacheKey,
    maintenance::{Imported, PurgeCtx, PurgeScope, export_record, import_keeps_existing},
    views::decode,
};
use crate::{
    catalog::Catalog,
    state::AppState,
    store::{Store, StoreError, stats::Counter},
};

#[derive(Debug, Serialize, ToSchema)]
pub struct TableStatsView {
    pub tree_height: u32,
    pub leaf_pages: u64,
    pub branch_pages: u64,
    pub stored_bytes: u64,
    pub metadata_bytes: u64,
    pub fragmented_bytes: u64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CompressionSample {
    /// Entries decoded for the sample (the first ones in key order).
    pub sampled: u64,
    /// Stored envelope bytes of the sample.
    pub stored_bytes: u64,
    /// The same entries uncompressed (header + rmp payload).
    pub raw_bytes: u64,
    /// `raw_bytes / stored_bytes`.
    pub ratio: f64,
}

#[derive(Debug, Default, Serialize, ToSchema)]
pub struct StatsDetail {
    pub by_status: BTreeMap<String, u64>,
    pub by_searchtype: BTreeMap<String, u64>,
    pub by_benchmark: BTreeMap<String, u64>,
    /// Entries whose header cannot be read.
    pub undecodable: u64,
    pub compression: Option<CompressionSample>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CacheStats {
    pub entries: u64,
    pub file_bytes: u64,
    pub retry_queue_len: u64,
    /// `hits`, `misses`, `stale_hits`, `upstream_ok`, `upstream_err` (stored + unflushed).
    pub counters: BTreeMap<String, u64>,
    /// `hits / (hits + misses)`, when any request was counted.
    pub hit_ratio: Option<f64>,
    /// Jobs by state.
    pub jobs: BTreeMap<String, u64>,
    pub entries_table: TableStatsView,
    pub detail: Option<StatsDetail>,
}

const SAMPLE: u64 = 1000;
const PAGE: usize = 2000;

/// The JSON name of a serializable enum value (`"NoMatch"`, `"running"`).
fn name_of(v: impl Serialize) -> String {
    serde_json::to_value(v)
        .ok()
        .and_then(|v| v.as_str().map(str::to_owned))
        .unwrap_or_default()
}

/// Header-only scan in short read transactions (one page each), plus a compression
/// ratio sampled from the first entries.
pub fn stats_detail(store: &Store, codec: &Codec) -> Result<StatsDetail, StoreError> {
    let mut d = StatsDetail::default();
    let (mut sampled, mut stored, mut raw) = (0u64, 0u64, 0u64);
    let mut after: Option<String> = None;
    loop {
        let page = store.scan_entries("", after.as_deref(), PAGE)?;
        let Some((last, _)) = page.last() else {
            break;
        };
        after = Some(last.clone());
        for (key, bytes) in &page {
            if let Ok(k) = CacheKey::parse(key) {
                *d.by_searchtype
                    .entry(k.search_type.as_str().into())
                    .or_default() += 1;
                *d.by_benchmark
                    .entry(k.benchmark_id.to_string())
                    .or_default() += 1;
            }
            let Ok((envelope, _)) = decode_stored(bytes) else {
                d.undecodable += 1;
                continue;
            };
            let Ok(h) = Header::parse(&envelope) else {
                d.undecodable += 1;
                continue;
            };
            *d.by_status.entry(name_of(h.status)).or_default() += 1;
            if sampled < SAMPLE
                && let Ok(s) = decode(bytes, codec)
                && let Ok(payload) = rmp_serde::to_vec_named(&s.entry.payload)
            {
                sampled += 1;
                stored += envelope.len() as u64;
                raw += (HEADER_LEN + payload.len()) as u64;
            }
        }
        if page.len() < PAGE {
            break;
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let ratio = if stored == 0 {
        0.0
    } else {
        raw as f64 / stored as f64
    };
    d.compression = (sampled > 0).then_some(CompressionSample {
        sampled,
        stored_bytes: stored,
        raw_bytes: raw,
        ratio,
    });
    Ok(d)
}

/// Entry counts, file size, counters (stored + unflushed), hit ratio, retry queue, jobs
/// by state and redb entries-table statistics; `full` adds [`stats_detail`]. Blocking.
pub fn cache_stats(st: &AppState, full: bool) -> Result<CacheStats, StoreError> {
    let totals = st.store.stat_totals()?;
    let counters: BTreeMap<String, u64> = Counter::ALL
        .iter()
        .map(|c| {
            let stored = totals.get(c.name()).copied().unwrap_or(0);
            (c.name().to_owned(), stored + st.stats.pending(*c))
        })
        .collect();
    let (hits, misses) = (counters["hits"], counters["misses"]);
    #[allow(clippy::cast_precision_loss)]
    let hit_ratio = (hits + misses > 0).then(|| hits as f64 / (hits + misses) as f64);
    let mut jobs: BTreeMap<String, u64> = BTreeMap::new();
    for j in st.store.job_list()? {
        *jobs.entry(name_of(j.state)).or_default() += 1;
    }
    let t = st.store.entries_table_stats()?;
    Ok(CacheStats {
        entries: st.store.entry_count()?,
        file_bytes: st.store.file_size()?,
        retry_queue_len: st.store.retry_len()?,
        counters,
        hit_ratio,
        jobs,
        entries_table: TableStatsView {
            tree_height: t.tree_height(),
            leaf_pages: t.leaf_pages(),
            branch_pages: t.branch_pages(),
            stored_bytes: t.stored_bytes(),
            metadata_bytes: t.metadata_bytes(),
            fragmented_bytes: t.fragmented_bytes(),
        },
        detail: if full {
            Some(stats_detail(&st.store, st.codec)?)
        } else {
            None
        },
    })
}

/// `Match`, `NoMatch`/`No_Match`, `Tie` (any case).
pub fn parse_status(s: &str) -> Option<Status> {
    match s.to_ascii_lowercase().replace('_', "").as_str() {
        "match" => Some(Status::Match),
        "nomatch" => Some(Status::NoMatch),
        "tie" => Some(Status::Tie),
        _ => None,
    }
}

/// A purge scope from its name and value: `all`, `expired`, `benchmark` (id or name),
/// `vintage` (`benchmark:vintage`), `status`, `prefix` (matched exactly; trailing
/// spaces are part of it).
pub fn parse_scope(
    catalog: &Catalog,
    scope: &str,
    value: Option<&str>,
) -> Result<PurgeScope, String> {
    let value = if scope == "prefix" {
        value.filter(|v| !v.is_empty())
    } else {
        value.map(str::trim).filter(|v| !v.is_empty())
    };
    let need = || value.ok_or_else(|| format!("scope `{scope}` needs a value"));
    let bench = |b: &str| {
        catalog
            .resolve_benchmark(b)
            .map(|b| b.id)
            .ok_or_else(|| format!("unknown benchmark `{b}`"))
    };
    Ok(match scope {
        "all" => PurgeScope::All,
        "expired" => PurgeScope::Expired,
        "benchmark" => PurgeScope::Benchmark(bench(need()?)?),
        "vintage" => {
            let (b, v) = need()?
                .split_once(':')
                .ok_or("vintage value must be `benchmark:vintage`")?;
            let benchmark_id = bench(b)?;
            let vintage_id = catalog
                .resolve_vintage(benchmark_id, v)
                .map(|v| v.id)
                .ok_or_else(|| format!("unknown vintage `{v}` for benchmark {benchmark_id}"))?;
            PurgeScope::Vintage {
                benchmark_id,
                vintage_id,
            }
        }
        "status" => {
            PurgeScope::Status(parse_status(need()?).ok_or("status must be Match, NoMatch or Tie")?)
        }
        "prefix" => PurgeScope::Prefix(need()?.to_owned()),
        other => return Err(format!("unknown scope `{other}`")),
    })
}

/// The expiry inputs for a purge: current TTLs and each benchmark's class.
pub fn purge_context(st: &AppState) -> PurgeCtx {
    let catalog = st.catalog.load();
    PurgeCtx {
        ttl: st.settings().config.ttl.clone(),
        classes: catalog
            .benchmarks()
            .iter()
            .map(|b| (b.id, catalog.class(b.id)))
            .collect(),
        now: SystemTime::now(),
    }
}

pub const EXPORT_PAGE: usize = 500;

/// Export entries under `prefix` as JSON lines, one short read transaction per page so
/// a long export never blocks compaction. `sink` receives each page's lines and returns
/// `false` to stop early. Undecodable entries are skipped with a warning. Returns the
/// number of records written. Blocking.
pub fn export_pages(
    store: &Store,
    codec: &Codec,
    prefix: &str,
    mut sink: impl FnMut(Vec<u8>) -> bool,
) -> Result<u64, StoreError> {
    let mut after: Option<String> = None;
    let mut records = 0u64;
    loop {
        let page = store.scan_entries(prefix, after.as_deref(), EXPORT_PAGE)?;
        let mut chunk = Vec::new();
        for (k, v) in &page {
            match export_record(k, v, codec) {
                Ok(rec) => {
                    if serde_json::to_writer(&mut chunk, &rec).is_ok() {
                        chunk.push(b'\n');
                        records += 1;
                    }
                }
                Err(e) => warn!(key = %k, error = %e, "export: skipping undecodable entry"),
            }
        }
        if !chunk.is_empty() && !sink(chunk) {
            return Ok(records);
        }
        match page.last() {
            Some((k, _)) if page.len() == EXPORT_PAGE => after = Some(k.clone()),
            _ => return Ok(records),
        }
    }
}

const IMPORT_BATCH: usize = 1000;

/// Store imported records in batches. An existing newer entry is kept; restored
/// batch/job NoMatch entries get their re-check queued in the same transaction.
pub async fn store_imported(st: &AppState, records: Vec<Imported>) -> Result<(), StoreError> {
    let due = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
        + st.settings().config.retry.negative_delay.as_secs();
    let mut items = records;
    while !items.is_empty() {
        let rest = items.split_off(items.len().min(IMPORT_BATCH));
        let batch = std::mem::replace(&mut items, rest);
        let retries = batch
            .iter()
            .filter(|i| i.needs_retry)
            .map(|i| (i.key.clone(), due, 0))
            .collect();
        let batch = batch.into_iter().map(|i| (i.key, i.stored)).collect();
        st.store
            .put_entries_with(
                batch,
                |existing, new: &Vec<u8>| {
                    if existing.is_some_and(|e| import_keeps_existing(e, new)) {
                        None
                    } else {
                        Some(new.clone())
                    }
                },
                retries,
            )
            .await?;
    }
    Ok(())
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CompactResponse {
    /// `false`: there was nothing to reclaim.
    pub compacted: bool,
    pub file_bytes_before: u64,
    pub file_bytes_after: u64,
    pub millis: u64,
}

/// Reclaim free space. Blocks reads and writes while it runs. Blocking.
pub fn compact(store: &Store) -> Result<CompactResponse, StoreError> {
    let before = store.file_size()?;
    let start = std::time::Instant::now();
    let compacted = store.compact()?;
    Ok(CompactResponse {
        compacted,
        file_bytes_before: before,
        file_bytes_after: store.file_size()?,
        millis: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
    })
}
