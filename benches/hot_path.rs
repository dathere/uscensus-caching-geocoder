//! HIT-path latency (PLAN.md §9.10: p50 < 200 µs on a warm db). Each iteration is a full
//! in-process request through the router: routing, query parsing, keying, store read,
//! envelope decode (zstd + dictionary), view projection/layer filtering and rendering.
//!
//! The upstream points at a closed port with no retries, so a miss would error out
//! instead of quietly measuring something else. Report criterion's *median*.

use std::time::SystemTime;

use axum::{
    Router,
    body::Body,
    http::{Request, StatusCode},
};
use criterion::{Criterion, criterion_group, criterion_main};
use http_body_util::BodyExt;
use tower::ServiceExt;
use ucgeocoder::{
    cache::{
        codec::Codec,
        envelope::{Entry, EntrySource, UpstreamSingle, encode_stored},
        key::{CacheKey, KeyInput},
    },
    config::{self, Layers},
    http::router,
    state::{AppState, SharedState},
    store::{Store, StoreOptions},
    upstream::Upstream,
};

const FIXTURE: &[u8] = include_bytes!("../tests/fixtures/live/geo_address_all.json");
const BASE: &str = "/geocoder/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&vintage=4&format=json";

fn setup(dir: &std::path::Path) -> (SharedState, Router) {
    let mut settings = config::load(&Layers {
        toml: None,
        env: &|_| None,
        flags: &[],
    })
    .unwrap();
    settings.config.store.db_dir = dir.to_owned();
    settings.config.upstream.url = "http://127.0.0.1:9".into();
    settings.config.throttle.max_retries = 0;
    let store = Store::open(&StoreOptions::from(&settings.config.store)).unwrap();
    let upstream = Upstream::new(&settings.config).unwrap();
    let state = AppState::new(settings, store, upstream);
    let app = router(state.clone());
    (state, app)
}

async fn seed(state: &SharedState) {
    let single = UpstreamSingle::from_response(FIXTURE).unwrap();
    let entry = Entry {
        status: single.status,
        source: EntrySource::Single,
        views: single.views,
        verified_at: 0,
        payload: single.payload,
    };
    let key = CacheKey::new(
        4,
        4,
        &KeyInput::Address {
            street: "4600 Silver Hill Rd".into(),
            city: "Washington".into(),
            state: "DC".into(),
            zip: String::new(),
        },
    );
    let stored = encode_stored(&entry.encode(Codec::embedded()).unwrap(), SystemTime::now());
    state
        .store
        .put_entry(key.to_string(), stored)
        .await
        .unwrap();
}

async fn hit(app: &Router, uri: &str) -> usize {
    let resp = app
        .clone()
        .oneshot(Request::get(uri).body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.headers()["x-cache"], "HIT");
    resp.into_body().collect().await.unwrap().to_bytes().len()
}

fn hot_path(c: &mut Criterion) {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let (state, app) = rt.block_on(async {
        let (state, app) = setup(dir.path());
        seed(&state).await;
        (state, app)
    });

    let cases = [
        ("hit_geographies_layers_all", format!("{BASE}&layers=all")),
        ("hit_geographies_default_layers", BASE.to_owned()),
        (
            "hit_locations",
            "/geocoder/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&format=json"
                .to_owned(),
        ),
    ];
    // Warm the page cache and check every case is a HIT before measuring.
    for (_, uri) in &cases {
        rt.block_on(hit(&app, uri));
    }
    let mut group = c.benchmark_group("single_record");
    for (name, uri) in &cases {
        group.bench_function(*name, |b| b.to_async(&rt).iter(|| hit(&app, uri)));
    }
    group.finish();
    drop(app);
    drop(state);
}

criterion_group!(benches, hot_path);
criterion_main!(benches);
