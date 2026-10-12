//! Admin API (PLAN.md §8): OpenAPI document, auth, cache maintenance, runtime config,
//! upstream state and metrics.

mod common;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
};
use common::{Harness, harness, json};

const SNAPSHOT: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/snapshots/openapi.json");

async fn get_auth(h: &Harness, uri: &str, token: Option<&str>) -> (StatusCode, HeaderMap, Bytes) {
    let mut req = Request::get(uri);
    if let Some(t) = token {
        req = req.header("authorization", format!("Bearer {t}"));
    }
    h.request(req.body(Body::empty()).unwrap()).await
}

/// `UCG_UPDATE_SNAPSHOTS=1 cargo test --test admin_api` rewrites the snapshot.
#[tokio::test]
async fn openapi_document_matches_snapshot() {
    let h = harness(|c| c.admin.token = Some("t".into())).await;
    // Public even when a token is configured.
    let (status, headers, body) = get_auth(&h, "/api-docs/openapi.json", None).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/json");
    let doc = json(&body);
    assert_eq!(doc["info"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(
        doc["components"]["securitySchemes"]["bearerAuth"]["scheme"],
        "bearer"
    );
    if std::env::var_os("UCG_UPDATE_SNAPSHOTS").is_some() {
        std::fs::create_dir_all(std::path::Path::new(SNAPSHOT).parent().unwrap()).unwrap();
        std::fs::write(SNAPSHOT, &body).unwrap();
    }
    let expected = std::fs::read_to_string(SNAPSHOT)
        .expect("snapshot missing: run with UCG_UPDATE_SNAPSHOTS=1");
    assert_eq!(
        String::from_utf8_lossy(&body),
        expected,
        "OpenAPI document changed; review and rerun with UCG_UPDATE_SNAPSHOTS=1"
    );
}

use std::time::{Duration, SystemTime};

use serde_json::{Value, json};
use ucgeocoder::cache::{
    codec::Codec,
    envelope::{BatchView, Entry, EntryPayload, EntrySource, Status, Views, encode_stored},
    views::decode,
};

const TOKEN: &str = "s3cret";

async fn send(
    h: &Harness,
    method: &str,
    uri: &str,
    body: Option<Value>,
) -> (StatusCode, HeaderMap, Bytes) {
    let mut req = Request::builder()
        .method(method)
        .uri(uri)
        .header("authorization", format!("Bearer {TOKEN}"));
    let body = match body {
        Some(v) => {
            req = req.header("content-type", "application/json");
            Body::from(v.to_string())
        }
        None => Body::empty(),
    };
    h.request(req.body(body).unwrap()).await
}

async fn admin(tweak: impl FnOnce(&mut ucgeocoder::config::Config)) -> Harness {
    harness(|c| {
        c.admin.token = Some(TOKEN.into());
        tweak(c);
    })
    .await
}

fn pct(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

const DAY: Duration = Duration::from_secs(86_400);

/// Six entries across benchmarks, vintages, statuses and ages.
fn seed_entries() -> Vec<(&'static str, Status, EntrySource, Duration)> {
    vec![
        (
            "v1|address|4|4|1/2 MAIN ST;A;DC;",
            Status::Match,
            EntrySource::Single,
            Duration::ZERO,
        ),
        (
            "v1|address|4|4|B;;;",
            Status::NoMatch,
            EntrySource::Batch,
            2 * DAY,
        ),
        (
            "v1|coordinates|4|4|1.000000,2.000000",
            Status::Tie,
            EntrySource::Batch,
            Duration::ZERO,
        ),
        (
            "v1|address|8|8|C;;;",
            Status::Match,
            EntrySource::Batch,
            Duration::ZERO,
        ),
        (
            "v1|oneline|2020|2020|D",
            Status::Match,
            EntrySource::Single,
            400 * DAY,
        ),
        (
            "v1|address|4|410|E;;;",
            Status::Match,
            EntrySource::Single,
            200 * DAY,
        ),
    ]
}

async fn seed(h: &Harness) {
    let now = SystemTime::now();
    let items = seed_entries()
        .into_iter()
        .map(|(k, status, source, age)| {
            let e = Entry {
                status,
                source,
                views: if source == EntrySource::Single {
                    Views::JSON
                } else {
                    Views::BATCH
                },
                verified_at: 5,
                payload: EntryPayload {
                    json: (source == EntrySource::Single).then(|| {
                        serde_bytes::ByteBuf::from(br#"[{"x":-76.92836638093}]"#.to_vec())
                    }),
                    batch: (source == EntrySource::Batch).then(BatchView::default),
                    benchmark_name: "Public_AR_Current".into(),
                    vintage_name: "Current_Current".into(),
                },
            };
            (
                k.to_owned(),
                encode_stored(&e.encode(Codec::embedded()).unwrap(), now - age),
            )
        })
        .collect();
    h.state.store.put_entries(items).await.unwrap();
}

async fn keys(h: &Harness) -> Vec<String> {
    h.state
        .store
        .scan_entries("", None, 100)
        .unwrap()
        .into_iter()
        .map(|(k, _)| k)
        .collect()
}

#[tokio::test]
async fn admin_routes_require_the_token_and_public_routes_do_not() {
    let h = admin(|_| {}).await;
    for uri in [
        "/api/v1/cache/stats",
        "/api/v1/config",
        "/api/v1/upstream",
        "/api/v1/catalog",
        "/api/v1/jobs",
        "/metrics",
    ] {
        let (status, headers, _) = get_auth(&h, uri, None).await;
        assert_eq!(status, 401, "{uri}");
        assert_eq!(headers["www-authenticate"], "Bearer", "{uri}");
        assert_eq!(get_auth(&h, uri, Some("wrong")).await.0, 401, "{uri}");
        assert_eq!(get_auth(&h, uri, Some(TOKEN)).await.0, 200, "{uri}");
    }
    for uri in ["/healthz", "/api-docs/openapi.json"] {
        assert_eq!(get_auth(&h, uri, None).await.0, 200, "{uri}");
    }
    // Unknown /api/v1 routes are 404, not 401 (the guard is a route layer).
    assert_eq!(get_auth(&h, "/api/v1/nope", None).await.0, 404);

    let open = admin(|c| c.admin.metrics_public = true).await;
    assert_eq!(get_auth(&open, "/metrics", None).await.0, 200);
    assert_eq!(get_auth(&open, "/api/v1/config", None).await.0, 401);
}

#[tokio::test]
async fn every_purge_scope_with_dry_run() {
    let h = admin(|_| {}).await;
    let cases: &[(Value, &[usize])] = &[
        (json!({"scope": "all"}), &[0, 1, 2, 3, 4, 5]),
        // Batch NoMatch past 24 h and a Current-class Match past 180 d; the 400-day-old
        // Census2020 (fixed-class) Match never expires.
        (json!({"scope": "expired"}), &[1, 5]),
        (json!({"scope": "benchmark", "value": "4"}), &[0, 1, 2, 5]),
        (
            json!({"scope": "benchmark", "value": "Public_AR_Current"}),
            &[0, 1, 2, 5],
        ),
        (json!({"scope": "vintage", "value": "4:4"}), &[0, 1, 2]),
        (
            json!({"scope": "vintage", "value": "4:Census2010_Current"}),
            &[5],
        ),
        (json!({"scope": "status", "value": "NoMatch"}), &[1]),
        (json!({"scope": "status", "value": "match"}), &[0, 3, 4, 5]),
        (
            json!({"scope": "prefix", "value": "v1|address|"}),
            &[0, 1, 3, 5],
        ),
    ];
    let all: Vec<&str> = seed_entries().iter().map(|e| e.0).collect();
    for (req, expected) in cases {
        send(
            &h,
            "POST",
            "/api/v1/cache/purge",
            Some(json!({"scope": "all"})),
        )
        .await;
        seed(&h).await;
        let mut dry = req.clone();
        dry["dry_run"] = json!(true);
        let (status, _, body) = send(&h, "POST", "/api/v1/cache/purge", Some(dry)).await;
        assert_eq!(status, 200, "{req}: {}", String::from_utf8_lossy(&body));
        assert_eq!(json(&body)["matched"], expected.len(), "dry run {req}");
        assert_eq!(json(&body)["dry_run"], true);
        assert_eq!(keys(&h).await.len(), 6, "dry run deleted something: {req}");

        let (_, _, body) = send(&h, "POST", "/api/v1/cache/purge", Some(req.clone())).await;
        assert_eq!(json(&body)["matched"], expected.len(), "{req}");
        let mut left = keys(&h).await;
        left.sort();
        let mut want: Vec<String> = all
            .iter()
            .enumerate()
            .filter(|(i, _)| !expected.contains(i))
            .map(|(_, k)| (*k).to_owned())
            .collect();
        want.sort();
        assert_eq!(left, want, "{req}");
    }
    for bad in [
        json!({"scope": "nope"}),
        json!({"scope": "benchmark"}),
        json!({"scope": "benchmark", "value": "999"}),
        json!({"scope": "vintage", "value": "4"}),
        json!({"scope": "status", "value": "maybe"}),
    ] {
        assert_eq!(
            send(&h, "POST", "/api/v1/cache/purge", Some(bad.clone()))
                .await
                .0,
            400,
            "{bad}"
        );
    }
}

#[tokio::test]
async fn export_import_round_trip() {
    let src = admin(|_| {}).await;
    seed(&src).await;
    let (status, headers, body) = send(&src, "GET", "/api/v1/cache/export", None).await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/x-ndjson");
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert_eq!(text.lines().count(), 6);
    // The upstream JSON segment is carried verbatim.
    assert!(text.contains(r#""json":[{"x":-76.92836638093}]"#), "{text}");
    let (_, _, part) = send(
        &src,
        "GET",
        "/api/v1/cache/export?prefix=v1%7Caddress%7C8%7C",
        None,
    )
    .await;
    assert_eq!(String::from_utf8_lossy(&part).lines().count(), 1);

    let dst = admin(|_| {}).await;
    let mut upload = text.clone();
    upload.push_str("{not json}\n");
    let req = Request::post("/api/v1/cache/import")
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("content-type", "application/x-ndjson")
        .body(Body::from(upload))
        .unwrap();
    let (status, _, body) = dst.request(req).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let resp = json(&body);
    assert_eq!(
        (resp["records"].clone(), resp["invalid"].clone()),
        (json!(6), json!(1))
    );
    assert_eq!(resp["errors"][0]["line"], 7);

    for (k, _, _, _) in seed_entries() {
        let a = decode(
            &src.state.store.get_entry(k).unwrap().unwrap(),
            src.state.codec,
        )
        .unwrap();
        let b = decode(
            &dst.state.store.get_entry(k).unwrap().unwrap(),
            dst.state.codec,
        )
        .unwrap();
        assert_eq!(a.entry, b.entry, "{k}");
        assert_eq!(a.created_at, b.created_at, "{k}");
    }

    // Importing an older copy never rolls a newer entry back.
    let key = "v1|address|8|8|C;;;";
    let older = text
        .lines()
        .find(|l| l.contains("8|8|C"))
        .unwrap()
        .to_owned();
    send(
        &dst,
        "DELETE",
        &format!("/api/v1/cache/entries/{}", pct(key)),
        None,
    )
    .await;
    seed(&dst).await; // fresh copy, created now
    let before = dst.state.store.get_entry(key).unwrap();
    let req = Request::post("/api/v1/cache/import")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from(older))
        .unwrap();
    assert_eq!(dst.request(req).await.0, 200);
    assert_eq!(dst.state.store.get_entry(key).unwrap(), before);
}

#[tokio::test]
async fn csv_import_creates_a_warm_job() {
    let h = admin(|_| {}).await;
    let req = Request::post("/api/v1/cache/import?format=csv&returntype=locations&benchmark=4")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from("1,4600 Silver Hill Rd,Washington,DC,20233\n"))
        .unwrap();
    let (status, _, body) = h.request(req).await;
    assert_eq!(status, 201, "{}", String::from_utf8_lossy(&body));
    let resp = json(&body);
    assert_eq!(resp["job"]["kind"], "warm");
    assert_eq!(resp["records"], 1);
    let req = Request::post("/api/v1/cache/import?format=csv&benchmark=4")
        .header("authorization", format!("Bearer {TOKEN}"))
        .body(Body::from("1,x\n"))
        .unwrap();
    assert_eq!(h.request(req).await.0, 400);
}

#[tokio::test]
async fn entries_listing_inspection_and_delete() {
    let h = admin(|_| {}).await;
    seed(&h).await;
    let (_, _, body) = send(&h, "GET", "/api/v1/cache/entries?limit=4", None).await;
    let page = json(&body);
    assert_eq!(page["entries"].as_array().unwrap().len(), 4);
    let cursor = page["next_cursor"].as_str().unwrap().to_owned();
    let (_, _, body) = send(
        &h,
        "GET",
        &format!("/api/v1/cache/entries?limit=4&cursor={}", pct(&cursor)),
        None,
    )
    .await;
    let page = json(&body);
    assert_eq!(page["entries"].as_array().unwrap().len(), 2);
    assert!(page["next_cursor"].is_null());

    // A sparse filter: one NoMatch among six, one entry examined per... page of 1.
    let (_, _, body) = send(&h, "GET", "/api/v1/cache/entries?status=NoMatch", None).await;
    let page = json(&body);
    assert_eq!(page["entries"].as_array().unwrap().len(), 1);
    assert_eq!(page["entries"][0]["status"], "NoMatch");
    assert_eq!(page["entries"][0]["fresh"], false);
    let (_, _, body) = send(
        &h,
        "GET",
        "/api/v1/cache/entries?searchtype=address&benchmark=Public_AR_Current",
        None,
    )
    .await;
    assert_eq!(json(&body)["entries"].as_array().unwrap().len(), 3);

    // Keys contain `/`, `|`, `;` and spaces: percent-encoded in the path.
    let key = "v1|address|4|4|1/2 MAIN ST;A;DC;";
    let uri = format!("/api/v1/cache/entries/{}", pct(key));
    let (status, _, body) = send(&h, "GET", &uri, None).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let e = json(&body);
    assert_eq!(e["key"], key);
    assert_eq!(e["views"], json!(["json"]));
    assert_eq!(e["json"], json!([{"x": -76.92836638093}]));
    assert_eq!(send(&h, "DELETE", &uri, None).await.0, 204);
    assert_eq!(send(&h, "DELETE", &uri, None).await.0, 404);
    assert_eq!(send(&h, "GET", &uri, None).await.0, 404);
}

#[tokio::test]
async fn stats_and_compact() {
    let h = admin(|_| {}).await;
    seed(&h).await;
    let (status, _, body) = send(&h, "GET", "/api/v1/cache/stats", None).await;
    assert_eq!(status, 200);
    let s = json(&body);
    assert_eq!(s["entries"], 6);
    assert!(s["detail"].is_null());
    assert!(s["entries_table"]["stored_bytes"].as_u64().unwrap() > 0);
    let (_, _, body) = send(&h, "GET", "/api/v1/cache/stats?detail=full", None).await;
    let d = &json(&body)["detail"];
    assert_eq!(d["by_status"], json!({"Match": 4, "NoMatch": 1, "Tie": 1}));
    assert_eq!(d["by_benchmark"], json!({"2020": 1, "4": 4, "8": 1}));
    assert_eq!(d["compression"]["sampled"], 6);

    send(
        &h,
        "POST",
        "/api/v1/cache/purge",
        Some(json!({"scope": "all"})),
    )
    .await;
    let (status, _, body) = send(&h, "POST", "/api/v1/cache/compact", None).await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    assert!(json(&body)["file_bytes_after"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn config_overrides_change_the_live_limiter_and_persist() {
    let h = admin(|_| {}).await;
    let (_, _, body) = send(&h, "GET", "/api/v1/upstream", None).await;
    assert_eq!(json(&body)["single"]["concurrency"], 4);
    assert_eq!(json(&body)["single"]["available"], 4);

    let (status, _, body) = send(
        &h,
        "PUT",
        "/api/v1/config",
        Some(json!({"throttle.single_concurrency": 1, "ttl.tie": "7d"})),
    )
    .await;
    assert_eq!(status, 200, "{}", String::from_utf8_lossy(&body));
    let field = |body: &[u8], path: &str| {
        json(body)["fields"]
            .as_array()
            .unwrap()
            .iter()
            .find(|f| f["path"] == path)
            .unwrap()
            .clone()
    };
    let f = field(&body, "throttle.single_concurrency");
    assert_eq!(
        (f["value"].clone(), f["source"].clone()),
        (json!("1"), json!("override"))
    );
    assert_eq!(field(&body, "admin.token")["value"], "***");
    assert_eq!(field(&body, "admin.token")["overridable"], false);
    // The limiter itself changed: one single-record permit left.
    let (_, _, body) = send(&h, "GET", "/api/v1/upstream", None).await;
    assert_eq!(json(&body)["single"]["available"], 1);
    assert_eq!(h.state.settings().config.ttl.tie, 7 * DAY);

    for bad in [
        json!({"server.bind": "0.0.0.0:1"}),
        json!({"nope.key": 1}),
        json!({"throttle.single_rps": 0}),
        json!({"throttle.single_rps": [1]}),
    ] {
        let (status, _, _) = send(&h, "PUT", "/api/v1/config", Some(bad.clone())).await;
        assert_eq!(status, 400, "{bad}");
    }
    // Rejected changes left the live settings alone.
    assert_eq!(h.state.settings().config.throttle.single_rps, 1000);

    // Overrides survive a restart.
    let common::Harness {
        server,
        state,
        app,
        dir,
    } = h;
    drop(app);
    drop(state);
    let h = common::harness_with(server, dir, |c| c.admin.token = Some(TOKEN.into()));
    let (_, _, body) = send(&h, "GET", "/api/v1/upstream", None).await;
    assert_eq!(json(&body)["single"]["available"], 1);

    let (status, _, body) = send(
        &h,
        "DELETE",
        "/api/v1/config/throttle.single_concurrency",
        None,
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(
        field(&body, "throttle.single_concurrency")["source"],
        "default"
    );
    let (_, _, body) = send(&h, "GET", "/api/v1/upstream", None).await;
    assert_eq!(json(&body)["single"]["available"], 4);
    assert_eq!(
        send(
            &h,
            "DELETE",
            "/api/v1/config/throttle.single_concurrency",
            None
        )
        .await
        .0,
        404
    );
}

#[tokio::test]
async fn metrics_catalog_and_upstream() {
    let h = admin(|_| {}).await;
    h.mock_json(
        "/geographies/onelineaddress",
        common::fixture("geo_address_all.json"),
    )
    .await;
    let uri =
        "/geocoder/locations/onelineaddress?address=4600+Silver+Hill+Rd&benchmark=4&format=json";
    assert_eq!(h.get(uri).await.0, 200);
    assert_eq!(h.get(uri).await.0, 200);
    let (status, headers, body) = send(&h, "GET", "/metrics", None).await;
    assert_eq!(status, 200);
    assert!(
        headers["content-type"]
            .to_str()
            .unwrap()
            .starts_with("text/plain")
    );
    let text = String::from_utf8(body.to_vec()).unwrap();
    for needle in [
        "ucg_requests_total{endpoint=\"single\",cache=\"MISS\"}",
        "ucg_requests_total{endpoint=\"single\",cache=\"HIT\"}",
        "ucg_breaker_open 0",
        "ucg_cache_entries",
    ] {
        assert!(text.contains(needle), "missing {needle} in:\n{text}");
    }

    let (_, _, body) = send(&h, "GET", "/api/v1/catalog", None).await;
    let c = json(&body);
    let b4 = c["benchmarks"]
        .as_array()
        .unwrap()
        .iter()
        .find(|b| b["id"] == 4)
        .unwrap()
        .clone();
    assert_eq!(b4["class"], "current");
    assert!(
        b4["vintages"]
            .as_array()
            .unwrap()
            .iter()
            .any(|v| v["name"] == "Current_Current")
    );

    // Open the breaker, see it, reset it.
    for _ in 0..20 {
        h.state.upstream.throttle.record_failure();
    }
    let (_, _, body) = send(&h, "GET", "/api/v1/upstream", None).await;
    assert_eq!(json(&body)["breaker"]["open"], true);
    let (_, _, text) = send(&h, "GET", "/metrics", None).await;
    assert!(String::from_utf8_lossy(&text).contains("ucg_breaker_open 1"));
    let (status, _, body) = send(&h, "POST", "/api/v1/upstream/breaker/reset", None).await;
    assert_eq!(status, 200);
    assert_eq!(json(&body)["breaker"]["open"], false);
}
