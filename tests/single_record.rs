//! Single-record endpoints against a mock upstream serving the live fixtures.

mod common;

use std::time::Duration;

use common::{fixture, harness, json, x_cache};
use futures_util::future::join_all;
use serde_json::value::RawValue;
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{method, path, query_param},
};

const ADDR_GEO: &str = "/geocoder/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&vintage=4&format=json";
const COORDS: &str =
    "/geocoder/geographies/coordinates?x=-76.92744&y=38.84601&benchmark=4&vintage=4&format=json";

/// Raw bytes of `result.<field>` (or of `result.addressMatches`) in a response body.
fn raw_field(body: &[u8], field: &str) -> String {
    #[derive(serde::Deserialize)]
    struct Outer<'a> {
        #[serde(borrow)]
        result: std::collections::HashMap<String, &'a RawValue>,
    }
    let o: Outer<'_> = serde_json::from_slice(body).unwrap();
    o.result[field].get().to_owned()
}

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|w| w == needle.as_bytes())
}

#[tokio::test]
async fn golden_geographies_address_all_layers_then_hit() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;

    let (status, headers, body) = h.get(&format!("{ADDR_GEO}&layers=all")).await;
    assert_eq!(status, 200);
    assert_eq!(x_cache(&headers), "MISS");
    assert_eq!(headers["content-type"], "application/json");
    let expected = fixture("geo_address_all.json");
    assert_eq!(
        json(&body),
        json(&expected),
        "structurally equal to upstream"
    );
    assert!(
        contains(&expected, &raw_field(&body, "addressMatches")),
        "passthrough segment byte-exact"
    );

    let (_, headers, body2) = h.get(&format!("{ADDR_GEO}&layers=all")).await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(body2, body);
    assert_eq!(h.hits().await, 1);

    // Upstream was asked for every layer.
    let req = &h.server.received_requests().await.unwrap()[0];
    assert!(req.url.query().unwrap().contains("layers=all"));
}

#[tokio::test]
async fn golden_default_layers_are_filtered_from_the_stored_entry() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;
    let (_, _, body) = h.get(ADDR_GEO).await;
    let expected = fixture("geo_address_default.json");
    assert_eq!(json(&body), json(&expected));
    let geo = &json(&body)["result"]["addressMatches"][0]["geographies"];
    let all = fixture("geo_address_all.json");
    for (_, layer) in geo.as_object().unwrap() {
        let raw = serde_json::to_string(layer).unwrap();
        assert!(contains(&all, &raw), "layer bytes preserved");
    }
}

#[tokio::test]
async fn golden_coordinates_default_and_all() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/coordinates", fixture("geo_coords_all.json"))
        .await;
    let (_, headers, body) = h.get(&format!("{COORDS}&layers=all")).await;
    assert_eq!(x_cache(&headers), "MISS");
    let expected = fixture("geo_coords_all.json");
    assert_eq!(json(&body), json(&expected));
    assert!(contains(&expected, &raw_field(&body, "geographies")));

    let (_, headers, body) = h.get(COORDS).await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(json(&body), json(&fixture("geo_coords_default.json")));
    assert_eq!(h.hits().await, 1);
}

#[tokio::test]
async fn locations_are_projected_from_a_geographies_entry() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;
    h.get(ADDR_GEO).await;
    let (_, headers, body) = h
        .get("/geocoder/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&format=json")
        .await;
    assert_eq!(
        x_cache(&headers),
        "HIT",
        "locations shares the geographies entry"
    );
    assert_eq!(h.hits().await, 1);
    let v = json(&body);
    assert!(v["result"]["input"].get("vintage").is_none());
    assert!(
        v["result"]["addressMatches"][0]
            .get("geographies")
            .is_none()
    );

    // The full locations response with an alias benchmark is byte-identical to upstream.
    let (_, _, body) = h
        .get("/geocoder/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=Public_AR_Current&format=json")
        .await;
    assert_eq!(body.as_ref(), fixture("loc_address.json").trim_ascii_end());
}

#[tokio::test]
async fn jsonp_matches_upstream() {
    let h = harness(|_| {}).await;
    h.mock_json(
        "/geographies/onelineaddress",
        fixture("geo_address_all.json"),
    )
    .await;
    let (status, headers, body) = h
        .get("/geocoder/locations/onelineaddress?address=4600+Silver+Hill+Rd%2C+Washington%2C+DC+20233&benchmark=4&format=jsonp&callback=test")
        .await;
    assert_eq!(status, 200);
    assert_eq!(headers["content-type"], "application/javascript");
    assert_eq!(body.as_ref(), fixture("loc_jsonp.js").trim_ascii_end());
}

#[tokio::test]
async fn concurrent_identical_misses_make_one_upstream_call() {
    let h = harness(|_| {}).await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("geo_address_all.json"), "application/json")
                .set_delay(Duration::from_millis(200)),
        )
        .mount(&h.server)
        .await;
    let results = join_all((0..16).map(|_| h.get(ADDR_GEO))).await;
    assert!(results.iter().all(|(s, _, _)| *s == 200));
    assert!(results.iter().all(|(_, h, _)| x_cache(h) == "MISS"));
    assert_eq!(h.hits().await, 1);
}

#[tokio::test]
async fn nomatch_is_cached_for_geographies_requests() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/onelineaddress", fixture("loc_nomatch.json"))
        .await;
    let q = "/geocoder/geographies/onelineaddress?address=zzzz+nowhere&benchmark=4&vintage=4&format=json";
    let (_, headers, body) = h.get(q).await;
    assert_eq!(x_cache(&headers), "MISS");
    assert_eq!(
        json(&body)["result"]["addressMatches"],
        serde_json::json!([])
    );
    let (_, headers, _) = h.get(q).await;
    assert_eq!(x_cache(&headers), "HIT");
    let (_, headers, body) = h
        .get("/geocoder/locations/onelineaddress?address=zzzz+nowhere&benchmark=4&format=json")
        .await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(body.as_ref(), fixture("loc_nomatch.json").trim_ascii_end());
    assert_eq!(h.hits().await, 1);
}

#[tokio::test]
async fn stale_served_when_upstream_fails() {
    let h = harness(|c| {
        c.ttl.match_current = Duration::from_secs(1);
        c.throttle.max_retries = 0;
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("geo_address_all.json"), "application/json"),
        )
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let (_, headers, fresh) = h.get(ADDR_GEO).await;
    assert_eq!(x_cache(&headers), "MISS");
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (status, headers, stale) = h.get(ADDR_GEO).await;
    assert_eq!(status, 200);
    assert_eq!(x_cache(&headers), "STALE");
    assert_eq!(stale, fresh);
}

#[tokio::test]
async fn expired_without_stale_fallback_is_502() {
    let h = harness(|c| {
        c.ttl.match_current = Duration::from_secs(1);
        c.throttle.max_retries = 0;
        c.cache.serve_stale_on_error = false;
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("geo_address_all.json"), "application/json"),
        )
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    h.get(ADDR_GEO).await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (status, _, body) = h.get(ADDR_GEO).await;
    assert_eq!(status, 502);
    assert_eq!(json(&body)["status"], "502");
}

#[tokio::test]
async fn open_breaker_returns_503_with_retry_after() {
    let h = harness(|c| {
        c.throttle.max_retries = 0;
        c.throttle.breaker_threshold = 1;
    })
    .await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let (status, _, _) = h.get(ADDR_GEO).await;
    assert_eq!(status, 502);
    let (status, headers, _) = h.get(ADDR_GEO).await;
    assert_eq!(status, 503);
    assert!(headers.contains_key("retry-after"));
    assert_eq!(h.hits().await, 1);
}

#[tokio::test]
async fn census_errors_match_upstream_bytes() {
    let h = harness(|_| {}).await;
    let (status, headers, body) = h
        .get("/geocoder/locations/onelineaddress?address=4600+Silver+Hill+Rd&format=json")
        .await;
    assert_eq!(status, 400);
    assert_eq!(headers["content-type"], "application/json");
    assert_eq!(
        body.as_ref(),
        fixture("err_nobenchmark.json").trim_ascii_end()
    );

    let (status, _, body) = h
        .get("/geocoder/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&format=json")
        .await;
    assert_eq!(status, 400);
    assert_eq!(
        body.as_ref(),
        fixture("err_novintage.json").trim_ascii_end()
    );

    let (status, _, _) = h
        .get("/geocoder/nope/address?benchmark=4&format=json")
        .await;
    assert_eq!(status, 404);
    assert_eq!(h.hits().await, 0);
}

#[tokio::test]
async fn unkeyable_requests_pass_through_verbatim() {
    let h = harness(|_| {}).await;
    let err = r#"{"errors":["Invalid benchmark in request"],"status":"400"}"#;
    Mock::given(method("GET"))
        .and(path("/locations/onelineaddress"))
        .and(query_param("benchmark", "Bogus"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(err, "application/json"))
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/locations/onelineaddress"))
        .and(query_param("benchmark", "4"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw("<html>ok</html>", "text/html;charset=UTF-8"),
        )
        .mount(&h.server)
        .await;

    let (status, headers, body) = h
        .get("/geocoder/locations/onelineaddress?address=x&benchmark=Bogus&format=json")
        .await;
    assert_eq!((status.as_u16(), x_cache(&headers)), (400, "BYPASS"));
    assert_eq!(body.as_ref(), err.as_bytes());

    // No `format`: Census renders HTML; we forward it rather than inventing JSON.
    let (status, headers, body) = h
        .get("/geocoder/locations/onelineaddress?address=x&benchmark=4")
        .await;
    assert_eq!((status.as_u16(), x_cache(&headers)), (200, "BYPASS"));
    assert_eq!(headers["content-type"], "text/html;charset=UTF-8");
    assert_eq!(body.as_ref(), b"<html>ok</html>");
}

#[tokio::test]
async fn upstream_rejections_are_forwarded_and_not_cached() {
    // LUCA (11) is listed by Census but rejected for geographies.
    let h = harness(|_| {}).await;
    let err = r#"{"errors":["Invalid benchmark in request"],"status":"400"}"#;
    Mock::given(method("GET"))
        .and(path("/geographies/onelineaddress"))
        .respond_with(ResponseTemplate::new(400).set_body_raw(err, "application/json"))
        .mount(&h.server)
        .await;
    let q = "/geocoder/locations/onelineaddress?address=1+Main+St&benchmark=11&format=json";
    for _ in 0..2 {
        let (status, _, body) = h.get(q).await;
        assert_eq!(status, 400);
        assert_eq!(body.as_ref(), err.as_bytes());
    }
    assert_eq!(h.hits().await, 2);
}

#[tokio::test]
async fn catalog_endpoints_cache_with_ttl_and_echo_alias() {
    let h = harness(|c| c.ttl.catalog = Duration::from_secs(1)).await;
    h.mock_json("/benchmarks", fixture("benchmarks.json")).await;
    h.mock_json("/vintages", fixture("vintages_4.json")).await;

    let (_, headers, body) = h.get("/geocoder/benchmarks").await;
    assert_eq!(x_cache(&headers), "MISS");
    assert_eq!(body.as_ref(), fixture("benchmarks.json").trim_ascii_end());
    let (_, headers, _) = h.get("/geocoder/benchmarks").await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(h.hits().await, 1);

    let (_, _, body) = h
        .get("/geocoder/vintages?benchmark=Public_AR_Current")
        .await;
    assert_eq!(json(&body)["selectedBenchmark"], "Public_AR_Current");
    let (_, headers, body) = h.get("/geocoder/vintages?benchmark=4").await;
    assert_eq!(
        x_cache(&headers),
        "HIT",
        "aliases share one stored document"
    );
    assert_eq!(json(&body)["selectedBenchmark"], "4");
    assert_eq!(h.hits().await, 2);

    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, headers, _) = h.get("/geocoder/benchmarks").await;
    assert_eq!(x_cache(&headers), "MISS", "refetched after ttl.catalog");
    assert_eq!(h.hits().await, 3);
}

#[tokio::test]
async fn catalog_falls_back_to_snapshot_when_upstream_is_down() {
    let h = harness(|c| c.throttle.max_retries = 0).await;
    Mock::given(method("GET"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&h.server)
        .await;
    let (status, headers, body) = h.get("/geocoder/benchmarks").await;
    assert_eq!((status.as_u16(), x_cache(&headers)), (200, "STALE"));
    assert_eq!(
        json(&body)["benchmarks"][0]["benchmarkName"],
        "Public_AR_Current"
    );
}

#[tokio::test]
async fn flat_extension_via_header() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/coordinates", fixture("geo_coords_all.json"))
        .await;
    let req = axum::http::Request::get(format!("{COORDS}&layers=all"))
        .header("x-ucg-flatten", "1")
        .body(axum::body::Body::empty())
        .unwrap();
    let (_, _, body) = h.request(req).await;
    let v = json(&body);
    assert_eq!(v["result"]["flat"]["States"]["STUSAB"], "MD");
    assert_eq!(v["result"]["flat"]["Counties"]["GEOID"], "24033");
}

#[tokio::test]
async fn slow_locations_fetch_does_not_clobber_geographies_entry() {
    let h = harness(|c| c.cache.fetch_geographies_always = false).await;
    let mut loc =
        serde_json::from_slice::<serde_json::Value>(&fixture("loc_address.json")).unwrap();
    loc["result"]["input"]["address"] = serde_json::json!({"street": "4600 Silver Hill Rd"});
    Mock::given(method("GET"))
        .and(path("/locations/address"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(serde_json::to_vec(&loc).unwrap(), "application/json")
                .set_delay(Duration::from_millis(300)),
        )
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/geographies/address"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(fixture("geo_address_all.json"), "application/json")
                .set_delay(Duration::from_millis(50)),
        )
        .mount(&h.server)
        .await;
    let loc_q = "/geocoder/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&format=json";
    let (a, b) = tokio::join!(h.get(loc_q), async {
        tokio::time::sleep(Duration::from_millis(20)).await;
        h.get(ADDR_GEO).await
    });
    assert_eq!((a.0.as_u16(), b.0.as_u16()), (200, 200));
    let (_, headers, _) = h.get(ADDR_GEO).await;
    assert_eq!(
        x_cache(&headers),
        "HIT",
        "geographies entry survived the slower locations write"
    );
    assert_eq!(h.hits().await, 2);
}

#[tokio::test]
async fn malformed_catalog_200_does_not_replace_valid_document() {
    let h = harness(|c| c.ttl.catalog = Duration::from_secs(1)).await;
    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(fixture("benchmarks.json"), "application/json"),
        )
        .up_to_n_times(1)
        .mount(&h.server)
        .await;
    Mock::given(method("GET"))
        .and(path("/benchmarks"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("null", "application/json"))
        .mount(&h.server)
        .await;
    h.get("/geocoder/benchmarks").await;
    tokio::time::sleep(Duration::from_millis(1100)).await;
    let (_, headers, body) = h.get("/geocoder/benchmarks").await;
    assert_eq!(x_cache(&headers), "STALE");
    assert_eq!(body.as_ref(), fixture("benchmarks.json").trim_ascii_end());
    let (raw, _) = h.state.store.catalog_get("benchmarks").unwrap().unwrap();
    assert_eq!(
        raw,
        fixture("benchmarks.json"),
        "valid document kept in the store"
    );
}

#[tokio::test]
async fn concurrent_vintage_refreshes_both_land_in_the_catalog() {
    let h = harness(|_| {}).await;
    let mut v8: serde_json::Value = serde_json::from_slice(&fixture("vintages_4.json")).unwrap();
    v8["vintages"] = serde_json::json!([{"isDefault": true, "id": "8", "vintageName": "Test_Eight", "vintageDescription": "x"}]);
    for (bench, body, delay) in [
        ("4", fixture("vintages_4.json"), 150),
        ("8", serde_json::to_vec(&v8).unwrap(), 10),
    ] {
        Mock::given(method("GET"))
            .and(path("/vintages"))
            .and(query_param("benchmark", bench))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_raw(body, "application/json")
                    .set_delay(Duration::from_millis(delay)),
            )
            .mount(&h.server)
            .await;
    }
    tokio::join!(
        h.get("/geocoder/vintages?benchmark=4"),
        h.get("/geocoder/vintages?benchmark=8")
    );
    let c = h.state.catalog.load();
    assert_eq!(c.resolve_vintage(8, "Test_Eight").map(|v| v.id), Some(8));
    assert!(c.resolve_vintage(4, "ACS2026_Current").is_some());
}
