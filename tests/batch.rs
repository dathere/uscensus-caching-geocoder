//! Batch endpoints against a mock upstream serving the captured batch fixtures.

mod common;

use axum::{
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
};
use common::{Harness, fixture, harness, json, multipart, x_cache};
use wiremock::{
    Mock, ResponseTemplate,
    matchers::{body_string_contains, method, path},
};

async fn post(
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

async fn mock_post(h: &Harness, p: &str, body: Vec<u8>) {
    Mock::given(method("POST"))
        .and(path(p))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/plain"))
        .mount(&h.server)
        .await;
}

/// Data lines of each upstream upload, e.g. `["1,4600 Silver Hill Rd,..."]`.
async fn uploads(h: &Harness) -> Vec<Vec<String>> {
    h.server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.method.as_str() == "POST")
        .map(|r| {
            let body = String::from_utf8_lossy(&r.body).into_owned();
            body.lines()
                .filter(|l| l.contains(',') && l.chars().next().is_some_and(|c| c.is_ascii_digit()))
                .map(str::to_owned)
                .collect()
        })
        .collect()
}

const ADDR_LOC: &str = "/geocoder/locations/addressbatch";
const ADDR_GEO: &str = "/geocoder/geographies/addressbatch";
const COORDS: &str = "/geocoder/geographies/coordinatesbatch";

#[tokio::test]
async fn locations_batch_is_byte_identical_and_uses_geographies_upstream() {
    let h = harness(|_| {}).await;
    mock_post(&h, "/geographies/addressbatch", fixture("batch_geo.csv")).await;
    let (status, headers, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(status, 200);
    assert_eq!(body.as_ref(), fixture("batch_loc.csv"));
    assert_eq!(headers["content-type"], "text/plain");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=GeocodeResults.csv"
    );
    assert_eq!(headers["x-ucg-cache-misses"], "3");
    let up = uploads(&h).await;
    assert_eq!(up.len(), 1);
    assert_eq!(up[0].len(), 3);

    // Same file again: every row from cache; the No_Match row was queued for retry.
    let (_, headers, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(body.as_ref(), fixture("batch_loc.csv"));
    assert_eq!(uploads(&h).await.len(), 1);
    assert_eq!(h.state.store.retry_len().unwrap(), 1);
}

#[tokio::test]
async fn geographies_and_coordinates_batches_are_byte_identical() {
    let h = harness(|_| {}).await;
    mock_post(&h, "/geographies/addressbatch", fixture("batch_geo.csv")).await;
    mock_post(
        &h,
        "/geographies/coordinatesbatch",
        fixture("batch_coords.csv"),
    )
    .await;
    let (_, _, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert_eq!(body.as_ref(), fixture("batch_geo.csv"));
    let (_, _, body) = post(
        &h,
        COORDS,
        "coordinatesFile",
        &fixture("coords_in.csv"),
        &[("vintage", "4"), ("benchmark", "4")],
    )
    .await;
    assert_eq!(body.as_ref(), fixture("batch_coords.csv"));
}

#[tokio::test]
async fn locations_upstream_when_geographies_not_forced() {
    let h = harness(|c| c.cache.fetch_geographies_always = false).await;
    mock_post(&h, "/locations/addressbatch", fixture("batch_loc.csv")).await;
    let (_, _, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(body.as_ref(), fixture("batch_loc.csv"));
    let req = &h.server.received_requests().await.unwrap()[0];
    assert_eq!(req.url.path(), "/locations/addressbatch");
}

#[tokio::test]
async fn input_order_survives_shuffled_upstream_output() {
    let h = harness(|_| {}).await;
    let shuffled: Vec<u8> = String::from_utf8(fixture("batch_geo.csv"))
        .unwrap()
        .lines()
        .rev()
        .flat_map(|l| format!("{l}\n").into_bytes())
        .collect();
    mock_post(&h, "/geographies/addressbatch", shuffled).await;
    let (_, _, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert_eq!(body.as_ref(), fixture("batch_geo.csv"));
}

#[tokio::test]
async fn duplicate_ids_pass_through_and_duplicate_inputs_are_sent_once() {
    let h = harness(|_| {}).await;
    mock_post(&h, "/geographies/addressbatch", fixture("batch_geo.csv")).await;
    let input = b"a,4600 Silver Hill Rd,Washington,DC,20233\na,4600 SILVER HILL RD.,Washington,DC,20233\nb,1600 Pennsylvania Ave NW,Washington,DC,20500\n";
    let (_, headers, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        input,
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    let text = String::from_utf8(body.to_vec()).unwrap();
    let lines: Vec<&str> = text.lines().collect();
    assert_eq!(lines.len(), 3);
    assert!(lines[0].starts_with(r#""a","4600 Silver Hill Rd, Washington, DC, 20233","Match""#));
    assert!(lines[1].starts_with(r#""a","4600 SILVER HILL RD., Washington, DC, 20233","Match""#));
    assert!(lines[2].starts_with(r#""b","1600 Pennsylvania Ave NW"#));
    assert_eq!(
        uploads(&h).await[0].len(),
        2,
        "rows 1 and 2 share a cache key"
    );
    assert_eq!(headers["x-ucg-cache-misses"], "3");
}

#[tokio::test]
async fn tie_rows_are_reproduced_and_cached() {
    let h = harness(|_| {}).await;
    mock_post(
        &h,
        "/geographies/addressbatch",
        br#""1","1 Main St, X, Y, ","Tie""#.to_vec(),
    )
    .await;
    let fields = [("benchmark", "4"), ("vintage", "4")];
    let (_, _, body) = post(&h, ADDR_GEO, "addressFile", b"7,1 Main St,X,Y,\n", &fields).await;
    // Tie row shape is unverified upstream (PLAN.md §11.3); this asserts our assumption.
    assert_eq!(body.as_ref(), b"\"7\",\"1 Main St, X, Y, \",\"Tie\"\n");
    let (_, headers, body2) =
        post(&h, ADDR_GEO, "addressFile", b"7,1 Main St,X,Y,\n", &fields).await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(body2, body);
}

#[tokio::test]
async fn mixed_hits_and_misses_are_stitched() {
    let h = harness(|_| {}).await;
    mock_post(&h, "/geographies/addressbatch", fixture("batch_geo.csv")).await;
    let fields = [("benchmark", "4"), ("vintage", "4")];
    post(
        &h,
        ADDR_GEO,
        "addressFile",
        &fixture("batch_in.csv"),
        &fields,
    )
    .await;
    Mock::given(method("POST"))
        .and(path("/geographies/addressbatch"))
        .and(body_string_contains("Springfield"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#""4","1 Main St, Springfield, IL, 62701","No_Match""#,
            "text/plain",
        ))
        .with_priority(1)
        .mount(&h.server)
        .await;
    let mut input = fixture("batch_in.csv");
    input.extend_from_slice(b"4,1 Main St,Springfield,IL,62701\n");
    let (_, headers, body) = post(&h, ADDR_GEO, "addressFile", &input, &fields).await;
    assert_eq!(headers["x-ucg-cache-hits"], "3");
    assert_eq!(headers["x-ucg-cache-misses"], "1");
    let up = uploads(&h).await;
    assert_eq!(up[1], ["4,1 Main St,Springfield,IL,62701"]);
    let mut expected = fixture("batch_geo.csv");
    expected.extend_from_slice(b"\"4\",\"1 Main St, Springfield, IL, 62701\",\"No_Match\"\n");
    assert_eq!(body.as_ref(), expected.as_slice());
}

#[tokio::test]
async fn misses_are_chunked() {
    let h = harness(|c| c.batch.chunk_size = 2).await;
    mock_post(&h, "/geographies/addressbatch", fixture("batch_geo.csv")).await;
    let (_, _, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert_eq!(body.as_ref(), fixture("batch_geo.csv"));
    let up = uploads(&h).await;
    assert_eq!(up.iter().map(Vec::len).collect::<Vec<_>>(), [2, 1]);
}

/// Census's own texts for batch request errors (captured 2026-10-12, PLAN.md §11.5).
fn assert_upstream_text(status: StatusCode, headers: &HeaderMap, body: &[u8], fixture_name: &str) {
    assert_eq!(status, 400);
    assert_eq!(headers["content-type"], "text/plain;charset=UTF-8");
    assert_eq!(body, fixture(fixture_name).as_slice());
}

#[tokio::test]
async fn input_validation_errors() {
    let h = harness(|c| c.batch.max_rows = 2).await;
    let (status, headers, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        &fixture("batch_in.csv"),
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(headers["content-type"], "text/plain;charset=UTF-8");
    let text = String::from_utf8_lossy(&body);
    assert!(
        text.contains("exceeded the batch limit of 2 records"),
        "{text}"
    );

    // Missing/invalid benchmark or vintage: one generic Census body for all of them.
    for (path, fields) in [
        (ADDR_LOC, vec![]),
        (ADDR_LOC, vec![("benchmark", "99999")]),
        (ADDR_GEO, vec![("benchmark", "4")]),
        (ADDR_GEO, vec![("benchmark", "4"), ("vintage", "99999")]),
        (COORDS, vec![("benchmark", "4")]),
    ] {
        let (status, headers, body) = post(&h, path, "addressFile", b"1,a\n", &fields).await;
        assert_upstream_text(status, &headers, &body, "batch_err_params.txt");
    }

    // Census renders its HTML form for a missing file; ours is a JSON 400.
    let (status, _, body) = post(&h, ADDR_LOC, "otherField", b"1,a\n", &[("benchmark", "4")]).await;
    assert_eq!(status, 400);
    assert_eq!(json(&body)["errors"][0], "Batch file must be specified");
    assert_eq!(h.hits().await, 0);
}

#[tokio::test]
async fn empty_file_is_an_empty_200() {
    let h = harness(|_| {}).await;
    let (status, headers, body) =
        post(&h, ADDR_LOC, "addressFile", b"", &[("benchmark", "4")]).await;
    assert_eq!(status, 200);
    assert!(body.is_empty());
    assert_eq!(headers["content-type"], "text/plain");
    assert_eq!(
        headers["content-disposition"],
        "attachment; filename=GeocodeResults.csv"
    );
    assert_eq!(h.hits().await, 0);
}

#[tokio::test]
async fn extra_columns_are_ignored_like_census() {
    let h = harness(|_| {}).await;
    mock_post(
        &h,
        "/geographies/addressbatch",
        br#""1","a, b, c, d","No_Match"
"2","4600 Silver Hill Rd, Washington, DC, 20233","Match","Exact","4600 SILVER HILL RD, WASHINGTON, DC, 20233","-76.92836638093,38.84505589808","657091557","L","24","033","802405","2004"
"#
        .to_vec(),
    )
    .await;
    let (status, _, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        &fixture("batch_widerow_in.csv"),
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(status, 200);
    // Byte-identical to what Census answered for the same file.
    assert_eq!(body.as_ref(), fixture("batch_widerow.csv"));
    // Only the first five columns went upstream.
    assert_eq!(uploads(&h).await[0][0], "1,a,b,c,d");
}

#[tokio::test]
async fn single_record_entries_answer_batch_rows_heuristically() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;
    h.get("/geocoder/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=4&vintage=4&format=json")
        .await;
    let (_, headers, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        b"1,4600 Silver Hill Rd,Washington,DC,20233\n",
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert_eq!(x_cache(&headers), "HIT");
    assert_eq!(headers["x-ucg-heuristic-count"], "1");
    let first_line = String::from_utf8(fixture("batch_geo.csv"))
        .unwrap()
        .lines()
        .next()
        .unwrap()
        .to_owned()
        + "\n";
    assert_eq!(body.as_ref(), first_line.as_bytes());
}

#[tokio::test]
async fn batch_no_match_does_not_answer_single_lookups() {
    let h = harness(|_| {}).await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;
    mock_post(
        &h,
        "/geographies/addressbatch",
        br#""1","4600 Silver Hill Rd, Washington, DC, 20233","No_Match""#.to_vec(),
    )
    .await;
    let single = "/geocoder/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=4&format=json";

    // Batch first: its No_Match must not answer the single lookup.
    post(
        &h,
        ADDR_LOC,
        "addressFile",
        b"1,4600 Silver Hill Rd,Washington,DC,20233\n",
        &[("benchmark", "4")],
    )
    .await;
    let (_, headers, body) = h.get(single).await;
    assert_eq!(x_cache(&headers), "MISS");
    assert_eq!(
        json(&body)["result"]["addressMatches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
}

#[tokio::test]
async fn batch_no_match_does_not_replace_a_fresh_single_match() {
    // upgrade_when_idle sends heuristic rows upstream, so the batch really runs.
    let h = harness(|c| c.batch.upgrade_when_idle = true).await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;
    mock_post(
        &h,
        "/geographies/addressbatch",
        br#""1","4600 Silver Hill Rd, Washington, DC, 20233","No_Match""#.to_vec(),
    )
    .await;
    let single = "/geocoder/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=4&format=json";
    h.get(single).await;
    let (_, _, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        b"1,4600 Silver Hill Rd,Washington,DC,20233\n",
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(uploads(&h).await.len(), 1, "row went upstream");
    assert!(String::from_utf8_lossy(&body).contains("No_Match"));
    let (_, headers, body) = h.get(single).await;
    assert_eq!(
        x_cache(&headers),
        "HIT",
        "single Match survived the batch No_Match"
    );
    assert_eq!(
        json(&body)["result"]["addressMatches"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    assert_eq!(h.state.store.retry_len().unwrap(), 1);
}

#[tokio::test]
async fn incomplete_or_garbled_upstream_output_is_a_502_not_no_match() {
    let h = harness(|_| {}).await;
    Mock::given(method("POST"))
        .and(path("/geographies/addressbatch"))
        .and(body_string_contains("Springfield"))
        .respond_with(ResponseTemplate::new(200).set_body_raw("<html>busy</html>", "text/html"))
        .with_priority(1)
        .mount(&h.server)
        .await;
    // Answers ids 1-3 only.
    mock_post(&h, "/geographies/addressbatch", fixture("batch_geo.csv")).await;
    let fields = [("benchmark", "4"), ("vintage", "4")];

    let mut input = fixture("batch_in.csv");
    input.extend_from_slice(b"4,1 Main St,Shelbyville,IL,62565\n");
    let (status, _, body) = post(&h, ADDR_GEO, "addressFile", &input, &fields).await;
    assert_eq!(status, 502);
    assert_eq!(
        json(&body)["errors"][0],
        "Upstream batch response was incomplete"
    );

    let (status, _, body) = post(
        &h,
        ADDR_GEO,
        "addressFile",
        b"1,1 Main St,Springfield,IL,62701\n",
        &fields,
    )
    .await;
    assert_eq!(status, 502);
    assert_eq!(
        json(&body)["errors"][0],
        "Upstream batch response was not understood"
    );
    assert_eq!(
        h.state.store.retry_len().unwrap(),
        0,
        "no fabricated negatives"
    );
}

#[tokio::test]
async fn oversized_upload_is_rejected_while_reading() {
    let h = harness(|_| {}).await;
    let huge = "1\n".repeat(2_000_000);
    let (status, headers, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        huge.as_bytes(),
        &[("benchmark", "4")],
    )
    .await;
    // Census's own text, byte-for-byte at the default 10,000 limit.
    assert_upstream_text(status, &headers, &body, "batch_err_toomany.txt");
    assert_eq!(h.hits().await, 0);
}

#[tokio::test]
async fn single_refresh_does_not_resurrect_old_batch_fields() {
    let h = harness(|_| {}).await;
    let stale_row = r#""1","4600 Silver Hill Rd, Washington, DC, 20233","Match","Exact","4600 SILVER HILL RD, WASHINGTON, DC, 20233","-1.5,2.5","657091557","L","24","033","802405","2004""#;
    mock_post(
        &h,
        "/geographies/addressbatch",
        format!("{stale_row}\n").into_bytes(),
    )
    .await;
    h.mock_json("/geographies/address", fixture("geo_address_all.json"))
        .await;
    let fields = [("benchmark", "4"), ("vintage", "4")];
    let row = b"1,4600 Silver Hill Rd,Washington,DC,20233\n";
    post(&h, ADDR_GEO, "addressFile", row, &fields).await;

    // A single-record fetch replaces the batch-only entry with fresh JSON...
    let (_, headers, _) = h
        .get("/geocoder/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=4&vintage=4&format=json")
        .await;
    assert_eq!(x_cache(&headers), "MISS");
    // ...and later batch rows come from that JSON, not the old batch view.
    let (_, headers, body) = post(&h, ADDR_GEO, "addressFile", row, &fields).await;
    assert_eq!(x_cache(&headers), "HIT");
    let text = String::from_utf8(body.to_vec()).unwrap();
    assert!(text.contains("-76.92836638093,38.84505589808"), "{text}");
    assert!(!text.contains("-1.5"), "{text}");
}

#[tokio::test]
async fn coordinates_batch_no_match_keeps_single_coordinates_entry() {
    let h = harness(|c| c.batch.upgrade_when_idle = true).await;
    h.mock_json("/geographies/coordinates", fixture("geo_coords_all.json"))
        .await;
    mock_post(
        &h,
        "/geographies/coordinatesbatch",
        br#""1","-76.92744","38.84601","No_Match""#.to_vec(),
    )
    .await;
    let single = "/geocoder/geographies/coordinates?x=-76.92744&y=38.84601&benchmark=4&vintage=4&format=json";
    h.get(single).await;
    let (_, _, body) = post(
        &h,
        COORDS,
        "coordinatesFile",
        b"1,-76.92744,38.84601\n",
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert!(
        String::from_utf8_lossy(&body).contains("No_Match"),
        "upstream really ran"
    );
    let (_, headers, _) = h.get(single).await;
    assert_eq!(
        x_cache(&headers),
        "HIT",
        "single coordinates entry survived"
    );
}

#[tokio::test]
async fn truncated_upstream_match_rows_are_rejected() {
    let h = harness(|_| {}).await;
    mock_post(
        &h,
        "/geographies/coordinatesbatch",
        br#""1","-76.92744","38.84601","Match","24""#.to_vec(),
    )
    .await;
    let (status, _, _) = post(
        &h,
        COORDS,
        "coordinatesFile",
        b"1,-76.92744,38.84601\n",
        &[("benchmark", "4"), ("vintage", "4")],
    )
    .await;
    assert_eq!(status, 502);
    assert_eq!(h.state.store.entry_count().unwrap(), 0, "nothing cached");
}

#[tokio::test]
async fn one_extremely_wide_row_is_rejected() {
    let h = harness(|_| {}).await;
    let wide = format!("1{}\n", ",".repeat(2_000_000));
    let (status, _, body) = post(
        &h,
        ADDR_LOC,
        "addressFile",
        wide.as_bytes(),
        &[("benchmark", "4")],
    )
    .await;
    assert_eq!(status, 400);
    assert_eq!(json(&body)["errors"][0], "Malformed rows: 1");
}
