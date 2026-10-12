//! Census-compatible batch endpoints (PLAN.md §6.2):
//! `POST /geocoder/{locations|geographies}/addressbatch` and
//! `POST /geocoder/geographies/coordinatesbatch`.
//!
//! Rows are answered from the cache where possible; only misses (deduplicated by
//! cache key) go upstream, in sequential chunks. The response is buffered (v1).

use std::{
    collections::HashMap,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

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
    cache::{
        envelope::{BatchView, Entry, EntryPayload, EntrySource, Status, Views},
        key::{CacheKey, KeyInput},
        views::{BatchLookup, BatchNeed, classify_batch, merge_batch},
    },
    catalog::{Benchmark, Vintage},
    census::{
        batch_csv::{
            AddressRow, BatchInputError, CoordRow, Record, RowResult, UpstreamRow, address_rows,
            coord_rows, output_writer, parse_upstream_addresses, parse_upstream_coordinates,
            read_records, upload_addresses, upload_coordinates, write_address_row, write_coord_row,
        },
        params::{MSG_NO_BENCHMARK, MSG_NO_VINTAGE},
        types::ReturnType,
    },
    error::AppError,
    state::SharedState,
    store::stats::Counter,
    upstream::{UpstreamError, batch::BatchEndpoint, throttle::Priority},
};

const MSG_INVALID_BENCHMARK: &str = "Invalid benchmark in request";
/// Unverified upstream text (PLAN.md §11.5).
const MSG_INVALID_VINTAGE: &str = "Invalid vintage in request";

pub fn routes(max_body_bytes: usize) -> Router<SharedState> {
    Router::new()
        .route("/geocoder/{returntype}/addressbatch", post(address_batch))
        .route(
            "/geocoder/geographies/coordinatesbatch",
            post(coordinates_batch),
        )
        .layer(DefaultBodyLimit::max(max_body_bytes))
}

/// One input row kind (addresses or coordinates).
trait Row: Send + Sync + 'static {
    fn idx(&self) -> u32;
    fn key_input(&self) -> KeyInput;
    /// `street, city, state, zip` for the match-type heuristic (empty for coordinates).
    fn echo(&self) -> String;
    fn upload<'a>(rows: impl IntoIterator<Item = &'a Self>) -> Vec<u8>
    where
        Self: 'a;
    fn parse_upstream(body: &[u8]) -> Result<Vec<UpstreamRow>, String>;
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
    fn parse_upstream(body: &[u8]) -> Result<Vec<UpstreamRow>, String> {
        parse_upstream_addresses(body)
    }
    fn write(
        &self,
        w: &mut csv::Writer<Vec<u8>>,
        result: &RowResult,
        geographies: bool,
    ) -> csv::Result<()> {
        write_address_row(w, self, result, geographies)
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
    fn parse_upstream(body: &[u8]) -> Result<Vec<UpstreamRow>, String> {
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

#[derive(Default)]
struct Upload {
    file: Option<(Option<String>, Bytes)>,
    benchmark: Option<String>,
    vintage: Option<String>,
}

/// Read every multipart field before validating (fields may arrive in any order).
async fn read_upload(mut mp: Multipart, file_field: &str) -> Result<Upload, Box<Response>> {
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
        match name.as_str() {
            n if n == file_field => up.file = Some((file_name, data)),
            "benchmark" => up.benchmark = Some(String::from_utf8_lossy(&data).trim().to_owned()),
            "vintage" => up.vintage = Some(String::from_utf8_lossy(&data).trim().to_owned()),
            _ => {}
        }
    }
    Ok(up)
}

fn bad_request(msg: impl Into<String>) -> Box<Response> {
    Box::new(AppError::BadRequest(msg.into()).into_response())
}

/// Resolve benchmark/vintage; `locations` batches use the benchmark's default vintage
/// for keys so they share entries with single-record lookups.
fn resolve(
    st: &SharedState,
    up: &Upload,
    rt: ReturnType,
) -> Result<(Benchmark, Vintage), Box<Response>> {
    let Some(b) = up.benchmark.as_deref().filter(|b| !b.is_empty()) else {
        return Err(bad_request(MSG_NO_BENCHMARK));
    };
    let catalog = st.catalog.load();
    let bench = catalog
        .resolve_benchmark(b)
        .cloned()
        .ok_or_else(|| bad_request(MSG_INVALID_BENCHMARK))?;
    let vintage = match (rt, up.vintage.as_deref().filter(|v| !v.is_empty())) {
        (ReturnType::Geographies, None) => return Err(bad_request(MSG_NO_VINTAGE)),
        (ReturnType::Geographies, Some(v)) => catalog.resolve_vintage(bench.id, v),
        (ReturnType::Locations, _) => catalog.default_vintage(bench.id),
    }
    .cloned()
    .ok_or_else(|| bad_request(MSG_INVALID_VINTAGE))?;
    Ok((bench, vintage))
}

/// Parse the uploaded file off the async workers, enforcing the row limit while
/// reading so oversized uploads are rejected before they are materialized.
async fn parse_rows<R: Send + 'static>(
    up: &Upload,
    parse: fn(Vec<Record>) -> Result<Vec<R>, BatchInputError>,
    max_rows: usize,
) -> Result<Vec<R>, Box<Response>> {
    let Some((name, data)) = up.file.clone() else {
        return Err(bad_request("Batch file must be specified"));
    };
    let parsed = tokio::task::spawn_blocking(move || {
        read_records(&data, name.as_deref(), max_rows).and_then(parse)
    })
    .await
    .map_err(|e| {
        Box::new(AppError::Internal(anyhow::anyhow!("batch parse task: {e}")).into_response())
    })?;
    let rows = parsed.map_err(|e| bad_request(e.to_string()))?;
    if rows.is_empty() {
        return Err(bad_request("Batch file contains no records"));
    }
    Ok(rows)
}

async fn address_batch(
    State(st): State<SharedState>,
    Path(rt): Path<String>,
    mp: Multipart,
) -> Response {
    let Some(rt) = ReturnType::parse(&rt) else {
        return AppError::NotFound("Not Found".into()).into_response();
    };
    let up = match read_upload(mp, "addressFile").await {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let (bench, vintage) = match resolve(&st, &up, rt) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let max_rows = st.settings().config.batch.max_rows;
    let rows = match parse_rows(&up, address_rows, max_rows).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    let geographies_out = rt == ReturnType::Geographies;
    let fetch_geo = geographies_out || st.settings().config.cache.fetch_geographies_always;
    let need = if geographies_out {
        BatchNeed::Geographies
    } else {
        BatchNeed::Locations
    };
    run_batch(
        st,
        rows,
        bench,
        vintage,
        need,
        BatchEndpoint::Address {
            geographies: fetch_geo,
        },
        fetch_geo,
        geographies_out,
    )
    .await
}

async fn coordinates_batch(State(st): State<SharedState>, mp: Multipart) -> Response {
    let up = match read_upload(mp, "coordinatesFile").await {
        Ok(u) => u,
        Err(r) => return *r,
    };
    let (bench, vintage) = match resolve(&st, &up, ReturnType::Geographies) {
        Ok(x) => x,
        Err(r) => return *r,
    };
    let max_rows = st.settings().config.batch.max_rows;
    let rows = match parse_rows(&up, coord_rows, max_rows).await {
        Ok(r) => r,
        Err(r) => return *r,
    };
    run_batch(
        st,
        rows,
        bench,
        vintage,
        BatchNeed::Coordinates,
        BatchEndpoint::Coordinates,
        true,
        true,
    )
    .await
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn upstream_error(e: UpstreamError) -> Response {
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

#[allow(clippy::too_many_arguments)]
async fn run_batch<R: Row>(
    st: SharedState,
    rows: Vec<R>,
    bench: Benchmark,
    vintage: Vintage,
    need: BatchNeed,
    endpoint: BatchEndpoint,
    upstream_vintage: bool,
    geographies_out: bool,
) -> Response {
    let settings = st.settings();
    let cfg = settings.config.clone();
    let class = st.catalog.load().class(bench.id);
    let rows = Arc::new(rows);
    let keys: Arc<Vec<String>> = Arc::new(
        rows.iter()
            .map(|r| CacheKey::new(bench.id, vintage.id, &r.key_input()).to_string())
            .collect(),
    );

    // Decoding up to 10k entries is CPU work: keep it off the async workers.
    let lookups = {
        let (store, codec, rows, keys, cfg) = (
            st.store.clone(),
            st.codec,
            Arc::clone(&rows),
            Arc::clone(&keys),
            cfg.clone(),
        );
        tokio::task::spawn_blocking(move || {
            let refs: Vec<&str> = keys.iter().map(String::as_str).collect();
            let stored = store.get_entries(&refs)?;
            let now = SystemTime::now();
            Ok::<_, crate::store::StoreError>(
                stored
                    .iter()
                    .zip(rows.iter())
                    .map(|(bytes, row)| {
                        classify_batch(bytes.as_deref(), codec, &cfg, class, need, now, &row.echo())
                    })
                    .collect::<Vec<_>>(),
            )
        })
        .await
    };
    let mut lookups = match lookups {
        Ok(Ok(l)) => l,
        Ok(Err(e)) => return AppError::Internal(e.into()).into_response(),
        Err(e) => {
            return AppError::Internal(anyhow::anyhow!("batch lookup task: {e}")).into_response();
        }
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
    let mut seen: HashMap<&str, ()> = HashMap::new();
    let mut misses: Vec<usize> = Vec::new();
    for (i, l) in lookups.iter().enumerate() {
        if *l == BatchLookup::Miss && seen.insert(keys[i].as_str(), ()).is_none() {
            misses.push(i);
        }
    }

    let mut fetched: HashMap<String, RowResult> = HashMap::new();
    let chunk_size = cfg.batch.chunk_size.max(1);
    let vintage_param = upstream_vintage.then_some(vintage.id);
    for chunk in misses.chunks(chunk_size) {
        let csv = R::upload(chunk.iter().map(|&i| &rows[i]));
        let resp = match st
            .upstream
            .fetch_batch(Priority::Sync, endpoint, bench.id, vintage_param, csv)
            .await
        {
            Ok(r) => r,
            Err(e) => {
                if matches!(e, UpstreamError::Exhausted { .. }) {
                    st.stats.record(Counter::UpstreamErr);
                }
                return upstream_error(e);
            }
        };
        st.stats.record(Counter::UpstreamOk);
        let parsed = match R::parse_upstream(&resp.body) {
            Ok(p) => p,
            Err(e) => {
                warn!(error = %e, "upstream batch output not understood");
                st.stats.record(Counter::UpstreamErr);
                return AppError::BadGateway("Upstream batch response was not understood".into())
                    .into_response();
            }
        };
        let by_idx: HashMap<u32, UpstreamRow> = parsed.into_iter().map(|u| (u.idx, u)).collect();
        // Every submitted row must be answered: an absent line is an upstream failure,
        // never evidence of No_Match.
        let missing = chunk
            .iter()
            .filter(|&&i| !by_idx.contains_key(&rows[i].idx()))
            .count();
        if missing > 0 {
            warn!(
                missing,
                submitted = chunk.len(),
                "upstream batch response omitted rows"
            );
            st.stats.record(Counter::UpstreamErr);
            return AppError::BadGateway("Upstream batch response was incomplete".into())
                .into_response();
        }

        let now = SystemTime::now();
        let mut to_store: Vec<(String, Entry)> = Vec::new();
        let mut retries: Vec<(String, u64, u8)> = Vec::new();
        for &i in chunk {
            let key = keys[i].clone();
            let Some(u) = by_idx.get(&rows[i].idx()) else {
                continue;
            };
            let result = match u.status {
                Status::Match => RowResult::Match(Box::new(u.view.clone())),
                Status::NoMatch => RowResult::NoMatch,
                Status::Tie => RowResult::Tie,
            };
            if u.status == Status::NoMatch {
                retries.push((
                    key.clone(),
                    unix_now() + cfg.retry.negative_delay.as_secs(),
                    0,
                ));
            }
            to_store.push((
                key.clone(),
                Entry {
                    status: u.status,
                    source: EntrySource::Batch,
                    views: Views::BATCH,
                    verified_at: unix_now(),
                    payload: EntryPayload {
                        json: None,
                        batch: Some(if u.status == Status::Match {
                            u.view.clone()
                        } else {
                            BatchView::default()
                        }),
                        benchmark_name: bench.name.clone(),
                        vintage_name: vintage.name.clone(),
                    },
                },
            ));
            fetched.insert(key, result);
        }
        let (codec, ttl_cfg) = (st.codec, cfg.ttl.clone());
        if let Err(e) = st
            .store
            .put_entries_with(to_store, move |existing, entry| {
                merge_batch(existing, entry, codec, &ttl_cfg, class, now)
            })
            .await
        {
            warn!(error = %e, "storing batch results failed; serving uncached");
        }
        if let Err(e) = st.store.retry_enqueue_many(retries).await {
            warn!(error = %e, "enqueueing batch No_Match retries failed");
        }
    }

    // Assemble in input order.
    let mut w = output_writer();
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
