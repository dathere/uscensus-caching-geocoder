//! Async jobs (PLAN.md §6.3) against a mock upstream that answers each uploaded row.

mod common;

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::{Duration, Instant},
};

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
};
use common::{Harness, fixture, harness, harness_with, json, multipart, x_cache};
use serde_json::{Value, json};
use ucgeocoder::{
    cache::{
        envelope::{EntrySource, Status},
        key::{CacheKey, KeyInput},
        views::decode,
    },
    jobs::{JobKind, JobRecord, JobState, worker},
};
use wiremock::{
    Mock, Request as MockRequest, Respond, ResponseTemplate,
    matchers::{method, path, query_param},
};

/// Answers every uploaded batch row: addresses containing "nowhere" are `No_Match`
/// unless `matches` is set; everything else matches (geographies-width rows).
/// Requests after the first `fast` ones are delayed by `stall`.
#[derive(Clone)]
struct Census {
    matches: Arc<AtomicBool>,
    seen: Arc<AtomicUsize>,
    fast: usize,
    stall: Duration,
}

impl Census {
    fn new() -> Self {
        Self {
            matches: Arc::new(AtomicBool::new(false)),
            seen: Arc::new(AtomicUsize::new(0)),
            fast: usize::MAX,
            stall: Duration::from_secs(60),
        }
    }
}

impl Respond for Census {
    fn respond(&self, req: &MockRequest) -> ResponseTemplate {
        let n = self.seen.fetch_add(1, Ordering::SeqCst);
        let coords = req.url.path().ends_with("coordinatesbatch");
        let body = String::from_utf8_lossy(&req.body).into_owned();
        let mut out = String::new();
        for line in body
            .lines()
            .filter(|l| l.contains(',') && l.starts_with(|c: char| c.is_ascii_digit()))
        {
            let rec = csv::ReaderBuilder::new()
                .has_headers(false)
                .from_reader(line.as_bytes())
                .records()
                .next()
                .unwrap()
                .unwrap();
            let f: Vec<&str> = rec.iter().collect();
            if coords {
                out += &format!(
                    "\"{}\",\"{}\",\"{}\",\"Match\",\"24\",\"033\",\"802405\",\"2004\"\n",
                    f[0], f[1], f[2]
                );
                continue;
            }
            let input = format!("{}, {}, {}, {}", f[1], f[2], f[3], f[4]);
            if f[1].to_lowercase().contains("nowhere") && !self.matches.load(Ordering::SeqCst) {
                out += &format!("\"{}\",\"{input}\",\"No_Match\"\n", f[0]);
            } else {
                out += &format!(
                    "\"{}\",\"{input}\",\"Match\",\"Exact\",\"{}\",\"-76.9,38.8\",\"123\",\"L\",\"24\",\"033\",\"802405\",\"2004\"\n",
                    f[0],
                    input.to_uppercase()
                );
            }
        }
        let t = ResponseTemplate::new(200).set_body_raw(out, "text/plain");
        if n >= self.fast {
            t.set_delay(self.stall)
        } else {
            t
        }
    }
}

async fn mount(h: &Harness, c: &Census) {
    Mock::given(method("POST"))
        .respond_with(c.clone())
        .mount(&h.server)
        .await;
}

async fn send_json(h: &Harness, uri: &str, body: &Value) -> (StatusCode, HeaderMap, Bytes) {
    h.request(
        Request::post(uri)
            .header("content-type", "application/json")
            .body(Body::from(body.to_string()))
            .unwrap(),
    )
    .await
}

async fn post_empty(h: &Harness, uri: &str) -> (StatusCode, HeaderMap, Bytes) {
    h.request(Request::post(uri).body(Body::empty()).unwrap())
        .await
}

async fn upload(
    h: &Harness,
    uri: &str,
    file_field: &str,
    file: &[u8],
    fields: &[(&str, &str)],
) -> (StatusCode, HeaderMap, Bytes) {
    let (ct, body) = multipart(file_field, file, fields);
    h.request(
        Request::post(uri)
            .header("content-type", ct)
            .body(Body::from(body))
            .unwrap(),
    )
    .await
}

/// Batch uploads seen upstream, as their data lines.
async fn uploads(h: &Harness) -> Vec<Vec<String>> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| {
            String::from_utf8_lossy(&r.body)
                .lines()
                .filter(|l| l.contains(',') && l.starts_with(|c: char| c.is_ascii_digit()))
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

async fn wait_for(h: &Harness, id: &str, done: impl Fn(&JobRecord) -> bool) -> JobRecord {
    let start = Instant::now();
    loop {
        let job = h.state.store.job_get(id).unwrap().expect("job exists");
        if done(&job) {
            return job;
        }
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "timed out; job is {job:?}"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
}

fn finished(j: &JobRecord) -> bool {
    matches!(j.state, JobState::Completed | JobState::Failed)
}

fn id_of(body: &[u8]) -> String {
    json(body)["id"].as_str().unwrap().to_owned()
}

async fn results(h: &Harness, id: &str, query: &str) -> (HeaderMap, String) {
    let (status, headers, body) = h.get(&format!("/api/v1/jobs/{id}/results{query}")).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    (headers, String::from_utf8(body.to_vec()).unwrap())
}

const NOWHERE_ROW: &[u8] = b"8,zzzz nowhere,,,\n";

#[tokio::test]
async fn multipart_job_skips_cached_rows_and_drains() {
    let h = harness(|_| {}).await;
    let census = Census::new();
    mount(&h, &census).await;
    // Warm two rows through the synchronous batch endpoint.
    let warm = b"1,4600 Silver Hill Rd,Washington,DC,20233\n3,1600 Pennsylvania Ave NW,Washington,DC,20500\n";
    let (status, _, _) = upload(
        &h,
        "/geocoder/locations/addressbatch",
        "addressFile",
        warm,
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(status, 200);

    let mut file = warm.to_vec();
    file.extend_from_slice(b"7,10 Main St,Springfield,IL,62701\n");
    file.extend_from_slice(NOWHERE_ROW);
    file.extend_from_slice(b"9,10 Main St,Springfield,IL,62701\n");
    let (status, _, body) = upload(
        &h,
        "/api/v1/jobs",
        "addressFile",
        &file,
        &[("returntype", "locations"), ("benchmark", "4")],
    )
    .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let created = json(&body);
    assert_eq!(created["state"], "queued");
    assert_eq!(created["counts"]["total"], 5);
    assert_eq!(created["counts"]["cached"], 2);
    assert_eq!(created["counts"]["pending"], 3);
    let id = id_of(&body);

    let jobs = worker::spawn(h.state.clone());
    let job = wait_for(&h, &id, finished).await;
    assert_eq!(job.state, JobState::Completed);
    assert_eq!(
        (
            job.totals.matched,
            job.totals.no_match,
            job.totals.cached,
            job.totals.pending
        ),
        (4, 1, 2, 0)
    );
    // One upload for the warm-up, one for the job's misses (the repeated address once).
    let up = uploads(&h).await;
    assert_eq!(up.len(), 2);
    assert_eq!(up[1].len(), 2, "{:?}", up[1]);
    assert_eq!(h.state.store.retry_len().unwrap(), 1);

    let (headers, csv) = results(&h, &id, "").await;
    assert_eq!(headers["content-type"], "text/plain");
    let ids: Vec<&str> = csv
        .lines()
        .map(|l| l.split(',').next().unwrap().trim_matches('"'))
        .collect();
    assert_eq!(ids, ["1", "3", "7", "8", "9"]);
    assert_eq!(
        csv.lines().nth(3).unwrap(),
        r#""8","zzzz nowhere, , , ","No_Match""#
    );
    // locations width: 8 columns for a match.
    assert_eq!(csv.lines().nth(2).unwrap().matches("\",\"").count(), 7);

    let (headers, lines) = results(&h, &id, "?format=json&only=unmatched").await;
    assert_eq!(headers["content-type"], "application/x-ndjson");
    let lines: Vec<Value> = lines.lines().map(|l| json(l.as_bytes())).collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["id"], "8");
    assert_eq!(lines[0]["result"], "No_Match");

    let (status, _, body) = h.get(&format!("/api/v1/jobs/{id}")).await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["progress"], 100.0);
    jobs.shutdown().await;
}

#[tokio::test]
async fn json_oneline_job_runs_single_records_under_the_limiter() {
    let h = harness(|c| {
        c.throttle.single_rps = 20;
        c.throttle.single_burst = 1;
    })
    .await;
    h.mock_json(
        "/geographies/onelineaddress",
        fixture("geo_address_all.json"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/geographies/onelineaddress"))
        .and(query_param("address", "zzzz nowhere"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("loc_nomatch.json"), "application/json"),
        )
        .with_priority(1)
        .mount(&h.server)
        .await;
    let rows: Vec<Value> = ["1 A St", "2 B St", "3 C St", "zzzz nowhere"]
        .iter()
        .enumerate()
        .map(|(i, a)| json!({"id": i + 1, "address": a}))
        .collect();
    let (status, _, body) = send_json(
        &h,
        "/api/v1/jobs",
        &json!({"returntype": "locations", "benchmark": "Public_AR_Current", "rows": rows}),
    )
    .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let id = id_of(&body);
    let start = Instant::now();
    let jobs = worker::spawn(h.state.clone());
    let job = wait_for(&h, &id, finished).await;
    // Four single-record calls at 20/s with burst 1: at least three 50 ms gaps.
    assert!(start.elapsed() >= Duration::from_millis(140));
    assert_eq!(job.state, JobState::Completed);
    assert_eq!((job.totals.matched, job.totals.no_match), (3, 1));
    assert_eq!(job.search_type.as_str(), "oneline");
    assert_eq!(h.hits().await, 4);

    let (_, lines) = results(&h, &id, "?format=json").await;
    let lines: Vec<Value> = lines.lines().map(|l| json(l.as_bytes())).collect();
    assert_eq!(lines.len(), 4);
    assert_eq!(lines[0]["id"], "1");
    assert_eq!(lines[0]["input"], "1 A St");
    assert_eq!(lines[0]["result"], "Match");
    assert_eq!(
        lines[0]["matched_address"],
        "4600 SILVER HILL RD, WASHINGTON, DC, 20233"
    );
    assert_eq!(lines[3]["result"], "No_Match");

    // The answers are cache entries: the same addresses as a new job are all cached.
    let (status, _, body) = send_json(
        &h,
        "/api/v1/jobs",
        &json!({"returntype": "locations", "benchmark": "4", "rows": [{"id": "x", "address": "1 A St"}]}),
    )
    .await;
    assert_eq!(status, 201);
    assert_eq!(json(&body)["state"], "completed");
    assert_eq!(json(&body)["counts"]["cached"], 1);
    jobs.shutdown().await;
}

#[tokio::test]
async fn json_coordinates_job_and_submit_validation() {
    let h = harness(|_| {}).await;
    mount(&h, &Census::new()).await;
    let (status, _, body) = send_json(
        &h,
        "/api/v1/jobs",
        &json!({"returntype": "geographies", "benchmark": "4", "vintage": "4",
                "rows": [{"id": "a", "x": -76.92744, "y": 38.84601}, {"id": "b", "x": "-77.0365", "y": "38.8977"}]}),
    )
    .await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let id = id_of(&body);
    let jobs = worker::spawn(h.state.clone());
    wait_for(&h, &id, finished).await;
    let (_, csv) = results(&h, &id, "").await;
    assert_eq!(
        csv,
        "\"a\",\"-76.92744\",\"38.84601\",\"Match\",\"24\",\"033\",\"802405\",\"2004\"\n\
         \"b\",\"-77.0365\",\"38.8977\",\"Match\",\"24\",\"033\",\"802405\",\"2004\"\n"
    );

    for (body, msg) in [
        (
            json!({"returntype": "locations", "benchmark": "4", "rows": [{"id": 1, "x": 1, "y": 2}]}),
            "Coordinates require returntype geographies",
        ),
        (
            json!({"returntype": "locations", "benchmark": "4", "rows": [{"id": 1, "address": "x"}, {"id": 2, "street": "y"}]}),
            "Malformed rows (all rows must be oneline records): 2",
        ),
        (
            json!({"returntype": "geographies", "benchmark": "4", "rows": [{"id": 1, "address": "x"}]}),
            "Vintage must be specified",
        ),
        (
            json!({"returntype": "locations", "benchmark": "4", "rows": []}),
            "Job contains no records",
        ),
        (
            json!({"returntype": "nope", "benchmark": "4", "rows": []}),
            "returntype must be locations or geographies",
        ),
    ] {
        let (status, _, resp) = send_json(&h, "/api/v1/jobs", &body).await;
        assert_eq!(status, 400, "{body}");
        assert_eq!(json(&resp)["errors"][0], msg, "{body}");
    }
    jobs.shutdown().await;
}

#[tokio::test]
async fn pause_resume_cancel_and_delete() {
    let h = harness(|_| {}).await;
    mount(&h, &Census::new()).await;
    let submit = |street: &'static str| {
        let h = &h;
        async move {
            let (status, _, body) = send_json(
                h,
                "/api/v1/jobs",
                &json!({"returntype": "locations", "benchmark": "4", "rows": [{"id": 1, "street": street}]}),
            )
            .await;
            assert_eq!(status, 201);
            id_of(&body)
        }
    };
    let a = submit("1 A St").await;
    let b = submit("2 B St").await;

    let (status, _, body) = post_empty(&h, &format!("/api/v1/jobs/{a}/pause")).await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["state"], "paused");
    let (status, _, body) = post_empty(&h, &format!("/api/v1/jobs/{b}/cancel")).await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["state"], "cancelled");

    let jobs = worker::spawn(h.state.clone());
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(uploads(&h).await.is_empty());
    assert_eq!(
        h.state.store.job_get(&a).unwrap().unwrap().state,
        JobState::Paused
    );

    let (status, _, body) = post_empty(&h, &format!("/api/v1/jobs/{a}/resume")).await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["state"], "queued");
    let job = wait_for(&h, &a, finished).await;
    assert_eq!(job.state, JobState::Completed);
    assert_eq!(uploads(&h).await.len(), 1);
    let cancelled = h.state.store.job_get(&b).unwrap().unwrap();
    assert_eq!(
        (cancelled.state, cancelled.totals.pending),
        (JobState::Cancelled, 1)
    );
    let (_, csv) = results(&h, &b, "").await;
    assert_eq!(csv, "\"1\",\"2 B St, , , \",\"Pending\"\n");

    for (action, id) in [("cancel", &a), ("pause", &a), ("resume", &b)] {
        let (status, _, _) = post_empty(&h, &format!("/api/v1/jobs/{id}/{action}")).await;
        assert_eq!(status, 409, "{action}");
    }
    let (status, _, _) = post_empty(&h, "/api/v1/jobs/nope/pause").await;
    assert_eq!(status, 404);

    let (status, _, body) = h.get("/api/v1/jobs?state=completed").await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["jobs"].as_array().unwrap().len(), 1);
    let (_, _, body) = h.get("/api/v1/jobs?limit=1").await;
    let page = json(&body);
    assert_eq!(page["jobs"][0]["id"], a.as_str());
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let (_, _, body) = h
        .get(&format!("/api/v1/jobs?limit=1&cursor={cursor}"))
        .await;
    let page = json(&body);
    assert_eq!(page["jobs"][0]["id"], b.as_str());
    assert!(page["next_cursor"].is_null());

    let req = Request::delete(format!("/api/v1/jobs/{a}"))
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.request(req).await.0, 204);
    assert_eq!(h.get(&format!("/api/v1/jobs/{a}")).await.0, 404);
    jobs.shutdown().await;
}

#[tokio::test]
async fn failing_chunks_are_retried_then_marked_failed() {
    let h = harness(|c| c.jobs.chunk_max_attempts = 2).await;
    Mock::given(method("POST"))
        .respond_with(ResponseTemplate::new(500))
        .mount(&h.server)
        .await;
    let (_, _, body) = send_json(
        &h,
        "/api/v1/jobs",
        &json!({"returntype": "locations", "benchmark": "4", "rows": [{"id": 1, "street": "1 A St"}]}),
    )
    .await;
    let id = id_of(&body);
    let jobs = worker::spawn(h.state.clone());
    let job = wait_for(&h, &id, finished).await;
    assert_eq!(job.state, JobState::Failed);
    assert_eq!(job.totals.failed, 1);
    // 2 chunk attempts x (1 + 3 retries).
    assert_eq!(uploads(&h).await.len(), 8);
    let (_, csv) = results(&h, &id, "").await;
    assert_eq!(csv, "\"1\",\"1 A St, , , \",\"Failed\"\n");
    jobs.shutdown().await;
}

#[tokio::test]
async fn a_running_job_resumes_after_restart() {
    let tweak = |c: &mut ucgeocoder::config::Config| c.jobs.chunk_size = 1;
    let h = harness(tweak).await;
    let mut census = Census::new();
    census.fast = 1; // the second chunk's upload hangs
    mount(&h, &census).await;
    let (_, _, body) = send_json(
        &h,
        "/api/v1/jobs",
        &json!({"returntype": "locations", "benchmark": "4",
                "rows": [{"id": 1, "street": "1 A St"}, {"id": 2, "street": "2 B St"}]}),
    )
    .await;
    let id = id_of(&body);
    let jobs = worker::spawn(h.state.clone());
    wait_for(&h, &id, |j| j.totals.pending == 1).await;
    let start = Instant::now();
    while !h
        .state
        .store
        .job_chunks(&id)
        .unwrap()
        .iter()
        .any(|(_, c)| c.state == ucgeocoder::jobs::ChunkStatus::InFlight)
    {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    // "Kill" the process: stop the runner mid-upload and drop every state reference.
    jobs.shutdown().await;
    let Harness {
        server,
        state,
        app,
        dir,
    } = h;
    drop(app);
    drop(state);

    let h = harness_with(server, dir, tweak);
    h.server.reset().await;
    mount(&h, &Census::new()).await;
    let job = h.state.store.job_get(&id).unwrap().unwrap();
    assert_eq!(job.state, JobState::Running);
    let jobs = worker::spawn(h.state.clone());
    let job = wait_for(&h, &id, finished).await;
    assert_eq!(job.state, JobState::Completed);
    assert_eq!((job.totals.matched, job.totals.pending), (2, 0));
    assert_eq!(uploads(&h).await, vec![vec!["2,2 B St,,,".to_owned()]]);
    jobs.shutdown().await;
}

fn nowhere_key() -> String {
    CacheKey::new(
        4,
        4,
        &KeyInput::Address {
            street: "zzzz nowhere".into(),
            city: String::new(),
            state: String::new(),
            zip: String::new(),
        },
    )
    .to_string()
}

#[tokio::test]
async fn refresh_negatives_upgrades_a_no_match() {
    let h = harness(|c| c.retry.negative_delay = Duration::ZERO).await;
    let census = Census::new();
    mount(&h, &census).await;
    let batch = || {
        upload(
            &h,
            "/geocoder/locations/addressbatch",
            "addressFile",
            NOWHERE_ROW,
            &[("benchmark", "4")],
        )
    };
    let (_, _, body) = batch().await;
    assert_eq!(
        body.as_ref(),
        br#""8","zzzz nowhere, , , ","No_Match""#
            .as_slice()
            .iter()
            .chain(b"\n")
            .copied()
            .collect::<Vec<_>>()
    );
    assert_eq!(h.state.store.retry_len().unwrap(), 1);

    // A job over the same row is answered from the cache (the fresh batch No_Match).
    let (_, _, body) = upload(
        &h,
        "/api/v1/jobs",
        "addressFile",
        NOWHERE_ROW,
        &[("returntype", "locations"), ("benchmark", "4")],
    )
    .await;
    let job_id = id_of(&body);
    assert_eq!(json(&body)["counts"]["cached"], 1);
    assert_eq!(
        results(&h, &job_id, "?only=unmatched")
            .await
            .1
            .lines()
            .count(),
        1
    );

    census.matches.store(true, Ordering::SeqCst);
    let jobs = worker::spawn(h.state.clone());
    let (status, _, body) = send_json(&h, "/api/v1/jobs/refresh-negatives", &json!({})).await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let created = json(&body);
    assert_eq!(created["jobs"].as_array().unwrap().len(), 1);
    assert_eq!(created["jobs"][0]["kind"], "refresh_negatives");
    let id = created["jobs"][0]["id"].as_str().unwrap().to_owned();
    let job = wait_for(&h, &id, finished).await;
    assert_eq!((job.state, job.totals.matched), (JobState::Completed, 1));
    assert_eq!(h.state.store.retry_len().unwrap(), 0);

    // The cache now answers the row as a Match, and the earlier job's results follow.
    let (_, headers, body) = batch().await;
    assert_eq!(x_cache(&headers), "HIT");
    assert!(String::from_utf8_lossy(&body).contains("\"Match\""));
    assert_eq!(results(&h, &job_id, "?only=unmatched").await.1, "");
    jobs.shutdown().await;
}

#[tokio::test]
async fn repeated_no_match_is_confirmed_after_the_last_attempt() {
    let h = harness(|c| {
        c.retry.negative_delay = Duration::ZERO;
        c.retry.negative_attempts = 2;
    })
    .await;
    mount(&h, &Census::new()).await;
    upload(
        &h,
        "/geocoder/locations/addressbatch",
        "addressFile",
        NOWHERE_ROW,
        &[("benchmark", "4")],
    )
    .await;
    let jobs = worker::spawn(h.state.clone());
    let source = || {
        let bytes = h.state.store.get_entry(&nowhere_key()).unwrap().unwrap();
        let e = decode(&bytes, h.state.codec).unwrap().entry;
        (e.status, e.source)
    };

    for (round, queued, src) in [(1, 1, EntrySource::Job), (2, 0, EntrySource::Confirmed)] {
        let (status, _, body) = send_json(
            &h,
            "/api/v1/jobs/refresh-negatives",
            &json!({"benchmark": "4"}),
        )
        .await;
        assert_eq!(status, 201);
        let id = json(&body)["jobs"][0]["id"].as_str().unwrap().to_owned();
        wait_for(&h, &id, finished).await;
        let due = h.state.store.retry_due(u64::MAX, 10).unwrap();
        assert_eq!(due.len(), queued, "round {round}");
        if let Some(item) = due.first() {
            assert_eq!(item.attempts, 1);
        }
        assert_eq!(source(), (Status::NoMatch, src), "round {round}");
    }
    // Nothing left to refresh.
    let (status, _, body) = send_json(&h, "/api/v1/jobs/refresh-negatives", &json!({})).await;
    assert_eq!(status, 201);
    assert!(json(&body)["jobs"].as_array().unwrap().is_empty());
    let (status, _, _) = send_json(
        &h,
        "/api/v1/jobs/refresh-negatives",
        &json!({"vintage": "4"}),
    )
    .await;
    assert_eq!(status, 400);
    jobs.shutdown().await;
}

#[tokio::test]
async fn scheduler_creates_refresh_jobs_on_interval() {
    let h = harness(|c| {
        c.retry.negative_delay = Duration::ZERO;
        c.retry.auto_refresh_interval = Duration::from_millis(50);
    })
    .await;
    mount(&h, &Census::new()).await;
    upload(
        &h,
        "/geocoder/locations/addressbatch",
        "addressFile",
        NOWHERE_ROW,
        &[("benchmark", "4")],
    )
    .await;
    let jobs = worker::spawn(h.state.clone());
    // Re-queued after each No_Match until the third attempt confirms it.
    let start = Instant::now();
    while h.state.store.retry_len().unwrap() > 0 {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    tokio::time::sleep(Duration::from_millis(300)).await;
    let refresh: Vec<JobRecord> = h
        .state
        .store
        .job_list()
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == JobKind::RefreshNegatives)
        .collect();
    assert_eq!(refresh.len(), 3);
    assert!(refresh.iter().all(|j| j.state == JobState::Completed));
    jobs.shutdown().await;
}

#[tokio::test]
async fn job_routes_require_the_admin_token_when_set() {
    let h = harness(|c| c.admin.token = Some("s3cret".into())).await;
    let (status, headers, _) = h.get("/api/v1/jobs").await;
    assert_eq!(status, 401);
    assert_eq!(headers["www-authenticate"], "Bearer");
    let (status, _, _) = send_json(&h, "/api/v1/jobs/refresh-negatives", &json!({})).await;
    assert_eq!(status, 401);
    let req = Request::get("/api/v1/jobs")
        .header("authorization", "Bearer s3cret")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.request(req).await.0, 200);
    let req = Request::get("/api/v1/jobs")
        .header("authorization", "Bearer wrong")
        .body(Body::empty())
        .unwrap();
    assert_eq!(h.request(req).await.0, 401);
}

#[tokio::test]
async fn batch_endpoint_no_match_is_queued_with_its_entry() {
    use ucgeocoder::{
        batch::{BatchTarget, fetch_chunk},
        census::batch_csv::AddressRow,
        upstream::{batch::BatchEndpoint, throttle::Priority},
    };
    let h = harness(|_| {}).await;
    mount(&h, &Census::new()).await;
    let catalog = h.state.catalog.load();
    let t = BatchTarget {
        bench: catalog.benchmark(4).unwrap().clone(),
        vintage: catalog.vintage(4, 4).unwrap().clone(),
        endpoint: BatchEndpoint::Address { geographies: true },
        upstream_vintage: true,
        class: catalog.class(4),
    };
    let row = AddressRow {
        idx: 1,
        user_id: "8".into(),
        street: "zzzz nowhere".into(),
        city: String::new(),
        state: String::new(),
        zip: String::new(),
    };
    let key = nowhere_key();
    // The batch endpoints' store step: the No_Match and its retry land together.
    let out = fetch_chunk(
        &h.state,
        &t,
        &[(&row, key.as_str())],
        Priority::Background,
        |_, _| EntrySource::Batch,
    )
    .await
    .unwrap();
    assert_eq!(out.len(), 1);
    assert!(h.state.store.get_entry(&key).unwrap().is_some());
    let due = h.state.store.retry_due(u64::MAX, 10).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].key, key);
}

#[tokio::test]
async fn overlapping_refresh_requests_are_refused() {
    let h = harness(|c| c.retry.negative_delay = Duration::ZERO).await;
    mount(&h, &Census::new()).await;
    upload(
        &h,
        "/geocoder/locations/addressbatch",
        "addressFile",
        NOWHERE_ROW,
        &[("benchmark", "4")],
    )
    .await;
    // No runner yet: the first refresh job stays queued (active).
    let (status, _, body) = send_json(&h, "/api/v1/jobs/refresh-negatives", &json!({})).await;
    assert_eq!(status, 201);
    assert_eq!(json(&body)["jobs"].as_array().unwrap().len(), 1);
    let (status, _, body) = send_json(&h, "/api/v1/jobs/refresh-negatives", &json!({})).await;
    assert_eq!(status, 409, "{}", String::from_utf8_lossy(&body));
    let refresh = h
        .state
        .store
        .job_list()
        .unwrap()
        .into_iter()
        .filter(|j| j.kind == JobKind::RefreshNegatives)
        .count();
    assert_eq!(refresh, 1);
}

#[tokio::test]
async fn unparseable_single_record_answer_retries_the_chunk() {
    let h = harness(|_| {}).await;
    h.mock_json(
        "/geographies/onelineaddress",
        fixture("geo_address_all.json"),
    )
    .await;
    Mock::given(method("GET"))
        .and(path("/geographies/onelineaddress"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("not json", "application/json"))
        .up_to_n_times(1)
        .with_priority(1)
        .mount(&h.server)
        .await;
    let (_, _, body) = send_json(
        &h,
        "/api/v1/jobs",
        &json!({"returntype": "locations", "benchmark": "4", "rows": [{"id": 1, "address": "1 A St"}]}),
    )
    .await;
    let id = id_of(&body);
    let jobs = worker::spawn(h.state.clone());
    let job = wait_for(&h, &id, finished).await;
    assert_eq!(job.state, JobState::Completed);
    assert_eq!((job.totals.matched, job.totals.failed), (1, 0));
    let chunks = h.state.store.job_chunks(&id).unwrap();
    assert_eq!(chunks[0].1.attempts, 1);
    jobs.shutdown().await;
}

async fn wait_chunk(h: &Harness, id: &str, state: ucgeocoder::jobs::ChunkStatus) {
    let start = Instant::now();
    while !h
        .state
        .store
        .job_chunks(id)
        .unwrap()
        .iter()
        .any(|(_, c)| c.state == state)
    {
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "chunk never {state:?}"
        );
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

async fn refresh_id(h: &Harness) -> String {
    let (status, _, body) = send_json(h, "/api/v1/jobs/refresh-negatives", &json!({})).await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let jobs = json(&body)["jobs"].clone();
    assert_eq!(jobs.as_array().unwrap().len(), 1);
    jobs[0]["id"].as_str().unwrap().to_owned()
}

/// A batch No_Match (queued an hour out) plus a queue item due now for the same key.
async fn due_no_match(h: &Harness) {
    upload(
        h,
        "/geocoder/locations/addressbatch",
        "addressFile",
        NOWHERE_ROW,
        &[("benchmark", "4")],
    )
    .await;
    h.state
        .store
        .retry_enqueue(nowhere_key(), 0, 0)
        .await
        .unwrap();
}

#[tokio::test]
async fn a_stale_refresh_job_does_not_recheck_a_key_its_cancelled_twin_handled() {
    let h = harness(|_| {}).await;
    let mut census = Census::new();
    census.fast = 1; // the refresh upload takes a while
    census.stall = Duration::from_millis(400);
    mount(&h, &census).await;
    due_no_match(&h).await;
    let jobs = worker::spawn(h.state.clone());

    let a = refresh_id(&h).await;
    wait_chunk(&h, &a, ucgeocoder::jobs::ChunkStatus::InFlight).await;
    // Cancelling makes A inactive while its upload is still running, so B is allowed
    // and captures the same key with the same (stale) attempt count.
    let (status, _, _) = post_empty(&h, &format!("/api/v1/jobs/{a}/cancel")).await;
    assert_eq!(status, 200);
    let b = refresh_id(&h).await;
    let job_b = wait_for(&h, &b, finished).await;
    let start = Instant::now();
    while !h
        .state
        .store
        .job_chunks(&a)
        .unwrap()
        .iter()
        .all(|(_, c)| c.state == ucgeocoder::jobs::ChunkStatus::Done)
    {
        assert!(start.elapsed() < Duration::from_secs(10));
        tokio::time::sleep(Duration::from_millis(5)).await;
    }

    // A's re-check counted once and re-queued the key an hour out; B did not upload.
    assert_eq!(uploads(&h).await.len(), 2);
    let due = h.state.store.retry_due(u64::MAX, 10).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].attempts, 1);
    assert!(due[0].due > 1000);
    assert_eq!((job_b.totals.cached, job_b.totals.no_match), (1, 1));
    jobs.shutdown().await;
}

#[tokio::test]
async fn an_interrupted_refresh_leaves_no_partial_progress() {
    let h = harness(|_| {}).await;
    let mut census = Census::new();
    census.fast = 1; // the refresh upload hangs
    mount(&h, &census).await;
    due_no_match(&h).await;
    let jobs = worker::spawn(h.state.clone());
    let id = refresh_id(&h).await;
    wait_chunk(&h, &id, ucgeocoder::jobs::ChunkStatus::InFlight).await;
    jobs.shutdown().await;
    let Harness {
        server,
        state,
        app,
        dir,
    } = h;
    drop(app);
    drop(state);

    let h = harness_with(server, dir, |_| {});
    h.server.reset().await;
    mount(&h, &Census::new()).await;
    // Nothing of the interrupted re-check was written: the entry is still the batch
    // No_Match and both queue items are still there with no attempt counted.
    let bytes = h.state.store.get_entry(&nowhere_key()).unwrap().unwrap();
    assert_eq!(
        decode(&bytes, h.state.codec).unwrap().entry.source,
        EntrySource::Batch
    );
    assert_eq!(h.state.store.retry_len().unwrap(), 2);

    let jobs = worker::spawn(h.state.clone());
    let job = wait_for(&h, &id, finished).await;
    assert_eq!(job.state, JobState::Completed);
    // Re-run once after the restart: exactly one attempt counted.
    assert_eq!(uploads(&h).await.len(), 1);
    let due = h.state.store.retry_due(u64::MAX, 10).unwrap();
    assert_eq!(due.len(), 1);
    assert_eq!(due[0].attempts, 1);
    let bytes = h.state.store.get_entry(&nowhere_key()).unwrap().unwrap();
    assert_eq!(
        decode(&bytes, h.state.codec).unwrap().entry.source,
        EntrySource::Job
    );
    jobs.shutdown().await;
}
