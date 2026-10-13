//! Census-compatible batch endpoints (PLAN.md §6.2):
//! `POST /geocoder/{locations|geographies}/addressbatch` and
//! `POST /geocoder/geographies/coordinatesbatch`.
//!
//! Rows are answered from the cache where possible; only misses (deduplicated by
//! cache key) go upstream, in sequential chunks. The response is buffered (v1).

use std::collections::{HashMap, HashSet};

use axum::{
    Router,
    body::Bytes,
    extract::{DefaultBodyLimit, Multipart, Path, State},
    http::{HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use tracing::warn;

use crate::{
    batch::{BatchTarget, ChunkError, Row, fetch_chunk, lookup},
    cache::{
        envelope::EntrySource,
        key::CacheKey,
        views::{BatchLookup, BatchNeed},
    },
    catalog::{Benchmark, Catalog, Vintage},
    census::{
        batch_csv::{
            BatchInputError, MAX_INPUT_FIELDS, Record, RowResult, UPSTREAM_PARAM_ERROR,
            address_rows, coord_rows, read_records, upstream_too_many,
        },
        params::{MSG_NO_BENCHMARK, MSG_NO_VINTAGE},
        types::ReturnType,
    },
    error::AppError,
    state::SharedState,
    store::stats::Counter,
    upstream::{UpstreamError, batch::BatchEndpoint, throttle::Priority},
};

pub(crate) const MSG_INVALID_BENCHMARK: &str = "Invalid benchmark in request";
/// Unverified upstream text (PLAN.md §11.5).
pub(crate) const MSG_INVALID_VINTAGE: &str = "Invalid vintage in request";

pub fn routes(max_body_bytes: usize) -> Router<SharedState> {
    Router::new()
        .route("/geocoder/{returntype}/addressbatch", post(address_batch))
        .route(
            "/geocoder/geographies/coordinatesbatch",
            post(coordinates_batch),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
}

/// A multipart upload: one file field plus text fields.
#[derive(Default)]
pub(crate) struct Upload {
    /// `(field name, file name, bytes)`.
    pub file: Option<(String, Option<String>, Bytes)>,
    pub fields: HashMap<String, String>,
}

impl Upload {
    pub fn field(&self, name: &str) -> Option<&str> {
        self.fields
            .get(name)
            .map(String::as_str)
            .filter(|v| !v.is_empty())
    }
}

/// Read every multipart field before validating (fields may arrive in any order).
/// Fields named in `file_fields` are kept as bytes; every other field as trimmed text.
pub(crate) async fn read_upload(
    mut mp: Multipart,
    file_fields: &[&str],
) -> Result<Upload, Box<Response>> {
    let mut up = Upload::default();
    loop {
        let field = match mp.next_field().await {
            Ok(Some(f)) => f,
            Ok(None) => break,
            Err(e) => return Err(Box::new((e.status(), e.body_text()).into_response())),
        };
        let name = field.name().unwrap_or_default().to_owned();
        let file_name = field.file_name().map(str::to_owned);
        let data = match field.bytes().await {
            Ok(b) => b,
            Err(e) => return Err(Box::new((e.status(), e.body_text()).into_response())),
        };
        if file_fields.contains(&name.as_str()) {
            up.file = Some((name, file_name, data));
        } else {
            let text = String::from_utf8_lossy(&data).trim().to_owned();
            up.fields.insert(name, text);
        }
    }
    Ok(up)
}

pub(crate) fn bad_request(msg: impl Into<String>) -> Box<Response> {
    Box::new(AppError::BadRequest(msg.into()).into_response())
}

/// Resolve benchmark/vintage; `locations` uses the benchmark's default vintage for
/// keys so batch rows share entries with single-record lookups.
pub(crate) fn resolve(
    st: &SharedState,
    benchmark: Option<&str>,
    vintage: Option<&str>,
    rt: ReturnType,
) -> Result<(Benchmark, Vintage), Box<Response>> {
    resolve_target(&st.catalog.load(), benchmark, vintage, rt).map_err(bad_request)
}

/// Resolve benchmark/vintage aliases; `locations` uses the benchmark's default vintage.
/// Errors are the Census-style messages.
pub fn resolve_target(
    catalog: &Catalog,
    benchmark: Option<&str>,
    vintage: Option<&str>,
    rt: ReturnType,
) -> Result<(Benchmark, Vintage), &'static str> {
    let Some(b) = benchmark.filter(|b| !b.is_empty()) else {
        return Err(MSG_NO_BENCHMARK);
    };
    let bench = catalog
        .resolve_benchmark(b)
        .cloned()
        .ok_or(MSG_INVALID_BENCHMARK)?;
    let vintage = match (rt, vintage.filter(|v| !v.is_empty())) {
        (ReturnType::Geographies, None) => return Err(MSG_NO_VINTAGE),
        (ReturnType::Geographies, Some(v)) => catalog.resolve_vintage(bench.id, v),
        (ReturnType::Locations, _) => catalog.default_vintage(bench.id),
    }
    .cloned()
    .ok_or(MSG_INVALID_VINTAGE)?;
    Ok((bench, vintage))
}

/// Parse the uploaded file off the async workers, enforcing the row limit while
/// reading so oversized uploads are rejected before they are materialized.
/// Census's own `text/plain` error body (mirrored byte-for-byte).
fn upstream_text_error(body: String) -> Box<Response> {
    let mut resp = (StatusCode::BAD_REQUEST, body).into_response();
    resp.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/plain;charset=UTF-8"),
    );
    Box::new(resp)
}

/// Census answers an empty batch file with `200` and an empty body.
fn empty_batch() -> Response {
    let mut resp = StatusCode::OK.into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=GeocodeResults.csv"),
    );
    resp
}

/// Resolve a batch's benchmark/vintage; any problem is Census's generic parameter error.
fn resolve_batch(
    st: &SharedState,
    up: &Upload,
    rt: ReturnType,
) -> Result<(Benchmark, Vintage), Box<Response>> {
    resolve_target(
        &st.catalog.load(),
        up.field("benchmark"),
        up.field("vintage"),
        rt,
    )
    .map_err(|_| upstream_text_error(UPSTREAM_PARAM_ERROR.to_owned()))
}

/// Parse the uploaded file off the async workers, enforcing the row limit while
/// reading so oversized uploads are rejected before they are materialized. Columns
/// beyond the row type's width are ignored (as Census does). An empty file yields no
/// rows (Census answers it with an empty `200`).
pub(crate) async fn parse_rows<R: Send + 'static>(
    up: &Upload,
    parse: fn(Vec<Record>) -> Result<Vec<R>, BatchInputError>,
    max_rows: usize,
) -> Result<Vec<R>, Box<Response>> {
    let Some((_, name, data)) = up.file.clone() else {
        // Census renders its HTML form here; a JSON error is more useful.
        return Err(bad_request("Batch file must be specified"));
    };
    let parsed = tokio::task::spawn_blocking(move || {
        read_records(&data, name.as_deref(), max_rows, MAX_INPUT_FIELDS).and_then(parse)
    })
    .await
    .map_err(|e| {
        Box::new(AppError::Internal(anyhow::anyhow!("batch parse task: {e}")).into_response())
    })?;
    match parsed {
        Ok(rows) => Ok(rows),
        Err(BatchInputError::TooMany(limit)) => Err(upstream_text_error(upstream_too_many(limit))),
        // Census answers malformed CSV with a 500 "cannot recover"; a 400 naming the
        // rows is more truthful.
        Err(e) => Err(bad_request(e.to_string())),
    }
}

/// The upstream endpoint, key need and upstream-vintage flag for an address batch.
pub(crate) fn address_plan(
    rt: ReturnType,
    fetch_geographies_always: bool,
) -> (BatchNeed, BatchEndpoint, bool) {
    let geographies_out = rt == ReturnType::Geographies;
    let fetch_geo = geographies_out || fetch_geographies_always;
    let need = if geographies_out {
        BatchNeed::Geographies
    } else {
        BatchNeed::Locations
    };
    (
        need,
        BatchEndpoint::Address {
            geographies: fetch_geo,
        },
        fetch_geo,
    )
}

async fn address_batch(
    State(st): State<SharedState>,
    Path(rt): Path<String>,
    mp: Multipart,
) -> Response {
    let Some(rt) = ReturnType::parse(&rt) else {
        return AppError::NotFound("Not Found".into()).into_response();
    };
    let up = match read_upload(mp, &["addressFile"]).await {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let (bench, vintage) = match resolve_batch(&st, &up, rt) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let max_rows = st.settings().config.batch.max_rows;
    let rows = match parse_rows(&up, address_rows, max_rows).await {
        Ok(r) if r.is_empty() => return empty_batch(),
        Ok(r) => r,
        Err(r) => return *r,
    };
    let (need, endpoint, upstream_vintage) =
        address_plan(rt, st.settings().config.cache.fetch_geographies_always);
    let class = st.catalog.load().class(bench.id);
    let target = BatchTarget {
        bench,
        vintage,
        endpoint,
        upstream_vintage,
        class,
    };
    run_batch(st, rows, target, need, rt == ReturnType::Geographies).await
}

async fn coordinates_batch(State(st): State<SharedState>, mp: Multipart) -> Response {
    let up = match read_upload(mp, &["coordinatesFile"]).await {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let (bench, vintage) = match resolve_batch(&st, &up, ReturnType::Geographies) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let max_rows = st.settings().config.batch.max_rows;
    let rows = match parse_rows(&up, coord_rows, max_rows).await {
        Ok(r) if r.is_empty() => return empty_batch(),
        Ok(r) => r,
        Err(r) => return *r,
    };
    let class = st.catalog.load().class(bench.id);
    let target = BatchTarget {
        bench,
        vintage,
        endpoint: BatchEndpoint::Coordinates,
        upstream_vintage: true,
        class,
    };
    run_batch(st, rows, target, BatchNeed::Coordinates, true).await
}

pub(crate) fn upstream_error(e: UpstreamError) -> Response {
    match e {
        UpstreamError::Rejected(r) => {
            let status = StatusCode::from_u16(r.status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
            let mut resp = (status, r.body).into_response();
            if let Some(ct) = r.content_type.and_then(|c| HeaderValue::from_str(&c).ok()) {
                resp.headers_mut().insert(header::CONTENT_TYPE, ct);
            }
            resp
        }
        UpstreamError::BreakerOpen(b) => AppError::Unavailable {
            msg: "Upstream geocoder temporarily unavailable".into(),
            retry_after_secs: Some(b.retry_after.as_secs().max(1)),
        }
        .into_response(),
        UpstreamError::Exhausted { last, .. } => {
            warn!(error = %last, "batch chunk failed after retries");
            AppError::BadGateway("Upstream geocoder unavailable".into()).into_response()
        }
    }
}

fn chunk_error(e: ChunkError) -> Response {
    match e {
        ChunkError::Upstream(e) => upstream_error(e),
        ChunkError::Unparseable(_) => {
            AppError::BadGateway("Upstream batch response was not understood".into())
                .into_response()
        }
        ChunkError::Incomplete { .. } => {
            AppError::BadGateway("Upstream batch response was incomplete".into()).into_response()
        }
    }
}

async fn run_batch<R: Row>(
    st: SharedState,
    rows: Vec<R>,
    target: BatchTarget,
    need: BatchNeed,
    geographies_out: bool,
) -> Response {
    let cfg = st.settings().config.clone();
    let keys: Vec<String> = rows
        .iter()
        .map(|r| CacheKey::new(target.bench.id, target.vintage.id, &r.key_input()).to_string())
        .collect();
    let echoes = rows.iter().map(Row::echo).collect();
    let mut lookups = match lookup(&st, keys.clone(), echoes, need, target.class).await {
        Ok(l) => l,
        Err(e) => return AppError::Internal(e).into_response(),
    };

    // Optionally let upstream confirm heuristic rows while the batch lane is idle.
    if cfg.batch.upgrade_when_idle {
        let (_, batch_free) = st.upstream.throttle.available_permits();
        let (_, batch_waiting) = st.upstream.throttle.sync_waiters();
        if batch_free > 0 && batch_waiting == 0 {
            for l in &mut lookups {
                if matches!(
                    l,
                    BatchLookup::Hit {
                        heuristic: true,
                        ..
                    }
                ) {
                    *l = BatchLookup::Miss;
                }
            }
        }
    }

    // Unique missing keys, in first-occurrence order, each with its first row.
    let mut seen: HashSet<&str> = HashSet::new();
    let misses: Vec<(&R, &str)> = lookups
        .iter()
        .enumerate()
        .filter(|(i, l)| **l == BatchLookup::Miss && seen.insert(keys[*i].as_str()))
        .map(|(i, _)| (&rows[i], keys[i].as_str()))
        .collect();

    let mut fetched: HashMap<String, RowResult> = HashMap::new();
    for chunk in misses.chunks(cfg.batch.chunk_size.max(1)) {
        let results = match fetch_chunk(&st, &target, chunk, Priority::Sync, |_, _| {
            EntrySource::Batch
        })
        .await
        {
            Ok(r) => r,
            Err(e) => return chunk_error(e),
        };
        fetched.extend(results);
    }

    // Assemble in input order.
    let mut w = crate::census::batch_csv::output_writer();
    let (mut hits, mut heuristic_count) = (0usize, 0usize);
    for (i, row) in rows.iter().enumerate() {
        let result = match &lookups[i] {
            BatchLookup::Hit { result, heuristic } => {
                hits += 1;
                heuristic_count += usize::from(*heuristic);
                st.stats.record_hit(&keys[i]);
                result.clone()
            }
            BatchLookup::Miss => {
                st.stats.record(Counter::Misses);
                fetched.get(&keys[i]).cloned().unwrap_or(RowResult::NoMatch)
            }
        };
        if let Err(e) = row.write(&mut w, &result, geographies_out) {
            return AppError::Internal(e.into()).into_response();
        }
    }
    let body = match w.into_inner() {
        Ok(b) => b,
        Err(e) => return AppError::Internal(anyhow::anyhow!("csv writer: {e}")).into_response(),
    };

    let mut resp = (StatusCode::OK, body).into_response();
    let h = resp.headers_mut();
    h.insert(header::CONTENT_TYPE, HeaderValue::from_static("text/plain"));
    h.insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_static("attachment; filename=GeocodeResults.csv"),
    );
    let row_misses = rows.len() - hits;
    if row_misses == 0 {
        h.insert("x-cache", HeaderValue::from_static("HIT"));
    } else {
        h.insert("x-ucg-cache-hits", HeaderValue::from(hits));
        h.insert("x-ucg-cache-misses", HeaderValue::from(row_misses));
    }
    if heuristic_count > 0 {
        h.insert("x-ucg-heuristic-count", HeaderValue::from(heuristic_count));
    }
    resp
}
