//! Upstream retry/backoff/breaker/priority behaviour against a mock Census server.

use std::{
    sync::Arc,
    time::{Duration, Instant},
};

use ucgeocoder::{
    config::Config,
    upstream::{Upstream, UpstreamError, throttle::Priority},
};
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{method, path},
};

fn config(server: &MockServer) -> Config {
    let mut c = Config::default();
    c.upstream.url = server.uri();
    c.throttle.single_rps = 1000;
    c.throttle.single_burst = 1000;
    c.throttle.retry_base_ms = 1;
    c.throttle.background_poll_ms = 5;
    c.throttle.batch_min_interval_secs = 0;
    c
}

async fn hits(server: &MockServer) -> usize {
    server.received_requests().await.unwrap().len()
}

#[tokio::test]
async fn honors_retry_after_on_429() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(ResponseTemplate::new(429).insert_header("Retry-After", "1"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(ResponseTemplate::new(200).set_body_string(r#"{"benchmarks":[]}"#))
        .mount(&server)
        .await;

    let up = Upstream::new(&config(&server)).unwrap();
    let start = Instant::now();
    let resp = up.fetch_benchmarks(Priority::Sync).await.unwrap();
    assert_eq!(resp.body.as_ref(), br#"{"benchmarks":[]}"#);
    assert!(
        start.elapsed() >= Duration::from_millis(950),
        "{:?}",
        start.elapsed()
    );
    assert_eq!(hits(&server).await, 2);
    assert!(up.last_ok_at().is_some());
    assert_eq!(up.recent_errors().len(), 1);
}

#[tokio::test]
async fn backs_off_exponentially_on_503() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    // UCG_RETRY_BASE_MS=1: all retries happen almost immediately.
    let up = Upstream::new(&config(&server)).unwrap();
    let start = Instant::now();
    let err = up.fetch_benchmarks(Priority::Sync).await.unwrap_err();
    assert!(
        matches!(err, UpstreamError::Exhausted { attempts: 4, .. }),
        "{err}"
    );
    assert_eq!(hits(&server).await, 4, "1 try + max_retries=3");
    assert!(start.elapsed() < Duration::from_secs(2));

    // With a 25 ms base the waits are at least 25 + 50 + 100 ms.
    let mut c = config(&server);
    c.throttle.retry_base_ms = 25;
    let up = Upstream::new(&c).unwrap();
    let start = Instant::now();
    up.fetch_benchmarks(Priority::Sync).await.unwrap_err();
    assert!(
        start.elapsed() >= Duration::from_millis(175),
        "{:?}",
        start.elapsed()
    );
}

#[tokio::test]
async fn census_400_is_forwarded_without_retry() {
    let server = MockServer::start().await;
    let body = r#"{"errors":["Benchmark must be specified"],"status":"400"}"#;
    Mock::given(method("GET"))
        .and(path("/vintages"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(body, "application/json"))
        .mount(&server)
        .await;
    let up = Upstream::new(&config(&server)).unwrap();
    let err = up.fetch_vintages(Priority::Sync, "").await.unwrap_err();
    let UpstreamError::Rejected(r) = err else {
        panic!("expected Rejected, got {err}");
    };
    assert_eq!(r.status, 400);
    assert_eq!(r.body.as_ref(), body.as_bytes());
    assert_eq!(r.content_type.as_deref(), Some("application/json"));
    assert_eq!(hits(&server).await, 1);
    assert!(!up.throttle.breaker_state().open);
}

#[tokio::test]
async fn breaker_opens_after_ten_failures_and_resets_on_success() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(ResponseTemplate::new(503))
        .with_priority(5)
        .mount(&server)
        .await;
    let mut c = config(&server);
    c.throttle.max_retries = 0;
    c.throttle.breaker_open_secs = 1;
    let up = Upstream::new(&c).unwrap();

    for i in 0..10 {
        assert!(!up.throttle.breaker_state().open, "opened early at {i}");
        let e = up.fetch_benchmarks(Priority::Sync).await.unwrap_err();
        assert!(matches!(e, UpstreamError::Exhausted { .. }));
    }
    let state = up.throttle.breaker_state();
    assert!(state.open);
    assert_eq!(state.consecutive_failures, 10);

    let e = up.fetch_benchmarks(Priority::Sync).await.unwrap_err();
    assert!(matches!(e, UpstreamError::BreakerOpen(_)), "{e}");
    assert_eq!(
        hits(&server).await,
        10,
        "an open breaker sends nothing upstream"
    );

    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(ResponseTemplate::new(200).set_body_string("{}"))
        .with_priority(1)
        .mount(&server)
        .await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    up.fetch_benchmarks(Priority::Sync).await.unwrap();
    let state = up.throttle.breaker_state();
    assert!(!state.open);
    assert_eq!(state.consecutive_failures, 0);
}

#[tokio::test]
async fn background_waits_for_queued_sync_requests() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/vintages"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_string("{}")
                .set_delay(Duration::from_millis(150)),
        )
        .mount(&server)
        .await;
    let mut c = config(&server);
    c.throttle.single_concurrency = 1;
    let up = Arc::new(Upstream::new(&c).unwrap());

    let a = {
        let up = Arc::clone(&up);
        tokio::spawn(async move { up.fetch_vintages(Priority::Sync, "A").await })
    };
    // A holds the only permit while upstream is "paused" for 150 ms.
    while hits(&server).await == 0 {
        tokio::time::sleep(Duration::from_millis(2)).await;
    }
    let c_bg = {
        let up = Arc::clone(&up);
        tokio::spawn(async move { up.fetch_vintages(Priority::Background, "C").await })
    };
    tokio::time::sleep(Duration::from_millis(20)).await;
    let b = {
        let up = Arc::clone(&up);
        tokio::spawn(async move { up.fetch_vintages(Priority::Sync, "B").await })
    };
    for t in [a, b, c_bg] {
        t.await.unwrap().unwrap();
    }

    let order: Vec<String> = server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .map(|r| {
            r.url
                .query_pairs()
                .find(|(k, _)| k == "benchmark")
                .map(|(_, v)| v.into_owned())
                .unwrap()
        })
        .collect();
    assert_eq!(
        order,
        ["A", "B", "C"],
        "background started before the queued sync request"
    );
}
