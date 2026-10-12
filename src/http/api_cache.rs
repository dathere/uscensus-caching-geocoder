//! `/api/v1/cache/*` (PLAN.md §8): stats, entry listing/inspection, purge, export,
//! import and compaction. Logic lives in `cache::maintenance` and the store; these are
//! thin handlers.

use std::{
    collections::{BTreeMap, HashMap},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json,
    body::{Body, Bytes},
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
};
use serde::{Deserialize, Serialize};
use serde_json::value::RawValue;
use tracing::warn;
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use super::{
    api_jobs::{JobView, inputs_from_upload},
    batch::{Upload, bad_request, resolve},
};
use crate::{
    cache::{
        envelope::{BatchView, EntrySource, HEADER_LEN, Header, Status, Views, decode_stored},
        key::{CacheKey, KEY_VERSION, SearchType},
        maintenance::{
            PurgeCtx, PurgeScope, export_record, import_keeps_existing, parse_import, purge,
        },
        policy::{Freshness, freshness, ttl},
        views::decode,
    },
    census::types::ReturnType,
    error::AppError,
    jobs::{
        JobKind,
        submit::{self, JobSpec, SubmitError},
    },
    state::SharedState,
    store::stats::Counter,
};

pub const TAG: &str = "cache";

pub fn router() -> OpenApiRouter<SharedState> {
    OpenApiRouter::new()
        .routes(routes!(stats))
        .routes(routes!(list_entries))
        .routes(routes!(get_entry, delete_entry))
        .routes(routes!(purge_entries))
        .routes(routes!(export))
        .routes(routes!(import))
        .routes(routes!(compact))
}

fn internal(e: impl Into<anyhow::Error>) -> Response {
    AppError::Internal(e.into()).into_response()
}

fn unix(t: SystemTime) -> u64 {
    t.duration_since(UNIX_EPOCH).map_or(0, |d| d.as_secs())
}

fn view_names(v: Views) -> Vec<String> {
    [
        (Views::JSON, "json"),
        (Views::GEO, "geo"),
        (Views::BATCH, "batch"),
    ]
    .into_iter()
    .filter(|(f, _)| v.contains(*f))
    .map(|(_, n)| n.to_owned())
    .collect()
}

fn purge_ctx(st: &SharedState) -> PurgeCtx {
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

// -------------------------------------------------------------------- stats

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatsQuery {
    /// `full`: also scan every entry header for per-status/searchtype/benchmark counts
    /// and sample the compression ratio (slower; the default is cheap enough to poll).
    pub detail: Option<String>,
}

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

/// Header-only scan in short read transactions (one page each).
fn stats_detail(st: &SharedState) -> anyhow::Result<StatsDetail> {
    let mut d = StatsDetail::default();
    let (mut sampled, mut stored, mut raw) = (0u64, 0u64, 0u64);
    let mut after: Option<String> = None;
    loop {
        let page = st.store.scan_entries("", after.as_deref(), PAGE)?;
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
            let status = serde_json::to_value(h.status)
                .ok()
                .and_then(|v| v.as_str().map(str::to_owned))
                .unwrap_or_default();
            *d.by_status.entry(status).or_default() += 1;
            if sampled < SAMPLE
                && let Ok(s) = decode(bytes, st.codec)
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

/// Entry counts, file size, hit ratio, retry queue, jobs and redb table statistics.
#[utoipa::path(
    get,
    path = "/api/v1/cache/stats",
    tag = TAG,
    params(StatsQuery),
    responses((status = 200, body = CacheStats))
)]
async fn stats(State(st): State<SharedState>, Query(q): Query<StatsQuery>) -> Response {
    let full = q.detail.as_deref() == Some("full");
    let st2 = st.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<CacheStats> {
        let st = st2;
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
            let name = serde_json::to_value(j.state)?
                .as_str()
                .unwrap_or_default()
                .to_owned();
            *jobs.entry(name).or_default() += 1;
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
            detail: if full { Some(stats_detail(&st)?) } else { None },
        })
    })
    .await;
    match result {
        Ok(Ok(s)) => Json(s).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("stats task: {e}")),
    }
}

// ------------------------------------------------------------------ entries

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct EntriesQuery {
    /// Key prefix (e.g. `v1|address|4|`); combined with nothing else.
    pub prefix: Option<String>,
    /// `oneline`, `address`, `addressPR` or `coordinates`.
    pub searchtype: Option<String>,
    /// Benchmark id or name (with `searchtype`).
    pub benchmark: Option<String>,
    /// `Match`, `NoMatch` or `Tie`.
    pub status: Option<String>,
    /// Page size (default 50, max 1000).
    pub limit: Option<usize>,
    /// `next_cursor` of the previous page.
    pub cursor: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntrySummary {
    pub key: String,
    pub status: Status,
    pub source: EntrySource,
    pub views: Vec<String>,
    /// Unix seconds.
    pub created_at: u64,
    pub verified_at: u64,
    /// Unix seconds when it stops being fresh (absent: never expires).
    pub expires_at: Option<u64>,
    pub fresh: bool,
    pub stored_bytes: usize,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntryPage {
    pub entries: Vec<EntrySummary>,
    /// Pass back as `cursor`; absent at the end. It is the last key *examined*, so
    /// filtered pages may be short (even empty) while it still advances.
    pub next_cursor: Option<String>,
}

fn parse_status(s: &str) -> Option<Status> {
    match s.to_ascii_lowercase().replace('_', "").as_str() {
        "match" => Some(Status::Match),
        "nomatch" => Some(Status::NoMatch),
        "tie" => Some(Status::Tie),
        _ => None,
    }
}

fn summary(st: &SharedState, key: &str, bytes: &[u8]) -> Option<EntrySummary> {
    let (envelope, created_at) = decode_stored(bytes).ok()?;
    let h = Header::parse(&envelope).ok()?;
    let class = CacheKey::parse(key)
        .map(|k| st.catalog.load().class(k.benchmark_id))
        .ok()?;
    let life = ttl(&st.settings().config.ttl, h.status, h.source, class);
    Some(EntrySummary {
        key: key.to_owned(),
        status: h.status,
        source: h.source,
        views: view_names(h.views),
        created_at: unix(created_at),
        verified_at: h.verified_at,
        expires_at: life.map(|d| unix(created_at + d)),
        fresh: freshness(created_at, SystemTime::now(), life, false) == Freshness::Fresh,
        stored_bytes: bytes.len(),
    })
}

/// Examined at most per request, so a sparse `status` filter cannot scan the whole table.
const EXAMINE_MAX: usize = 10_000;

/// Paginated entry listing (header fields only).
#[utoipa::path(
    get,
    path = "/api/v1/cache/entries",
    tag = TAG,
    params(EntriesQuery),
    responses((status = 200, body = EntryPage), (status = 400, description = "Bad filter"))
)]
async fn list_entries(State(st): State<SharedState>, Query(q): Query<EntriesQuery>) -> Response {
    let limit = q.limit.unwrap_or(50).clamp(1, 1000);
    let status = match q.status.as_deref() {
        None => None,
        Some(s) => match parse_status(s) {
            Some(s) => Some(s),
            None => return *bad_request("status must be Match, NoMatch or Tie"),
        },
    };
    let prefix = match (q.prefix, q.searchtype.as_deref(), q.benchmark.as_deref()) {
        (Some(p), _, _) => p,
        (None, None, None) => String::new(),
        (None, Some(stype), b) => {
            let Some(stype) = SearchType::parse(stype) else {
                return *bad_request("unknown searchtype");
            };
            match b {
                None => format!("{KEY_VERSION}|{stype}|"),
                Some(b) => match st.catalog.load().resolve_benchmark(b) {
                    Some(b) => CacheKey::benchmark_prefix(stype, b.id),
                    None => return *bad_request(super::batch::MSG_INVALID_BENCHMARK),
                },
            }
        }
        (None, None, Some(_)) => return *bad_request("benchmark requires searchtype"),
    };
    let st2 = st.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<EntryPage> {
        let mut entries = Vec::new();
        let mut cursor = q.cursor;
        let mut examined = 0usize;
        let mut exhausted = false;
        while entries.len() < limit && examined < EXAMINE_MAX {
            let want = (limit - entries.len()).max(64).min(EXAMINE_MAX - examined);
            let page = st2.store.scan_entries(&prefix, cursor.as_deref(), want)?;
            let mut stopped_early = false;
            for (i, (k, v)) in page.iter().enumerate() {
                examined += 1;
                cursor = Some(k.clone());
                let Some(s) = summary(&st2, k, v) else {
                    continue;
                };
                if status.is_none_or(|want| s.status == want) {
                    entries.push(s);
                    if entries.len() == limit {
                        stopped_early = i + 1 < page.len();
                        break;
                    }
                }
            }
            // Only a short page read to its end means there is nothing after the cursor.
            if !stopped_early && page.len() < want {
                exhausted = true;
                break;
            }
        }
        Ok(EntryPage {
            entries,
            next_cursor: if exhausted { None } else { cursor },
        })
    })
    .await;
    match result {
        Ok(Ok(p)) => Json(p).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("listing task: {e}")),
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct EntryDetail {
    #[serde(flatten)]
    pub summary: EntrySummary,
    /// The stored upstream JSON, verbatim: the `addressMatches` array (address
    /// searches) or the `geographies` object (coordinates).
    #[schema(value_type = Option<Value>)]
    pub json: Option<Box<RawValue>>,
    pub batch: Option<BatchView>,
    pub benchmark_name: String,
    pub vintage_name: String,
}

fn not_found() -> Response {
    AppError::NotFound("No such cache entry".into()).into_response()
}

/// One decoded entry. Percent-encode the key (it contains `|`, `;`, spaces, maybe `/`).
#[utoipa::path(
    get,
    path = "/api/v1/cache/entries/{key}",
    tag = TAG,
    params(("key" = String, Path, description = "Cache key, percent-encoded")),
    responses((status = 200, body = EntryDetail), (status = 404, description = "No such entry"))
)]
async fn get_entry(State(st): State<SharedState>, Path(key): Path<String>) -> Response {
    let bytes = match st.store.get_entry(&key) {
        Ok(Some(b)) => b,
        Ok(None) => return not_found(),
        Err(e) => return internal(e),
    };
    let (Some(summary), Ok(rec)) = (
        summary(&st, &key, &bytes),
        export_record(&key, &bytes, st.codec),
    ) else {
        return AppError::Conflict("The stored entry cannot be decoded".into()).into_response();
    };
    Json(EntryDetail {
        summary,
        json: rec.json,
        batch: rec.batch,
        benchmark_name: rec.benchmark_name,
        vintage_name: rec.vintage_name,
    })
    .into_response()
}

/// Delete one entry.
#[utoipa::path(
    delete,
    path = "/api/v1/cache/entries/{key}",
    tag = TAG,
    params(("key" = String, Path, description = "Cache key, percent-encoded")),
    responses((status = 204, description = "Deleted"), (status = 404, description = "No such entry"))
)]
async fn delete_entry(State(st): State<SharedState>, Path(key): Path<String>) -> Response {
    match st.store.delete_entry(key).await {
        Ok(true) => StatusCode::NO_CONTENT.into_response(),
        Ok(false) => not_found(),
        Err(e) => internal(e),
    }
}

// -------------------------------------------------------------------- purge

#[derive(Debug, Deserialize, ToSchema)]
pub struct PurgeRequest {
    /// `all`, `expired`, `benchmark`, `vintage`, `status` or `prefix`.
    pub scope: String,
    /// `benchmark`: id or name; `vintage`: `benchmark:vintage` (ids or names);
    /// `status`: `Match`/`NoMatch`/`Tie`; `prefix`: the key prefix.
    pub value: Option<String>,
    /// Count only.
    #[serde(default)]
    pub dry_run: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PurgeResponse {
    pub scope: String,
    pub dry_run: bool,
    /// Entries the scope selects (deleted unless `dry_run`).
    pub matched: u64,
}

fn purge_scope(st: &SharedState, req: &PurgeRequest) -> Result<PurgeScope, String> {
    // A prefix is matched exactly (trailing spaces are part of it); other values are
    // identifiers.
    let value = if req.scope == "prefix" {
        req.value.as_deref().filter(|v| !v.is_empty())
    } else {
        req.value
            .as_deref()
            .map(str::trim)
            .filter(|v| !v.is_empty())
    };
    let need = || value.ok_or_else(|| format!("scope `{}` needs a value", req.scope));
    let catalog = st.catalog.load();
    let bench = |b: &str| {
        catalog
            .resolve_benchmark(b)
            .map(|b| b.id)
            .ok_or_else(|| format!("unknown benchmark `{b}`"))
    };
    Ok(match req.scope.as_str() {
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

/// Delete (or with `dry_run`, count) entries by scope. Per-entry hit counters and retry
/// queue items of purged keys are left in place (a refresh settles a missing entry).
#[utoipa::path(
    post,
    path = "/api/v1/cache/purge",
    tag = TAG,
    request_body = PurgeRequest,
    responses((status = 200, body = PurgeResponse), (status = 400, description = "Bad scope or value"))
)]
async fn purge_entries(State(st): State<SharedState>, Json(req): Json<PurgeRequest>) -> Response {
    let scope = match purge_scope(&st, &req) {
        Ok(s) => s,
        Err(msg) => return AppError::BadRequest(msg).into_response(),
    };
    match purge(&st.store, scope, purge_ctx(&st), req.dry_run).await {
        Ok(matched) => Json(PurgeResponse {
            scope: req.scope,
            dry_run: req.dry_run,
            matched,
        })
        .into_response(),
        Err(e) => internal(e),
    }
}

// ------------------------------------------------------------ export/import

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ExportQuery {
    /// Only keys starting with this prefix.
    pub prefix: Option<String>,
}

const EXPORT_PAGE: usize = 500;

/// Stream entries as JSON lines (decoded; importable into any build).
#[utoipa::path(
    get,
    path = "/api/v1/cache/export",
    tag = TAG,
    params(ExportQuery),
    responses((status = 200, description = "JSON lines of `ExportRecord`", content_type = "application/x-ndjson", body = String))
)]
async fn export(State(st): State<SharedState>, Query(q): Query<ExportQuery>) -> Response {
    let prefix = q.prefix.unwrap_or_default();
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Bytes, std::io::Error>>(4);
    let (store, codec) = (st.store.clone(), st.codec);
    // Short read transactions, one page each, so a long export never blocks compaction.
    tokio::task::spawn_blocking(move || {
        let mut after: Option<String> = None;
        loop {
            let page = match store.scan_entries(&prefix, after.as_deref(), EXPORT_PAGE) {
                Ok(p) => p,
                Err(e) => {
                    warn!(error = %e, "export aborted");
                    // Fail the body so the client sees a truncated download.
                    let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
                    return;
                }
            };
            let mut chunk = Vec::new();
            for (k, v) in &page {
                match export_record(k, v, codec) {
                    Ok(rec) => {
                        if serde_json::to_writer(&mut chunk, &rec).is_ok() {
                            chunk.push(b'\n');
                        }
                    }
                    Err(e) => warn!(key = %k, error = %e, "export: skipping undecodable entry"),
                }
            }
            if !chunk.is_empty() && tx.blocking_send(Ok(Bytes::from(chunk))).is_err() {
                return; // client went away
            }
            match page.last() {
                Some((k, _)) if page.len() == EXPORT_PAGE => after = Some(k.clone()),
                _ => return,
            }
        }
    });
    let stream = futures_util::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    });
    let mut resp = Body::from_stream(stream).into_response();
    let h = resp.headers_mut();
    h.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/x-ndjson"),
    );
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=ucgeocoder-export.jsonl"),
    );
    resp
}

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct ImportQuery {
    /// `jsonl` (default): restore exported entries. `csv`: geocode the rows as a cache
    /// warming job (`returntype`, `benchmark`, `vintage`, `searchtype` as for jobs).
    pub format: Option<String>,
    pub returntype: Option<String>,
    pub benchmark: Option<String>,
    pub vintage: Option<String>,
    /// `address` (default), `onelineaddress` or `coordinates`.
    pub searchtype: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportError {
    /// 1-based line number.
    pub line: usize,
    pub error: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ImportResponse {
    pub format: String,
    /// Valid records restored (an existing newer entry is kept instead).
    pub records: u64,
    pub invalid: u64,
    /// The first invalid lines.
    pub errors: Vec<ImportError>,
    /// The warming job (`csv`).
    pub job: Option<JobView>,
}

/// Restore exported JSON lines, or warm the cache from a CSV via a `warm` job. The body
/// is bounded by `server.max_body_mb`.
#[utoipa::path(
    post,
    path = "/api/v1/cache/import",
    tag = TAG,
    params(ImportQuery),
    request_body(content(
        (String = "application/x-ndjson"),
        (String = "text/csv")
    )),
    responses(
        (status = 200, description = "Entries restored", body = ImportResponse),
        (status = 201, description = "Warming job created", body = ImportResponse),
        (status = 400, description = "Bad parameters or file")
    )
)]
async fn import(
    State(st): State<SharedState>,
    Query(q): Query<ImportQuery>,
    body: Bytes,
) -> Response {
    match q.format.as_deref().unwrap_or("jsonl") {
        "jsonl" => import_jsonl(st, body).await,
        "csv" => import_csv(st, q, body).await,
        _ => AppError::BadRequest("format must be jsonl or csv".into()).into_response(),
    }
}

const IMPORT_BATCH: usize = 1000;

async fn import_jsonl(st: SharedState, body: Bytes) -> Response {
    let codec = st.codec;
    let parsed = tokio::task::spawn_blocking(move || parse_import(&body, codec, 50)).await;
    let parsed = match parsed {
        Ok(p) => p,
        Err(e) => return internal(anyhow::anyhow!("import task: {e}")),
    };
    let (invalid, errors) = (
        parsed.invalid,
        parsed
            .errors
            .into_iter()
            .map(|(line, error)| ImportError { line, error })
            .collect(),
    );
    let items = parsed.records;
    let records = items.len() as u64;
    let due = unix(SystemTime::now()) + st.settings().config.retry.negative_delay.as_secs();
    let mut items = items;
    while !items.is_empty() {
        let rest = items.split_off(items.len().min(IMPORT_BATCH));
        let batch = std::mem::replace(&mut items, rest);
        // Restored batch/job NoMatch entries keep their pending re-check.
        let retries = batch
            .iter()
            .filter(|i| i.needs_retry)
            .map(|i| (i.key.clone(), due, 0))
            .collect();
        let batch = batch.into_iter().map(|i| (i.key, i.stored)).collect();
        let written = st
            .store
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
            .await;
        if let Err(e) = written {
            return internal(e);
        }
    }
    Json(ImportResponse {
        format: "jsonl".into(),
        records,
        invalid,
        errors,
        job: None,
    })
    .into_response()
}

async fn import_csv(st: SharedState, q: ImportQuery, body: Bytes) -> Response {
    let Some(rt) = q.returntype.as_deref().and_then(ReturnType::parse) else {
        return *bad_request("returntype must be locations or geographies");
    };
    let (bench, vintage) = match resolve(&st, q.benchmark.as_deref(), q.vintage.as_deref(), rt) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let up = Upload {
        file: Some(("file".into(), Some("import.csv".into()), body)),
        fields: HashMap::new(),
    };
    let inputs = match inputs_from_upload(&up, q.searchtype.as_deref().unwrap_or("address")).await {
        Ok(i) => i,
        Err(r) => return *r,
    };
    let spec = JobSpec {
        kind: JobKind::Warm,
        return_type: rt,
        bench,
        vintage,
        original_filename: Some("import.csv".into()),
        priority: -1,
    };
    let created = match submit::build_rows(&spec, inputs) {
        Ok((search_type, rows)) => submit::create(&st, spec, search_type, rows, true).await,
        Err(e) => Err(e),
    };
    match created {
        Ok(job) => (
            StatusCode::CREATED,
            Json(ImportResponse {
                format: "csv".into(),
                records: job.totals.total,
                invalid: 0,
                errors: Vec::new(),
                job: Some(JobView::from(job)),
            }),
        )
            .into_response(),
        Err(SubmitError::Invalid(msg)) => AppError::BadRequest(msg).into_response(),
        Err(SubmitError::Busy(msg)) => AppError::Conflict(msg).into_response(),
        Err(SubmitError::Store(e)) => internal(e),
        Err(SubmitError::Internal(e)) => internal(e),
    }
}

// ------------------------------------------------------------------ compact

#[derive(Debug, Serialize, ToSchema)]
pub struct CompactResponse {
    /// `false`: there was nothing to reclaim.
    pub compacted: bool,
    pub file_bytes_before: u64,
    pub file_bytes_after: u64,
    pub millis: u64,
}

/// Reclaim free space (`Database::compact`). Reads and writes wait while it runs.
#[utoipa::path(
    post,
    path = "/api/v1/cache/compact",
    tag = TAG,
    responses((status = 200, body = CompactResponse))
)]
async fn compact(State(st): State<SharedState>) -> Response {
    let store = st.store.clone();
    let result = tokio::task::spawn_blocking(move || -> anyhow::Result<CompactResponse> {
        let before = store.file_size()?;
        let start = std::time::Instant::now();
        let compacted = store.compact()?;
        Ok(CompactResponse {
            compacted,
            file_bytes_before: before,
            file_bytes_after: store.file_size()?,
            millis: u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX),
        })
    })
    .await;
    match result {
        Ok(Ok(r)) => Json(r).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("compact task: {e}")),
    }
}
