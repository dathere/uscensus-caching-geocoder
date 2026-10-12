//! Shared harness: a mock Census upstream, a temp store, and the app router.
#![allow(dead_code)] // each test binary uses a different subset

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
};
use http_body_util::BodyExt;
use tempfile::TempDir;
use tower::ServiceExt;
use ucgeocoder::{
    config::{self, Config, Layers},
    http::router,
    state::{AppState, SharedState},
    store::{Store, StoreOptions},
    upstream::Upstream,
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

pub struct Harness {
    pub server: MockServer,
    pub state: SharedState,
    pub app: Router,
    pub dir: TempDir,
}

pub fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(format!(
        "{}/tests/fixtures/live/{name}",
        env!("CARGO_MANIFEST_DIR")
    ))
    .unwrap()
}

/// Config pointed at `server` with a fast, test-friendly throttle.
pub fn test_config(server: &MockServer, dir: &TempDir) -> Config {
    let mut c = Config::default();
    c.upstream.url = server.uri();
    c.store.db_dir = dir.path().to_owned();
    c.store.writer_flush_ms = 1;
    c.throttle.single_rps = 1000;
    c.throttle.single_burst = 1000;
    c.throttle.retry_base_ms = 1;
    c.throttle.background_poll_ms = 5;
    c.throttle.batch_min_interval_secs = 0;
    c
}

pub async fn harness(tweak: impl FnOnce(&mut Config)) -> Harness {
    let server = MockServer::start().await;
    let dir = tempfile::tempdir().unwrap();
    let mut settings = config::load(&Layers {
        toml: None,
        env: &|_| None,
        flags: &[],
    })
    .unwrap();
    let mut cfg = test_config(&server, &dir);
    tweak(&mut cfg);
    settings.config = cfg;
    let store = Store::open(&StoreOptions::from(&settings.config.store)).unwrap();
    let upstream = Upstream::new(&settings.config).unwrap();
    let state = AppState::new(settings, store, upstream);
    let app = router(state.clone());
    Harness {
        server,
        state,
        app,
        dir,
    }
}

impl Harness {
    pub async fn get(&self, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
        self.request(Request::get(uri).body(Body::empty()).unwrap())
            .await
    }

    pub async fn request(&self, req: Request<Body>) -> (StatusCode, HeaderMap, Bytes) {
        let resp = self.app.clone().oneshot(req).await.unwrap();
        let status = resp.status();
        let headers = resp.headers().clone();
        let body = resp.into_body().collect().await.unwrap().to_bytes();
        (status, headers, body)
    }

    pub async fn hits(&self) -> usize {
        self.server.received_requests().await.unwrap().len()
    }

    /// Serve `body` (a fixture) for every GET to `p`.
    pub async fn mock_json(&self, p: &str, body: Vec<u8>) {
        Mock::given(method("GET"))
            .and(path(p))
            .respond_with(ResponseTemplate::new(200).set_body_raw(body, "application/json"))
            .mount(&self.server)
            .await;
    }
}

pub fn x_cache(h: &HeaderMap) -> &str {
    h.get("x-cache").map_or("", |v| v.to_str().unwrap())
}

pub fn json(b: &[u8]) -> serde_json::Value {
    serde_json::from_slice(b).unwrap()
}
