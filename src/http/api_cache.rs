//! `/api/v1/cache/*` (PLAN.md §8): stats, entry listing/inspection, purge, export,
//! import and compaction. Logic lives in `cache::admin`, `cache::maintenance` and the
//! store (shared with the offline CLI); these are thin handlers.

use std::{
    collections::HashMap,
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
        admin::{
            self, CacheStats, CompactResponse, cache_stats, export_pages, parse_scope,
            parse_status, purge_context, store_imported,
        },
        envelope::{BatchView, EntrySource, Header, Status, Views, decode_stored},
        key::{CacheKey, KEY_VERSION, SearchType},
        maintenance::{export_record, parse_import, purge},
        policy::{Freshness, freshness, ttl},
    },
    census::types::ReturnType,
    error::AppError,
    jobs::{
        JobKind,
        submit::{self, JobSpec, SubmitError},
    },
    state::SharedState,
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

// -------------------------------------------------------------------- stats

#[derive(Debug, Deserialize, IntoParams)]
#[into_params(parameter_in = Query)]
pub struct StatsQuery {
    /// `full`: also scan every entry header for per-status/searchtype/benchmark counts
    /// and sample the compression ratio (slower; the default is cheap enough to poll).
    pub detail: Option<String>,
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
    match tokio::task::spawn_blocking(move || cache_stats(&st2, full)).await {
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
    let scope = match parse_scope(&st.catalog.load(), &req.scope, req.value.as_deref()) {
        Ok(s) => s,
        Err(msg) => return AppError::BadRequest(msg).into_response(),
    };
    match purge(&st.store, scope, purge_context(&st), req.dry_run).await {
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
    tokio::task::spawn_blocking(move || {
        let sent = export_pages(&store, codec, &prefix, |chunk| {
            tx.blocking_send(Ok(Bytes::from(chunk))).is_ok() // false: client went away
        });
        if let Err(e) = sent {
            warn!(error = %e, "export aborted");
            // Fail the body so the client sees a truncated download.
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
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
    let records = parsed.records.len() as u64;
    if let Err(e) = store_imported(&st, parsed.records).await {
        return internal(e);
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

/// Reclaim free space (`Database::compact`). Reads and writes wait while it runs.
#[utoipa::path(
    post,
    path = "/api/v1/cache/compact",
    tag = TAG,
    responses((status = 200, body = CompactResponse))
)]
async fn compact(State(st): State<SharedState>) -> Response {
    let store = st.store.clone();
    match tokio::task::spawn_blocking(move || admin::compact(&store)).await {
        Ok(Ok(r)) => Json(r).into_response(),
        Ok(Err(e)) => internal(e),
        Err(e) => internal(anyhow::anyhow!("compact task: {e}")),
    }
}
