//! Census-compatible `/geocoder/...` single-record and catalog endpoints (PLAN.md §6.1).

use std::{
    sync::Arc,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    extract::{Path, RawQuery, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::Value;
use tracing::{debug, warn};

use crate::{
    cache::{
        envelope::{Entry, EntrySource, Status, UpstreamSingle, Views, encode_stored},
        key::{CacheKey, KeyInput},
        policy::{BenchmarkClass, Freshness, freshness, ttl},
        views::{Lookup, Need, Stored, classify},
    },
    catalog::Catalog,
    census::{
        layers::LayerFilter,
        params::{self, Parsed},
        response::{CT_JSON, RenderInput, render},
        types::{ReturnType, SearchPath},
    },
    error::AppError,
    state::{AppState, CATALOG_BENCHMARKS, SharedState, catalog_vintages_key},
    store::stats::Counter,
    upstream::{Kind, UpstreamError, UpstreamResponse, single::SingleQuery, throttle::Priority},
};

pub const X_CACHE: &str = "x-cache";

/// Shared result of one coalesced upstream fetch.
pub type FetchResult = Result<Arc<Stored>, FetchFailure>;

#[derive(Debug, Clone)]
pub enum FetchFailure {
    /// Upstream answered with something we do not cache (4xx, unparseable 200):
    /// forward it verbatim.
    Forward(UpstreamResponse),
    BreakerOpen(Duration),
    Exhausted(String),
}

pub fn routes() -> Router<SharedState> {
    Router::new()
        .route("/geocoder/benchmarks", get(benchmarks))
        .route("/geocoder/vintages", get(vintages))
        .route("/geocoder/{returntype}/{searchtype}", get(single))
}

fn unix_now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn with_cache_header(mut resp: Response, x_cache: &'static str) -> Response {
    resp.headers_mut()
        .insert(X_CACHE, HeaderValue::from_static(x_cache));
    resp
}

fn body_response(
    status: StatusCode,
    content_type: &str,
    body: impl Into<axum::body::Body>,
    x_cache: &'static str,
) -> Response {
    let mut resp = (status, body.into()).into_response();
    if let Ok(ct) = HeaderValue::from_str(content_type) {
        resp.headers_mut().insert(header::CONTENT_TYPE, ct);
    }
    with_cache_header(resp, x_cache)
}

/// Forward an upstream response as-is (status, Content-Type, body).
fn forward(r: UpstreamResponse, x_cache: &'static str) -> Response {
    let status = StatusCode::from_u16(r.status.as_u16()).unwrap_or(StatusCode::BAD_GATEWAY);
    body_response(
        status,
        r.content_type.as_deref().unwrap_or(CT_JSON),
        r.body,
        x_cache,
    )
}

fn upstream_failure(e: UpstreamError, x_cache: &'static str) -> Response {
    let resp = match e {
        UpstreamError::Rejected(r) => return forward(r, x_cache),
        UpstreamError::BreakerOpen(b) => AppError::Unavailable {
            msg: "Upstream geocoder temporarily unavailable".into(),
            retry_after_secs: Some(b.retry_after.as_secs().max(1)),
        },
        UpstreamError::Exhausted { last, .. } => {
            debug!(error = %last, "upstream exhausted");
            AppError::BadGateway("Upstream geocoder unavailable".into())
        }
    };
    with_cache_header(resp.into_response(), x_cache)
}

/// Send the client's original request upstream uncached and forward the answer.
async fn passthrough(st: &AppState, path: &str, raw_query: &str) -> Response {
    let url = format!("{}/{path}?{raw_query}", st.upstream.base_url());
    match st
        .upstream
        .execute(Kind::Single, Priority::Sync, |c| c.get(&url))
        .await
    {
        Ok(r) => forward(r, "BYPASS"),
        Err(e) => upstream_failure(e, "BYPASS"),
    }
}

async fn single(
    State(st): State<SharedState>,
    Path((rt, sp)): Path<(String, String)>,
    RawQuery(query): RawQuery,
    headers: HeaderMap,
) -> Response {
    let (Some(rt), Some(sp)) = (ReturnType::parse(&rt), SearchPath::parse(&sp)) else {
        return AppError::NotFound("Not Found".into()).into_response();
    };
    let raw_query = query.unwrap_or_default();
    let pairs: Vec<(String, String)> = form_urlencoded::parse(raw_query.as_bytes())
        .into_owned()
        .collect();
    let flat_header = headers
        .get("x-ucg-flatten")
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| matches!(v.trim(), "1" | "true"));
    let path = format!("{}/{}", rt.as_str(), sp.as_str());
    let req = match params::parse(rt, sp, &pairs, flat_header) {
        Parsed::Request(r) => r,
        Parsed::Error(e) => return e.into_response(),
        Parsed::Passthrough => return passthrough(&st, &path, &raw_query).await,
    };

    let catalog = st.catalog.load_full();
    let Some(bench) = catalog.resolve_benchmark(&req.benchmark) else {
        return passthrough(&st, &path, &raw_query).await;
    };
    let vintage = match &req.vintage {
        Some(v) => catalog.resolve_vintage(bench.id, v),
        None => catalog.default_vintage(bench.id),
    };
    let Some(vintage) = vintage else {
        return passthrough(&st, &path, &raw_query).await;
    };

    let key = CacheKey::new(bench.id, vintage.id, &req.input);
    let need = match (rt, sp) {
        (_, SearchPath::Coordinates) => Need::Coordinates,
        (ReturnType::Locations, _) => Need::Locations,
        (ReturnType::Geographies, _) => Need::Geographies,
    };
    let settings = st.settings();
    let cfg = &settings.config;
    let class = catalog.class(bench.id);

    let bytes = st.store.get_entry(key.as_str()).unwrap_or_else(|e| {
        warn!(error = %e, "store read failed; treating as miss");
        None
    });
    let lookup = classify(
        bytes.as_deref(),
        st.codec,
        cfg,
        class,
        need,
        SystemTime::now(),
    );
    let (stored, x_cache) = match lookup {
        Lookup::Hit(s) => {
            st.stats.record_hit(key.as_str());
            (Arc::new(s), "HIT")
        }
        miss => {
            let (stale, existing) = match miss {
                Lookup::Stale(s) => (Some(s.clone()), Some(s)),
                Lookup::Miss(existing) => (None, existing),
                Lookup::Corrupt => {
                    let (store, k) = (st.store.clone(), key.to_string());
                    tokio::spawn(async move {
                        if let Err(e) = store.delete_entry(k).await {
                            warn!(error = %e, "deleting undecodable entry failed");
                        }
                    });
                    (None, None)
                }
                Lookup::Hit(_) => unreachable!("handled above"),
            };
            st.stats.record(Counter::Misses);
            let fetch_geo = cfg.cache.fetch_geographies_always || need != Need::Locations;
            let job = FetchJob {
                state: Arc::clone(&st),
                key: key.clone(),
                input: req.input.clone(),
                benchmark_id: bench.id,
                vintage_id: vintage.id,
                fetch_geo,
                need,
                class,
                existing,
            };
            let sf_key = format!("{key}#{}", if fetch_geo { 'g' } else { 'l' });
            match st.singleflight.run(sf_key, || job.run()).await {
                Some(Ok(s)) => (s, "MISS"),
                Some(Err(FetchFailure::Forward(r))) => return forward(r, "MISS"),
                Some(Err(failure)) => match stale {
                    Some(s) => {
                        st.stats.record(Counter::StaleHits);
                        (Arc::new(s), "STALE")
                    }
                    None => {
                        let e =
                            match failure {
                                FetchFailure::BreakerOpen(d) => UpstreamError::BreakerOpen(
                                    crate::upstream::throttle::BreakerOpen { retry_after: d },
                                ),
                                FetchFailure::Exhausted(last) => {
                                    UpstreamError::Exhausted { attempts: 0, last }
                                }
                                FetchFailure::Forward(_) => unreachable!("handled above"),
                            };
                        return upstream_failure(e, "MISS");
                    }
                },
                None => {
                    return AppError::Internal(anyhow::anyhow!("upstream fetch task failed"))
                        .into_response();
                }
            }
        }
    };

    let e = &stored.entry;
    let stored_json = match (sp, e.status) {
        (SearchPath::Coordinates, _) | (_, Status::Match) => {
            e.payload.json.as_deref().map(Vec::as_slice)
        }
        (_, Status::NoMatch | Status::Tie) => None,
    };
    let layers = LayerFilter::resolve(req.layers.as_deref(), &vintage.name, &catalog);
    let input = RenderInput {
        stored: stored_json,
        benchmark: &bench.raw,
        vintage: (rt == ReturnType::Geographies).then_some(&vintage.raw),
        layers: &layers,
    };
    match render(&req, &input) {
        Ok((body, ct)) => body_response(StatusCode::OK, ct, body, x_cache),
        Err(e) => {
            AppError::Internal(anyhow::anyhow!("rendering cached entry: {e}")).into_response()
        }
    }
}

/// One coalesced upstream fetch + store.
struct FetchJob {
    state: SharedState,
    key: CacheKey,
    input: KeyInput,
    benchmark_id: u32,
    vintage_id: u32,
    fetch_geo: bool,
    need: Need,
    class: BenchmarkClass,
    existing: Option<Stored>,
}

impl FetchJob {
    async fn run(self) -> FetchResult {
        let st = &self.state;
        // Another fetch may have stored this key between our lookup and joining.
        if let Ok(Some(bytes)) = st.store.get_entry(self.key.as_str()) {
            let cfg = st.settings();
            if let Lookup::Hit(s) = classify(
                Some(&bytes),
                st.codec,
                &cfg.config,
                self.class,
                self.need,
                SystemTime::now(),
            ) {
                return Ok(Arc::new(s));
            }
        }

        let q = SingleQuery {
            input: &self.input,
            benchmark_id: self.benchmark_id,
            vintage_id: self.fetch_geo.then_some(self.vintage_id),
        };
        let resp = match st.upstream.fetch_single(Priority::Sync, q).await {
            Ok(r) => r,
            Err(UpstreamError::Rejected(r)) => return Err(FetchFailure::Forward(r)),
            Err(UpstreamError::BreakerOpen(b)) => {
                return Err(FetchFailure::BreakerOpen(b.retry_after));
            }
            Err(UpstreamError::Exhausted { last, .. }) => {
                st.stats.record(Counter::UpstreamErr);
                return Err(FetchFailure::Exhausted(last));
            }
        };
        st.stats.record(Counter::UpstreamOk);
        let single = match UpstreamSingle::from_response(&resp.body) {
            Ok(s) => s,
            Err(e) => {
                warn!(error = %e, key = %self.key, "upstream 200 body not storable; forwarding as-is");
                return Err(FetchFailure::Forward(resp));
            }
        };

        let mut entry = Entry {
            status: single.status,
            source: EntrySource::Single,
            views: single.views,
            verified_at: unix_now(),
            payload: single.payload,
        };
        // Keep a batch view written earlier for this key.
        if let Some(batch) = self.existing.and_then(|s| s.entry.payload.batch) {
            entry.payload.batch = Some(batch);
            entry.views = entry.views | Views::BATCH;
        }
        let now = SystemTime::now();
        match entry.encode(st.codec) {
            Ok(envelope) => {
                let key = self.key.to_string();
                let value = encode_stored(&envelope, now);
                let written = if self.fetch_geo {
                    st.store.put_entry(key, value).await
                } else {
                    // A locations-only fetch (fetch_geographies_always=false) must not
                    // replace a still-fresh geographies entry (e.g. one a concurrent fetch
                    // stored); checked atomically inside the write transaction.
                    let ttl_cfg = st.settings().config.ttl.clone();
                    let class = self.class;
                    st.store
                        .put_entry_unless(key, value, move |h, created_at| {
                            h.views.contains(Views::GEO)
                                && freshness(
                                    created_at,
                                    SystemTime::now(),
                                    ttl(&ttl_cfg, h.status, h.source, class),
                                    false,
                                ) == Freshness::Fresh
                        })
                        .await
                        .map(drop)
                };
                if let Err(e) = written {
                    warn!(error = %e, key = %self.key, "storing entry failed; serving uncached");
                }
            }
            Err(e) => warn!(error = %e, key = %self.key, "encoding entry failed; serving uncached"),
        }
        Ok(Arc::new(Stored {
            entry,
            created_at: now,
        }))
    }
}

// ---------------------------------------------------------------- catalog endpoints

/// Rewrite `selectedBenchmark` to the alias the caller used (Census echoes it).
fn with_selected_benchmark(body: &[u8], alias: &str) -> Vec<u8> {
    match serde_json::from_slice::<Value>(body) {
        Ok(Value::Object(mut m)) if m.contains_key("selectedBenchmark") => {
            m.insert("selectedBenchmark".into(), Value::String(alias.to_owned()));
            serde_json::to_vec(&Value::Object(m)).unwrap_or_else(|_| body.to_vec())
        }
        _ => body.to_vec(),
    }
}

/// Apply `f` to the latest catalog with compare-and-swap, so concurrent refreshes of
/// different documents never undo each other. Returns whether `f` accepted the update.
fn update_catalog(st: &AppState, f: impl Fn(&Catalog) -> Option<Catalog>) -> bool {
    let mut accepted = false;
    st.catalog.rcu(|cur| match f(cur) {
        Some(c) => {
            accepted = true;
            Arc::new(c)
        }
        None => {
            accepted = false;
            Arc::clone(cur)
        }
    });
    accepted
}

/// Serve a catalog document from the store when fresh, else refetch; fall back to a
/// stale stored copy, then to the embedded snapshot. A fetched body is cached only
/// if it parses as the expected document.
async fn catalog_item(
    st: &AppState,
    store_key: &str,
    fetch: impl Future<Output = Result<UpstreamResponse, UpstreamError>>,
    snapshot: Option<Value>,
    apply: impl Fn(&Catalog, &Value) -> Option<Catalog>,
) -> Result<(Vec<u8>, &'static str), Box<Response>> {
    let ttl = st.settings().config.ttl.catalog;
    let stored = st.store.catalog_get(store_key).unwrap_or_else(|e| {
        warn!(error = %e, "catalog read failed");
        None
    });
    if let Some((raw, at)) = &stored
        && (ttl.is_zero() || unix_now().saturating_sub(*at) < ttl.as_secs())
    {
        return Ok((raw.clone(), "HIT"));
    }
    match fetch.await {
        Ok(r) => {
            let valid = serde_json::from_slice::<Value>(&r.body)
                .ok()
                .is_some_and(|doc| update_catalog(st, |c| apply(c, &doc)));
            if valid {
                if let Err(e) = st
                    .store
                    .catalog_put(store_key.to_owned(), r.body.to_vec(), unix_now())
                    .await
                {
                    warn!(error = %e, "storing catalog item failed");
                }
                return Ok((r.body.to_vec(), "MISS"));
            }
            warn!(
                key = store_key,
                "upstream catalog body failed validation; not cached"
            );
            match stored {
                Some((raw, _)) => Ok((raw, "STALE")),
                None => Err(Box::new(forward(r, "MISS"))),
            }
        }
        Err(UpstreamError::Rejected(r)) => Err(Box::new(forward(r, "MISS"))),
        Err(e) => match (stored, snapshot) {
            (Some((raw, _)), _) => Ok((raw, "STALE")),
            (None, Some(doc)) => Ok((serde_json::to_vec(&doc).unwrap_or_default(), "STALE")),
            (None, None) => Err(Box::new(upstream_failure(e, "MISS"))),
        },
    }
}

async fn benchmarks(State(st): State<SharedState>) -> Response {
    let snapshot = Some(st.catalog.load().benchmarks_doc().clone());
    let fetch = st.upstream.fetch_benchmarks(Priority::Sync);
    match catalog_item(&st, CATALOG_BENCHMARKS, fetch, snapshot, |c, doc| {
        c.clone().with_benchmarks_doc(doc.clone()).ok()
    })
    .await
    {
        Ok((body, x)) => body_response(StatusCode::OK, CT_JSON, body, x),
        Err(resp) => *resp,
    }
}

async fn vintages(State(st): State<SharedState>, RawQuery(query): RawQuery) -> Response {
    let raw_query = query.unwrap_or_default();
    let alias = form_urlencoded::parse(raw_query.as_bytes())
        .find(|(k, _)| k == "benchmark")
        .map(|(_, v)| v.trim().to_owned())
        .filter(|v| !v.is_empty());
    let catalog = st.catalog.load_full();
    let (Some(alias), Some(id)) = (
        alias.clone(),
        alias
            .as_deref()
            .and_then(|a| catalog.resolve_benchmark(a))
            .map(|b| b.id),
    ) else {
        return passthrough(&st, "vintages", &raw_query).await;
    };
    let snapshot = catalog.vintages_doc(id).cloned();
    let fetch = st.upstream.fetch_vintages(Priority::Sync, &alias);
    let key = catalog_vintages_key(id);
    match catalog_item(&st, &key, fetch, snapshot, |c, doc| {
        c.clone().with_vintages_doc(id, doc.clone()).ok()
    })
    .await
    {
        Ok((body, x)) => body_response(
            StatusCode::OK,
            CT_JSON,
            with_selected_benchmark(&body, &alias),
            x,
        ),
        Err(resp) => *resp,
    }
}

/// Refresh benchmarks and every benchmark's vintages at background priority, seeding
/// the store. Run once at startup so alias resolution reflects Census's current list.
pub async fn refresh_catalog(st: SharedState) {
    let fetched = match st.upstream.fetch_benchmarks(Priority::Background).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "catalog refresh: benchmarks unavailable; using cached/snapshot catalog");
            return;
        }
    };
    let Ok(doc) = serde_json::from_slice::<Value>(&fetched.body) else {
        warn!("catalog refresh: benchmarks body is not JSON");
        return;
    };
    if !update_catalog(&st, |c| c.clone().with_benchmarks_doc(doc.clone()).ok()) {
        warn!("catalog refresh: unexpected benchmarks shape");
        return;
    }
    let _ = st
        .store
        .catalog_put(CATALOG_BENCHMARKS.into(), fetched.body.to_vec(), unix_now())
        .await;
    let ids: Vec<u32> = st
        .catalog
        .load()
        .benchmarks()
        .iter()
        .map(|b| b.id)
        .collect();
    for id in ids {
        match st
            .upstream
            .fetch_vintages(Priority::Background, &id.to_string())
            .await
        {
            Ok(r) => {
                let valid = serde_json::from_slice::<Value>(&r.body)
                    .ok()
                    .is_some_and(|doc| {
                        update_catalog(&st, |c| c.clone().with_vintages_doc(id, doc.clone()).ok())
                    });
                if valid {
                    let _ = st
                        .store
                        .catalog_put(catalog_vintages_key(id), r.body.to_vec(), unix_now())
                        .await;
                }
            }
            Err(e) => warn!(error = %e, benchmark = id, "catalog refresh: vintages unavailable"),
        }
    }
    debug!("catalog refreshed");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn selected_benchmark_rewrite() {
        let body = br#"{"vintages":[],"selectedBenchmark":"4","benchmarks":[]}"#;
        assert_eq!(
            with_selected_benchmark(body, "Public_AR_Current"),
            br#"{"vintages":[],"selectedBenchmark":"Public_AR_Current","benchmarks":[]}"#
        );
        assert_eq!(with_selected_benchmark(b"[1]", "x"), b"[1]");
    }
}
