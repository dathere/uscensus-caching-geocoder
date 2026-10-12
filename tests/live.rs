//! Live checks against census.gov (PLAN.md §9.10, §11). Skipped unless
//! `UCG_LIVE_TESTS=1`. One serial test, at most ~1 upstream request per 700 ms.
//!
//! For each case it (a) compares Census's answer with the fixture captured on
//! 2026-10-12 (drift detection: Census changes twice a year) and (b) runs the same input
//! through this proxy, in-process, pointed at the real upstream, and requires the same
//! bytes where the proxy mirrors Census. Refresh fixtures with `UCG_LIVE_RECORD=1`.

mod common;

use std::time::Duration;

use axum::{
    Router,
    body::{Body, Bytes},
    http::{HeaderMap, Request, StatusCode},
};
use common::{fixture, multipart};
use http_body_util::BodyExt;
use tower::ServiceExt;
use ucgeocoder::{
    config::{self, Layers},
    http::router,
    state::AppState,
    store::{Store, StoreOptions},
    upstream::Upstream,
};

const CENSUS: &str = "https://geocoding.geo.census.gov/geocoder";
const PAUSE: Duration = Duration::from_millis(700);

struct Live {
    client: reqwest::Client,
    proxy: Router,
    record: bool,
    _dir: tempfile::TempDir,
}

struct Answer {
    status: StatusCode,
    content_type: String,
    body: Bytes,
}

impl Live {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let mut settings = config::load(&Layers {
            toml: None,
            env: &|_| None,
            flags: &[],
        })
        .unwrap();
        settings.config.store.db_dir = dir.path().to_owned();
        let store = Store::open(&StoreOptions::from(&settings.config.store)).unwrap();
        let upstream = Upstream::new(&settings.config).unwrap();
        let proxy = router(AppState::new(settings, store, upstream));
        Self {
            client: reqwest::Client::new(),
            proxy,
            record: std::env::var_os("UCG_LIVE_RECORD").is_some(),
            _dir: dir,
        }
    }

    /// POST a batch file to Census directly.
    async fn census_batch(
        &self,
        path: &str,
        field: &str,
        file: &[u8],
        fields: &[(&str, &str)],
    ) -> Answer {
        tokio::time::sleep(PAUSE).await;
        let mut form = reqwest::multipart::Form::new().part(
            field.to_owned(),
            reqwest::multipart::Part::bytes(file.to_vec())
                .file_name("in.csv")
                .mime_str("text/csv")
                .unwrap(),
        );
        for (k, v) in fields {
            form = form.text((*k).to_owned(), (*v).to_owned());
        }
        let resp = self
            .client
            .post(format!("{CENSUS}/{path}"))
            .multipart(form)
            .timeout(Duration::from_secs(300))
            .send()
            .await
            .unwrap();
        let status = StatusCode::from_u16(resp.status().as_u16()).unwrap();
        let content_type = resp
            .headers()
            .get("content-type")
            .map(|v| v.to_str().unwrap().to_owned())
            .unwrap_or_default();
        Answer {
            status,
            content_type,
            body: resp.bytes().await.unwrap(),
        }
    }

    /// The same request through the proxy (which may itself call Census).
    async fn proxy_batch(
        &self,
        path: &str,
        field: &str,
        file: &[u8],
        fields: &[(&str, &str)],
    ) -> Answer {
        tokio::time::sleep(PAUSE).await;
        let (ct, body) = multipart(field, file, fields);
        let resp = self
            .proxy
            .clone()
            .oneshot(
                Request::post(format!("/geocoder/{path}"))
                    .header("content-type", ct)
                    .body(Body::from(body))
                    .unwrap(),
            )
            .await
            .unwrap();
        let status = resp.status();
        let headers: HeaderMap = resp.headers().clone();
        Answer {
            status,
            content_type: headers
                .get("content-type")
                .map(|v| v.to_str().unwrap().to_owned())
                .unwrap_or_default(),
            body: resp.into_body().collect().await.unwrap().to_bytes(),
        }
    }

    /// Compare Census's body with a fixture, or rewrite the fixture when recording.
    fn check_fixture(&self, name: &str, body: &[u8]) {
        if self.record {
            std::fs::write(
                format!("{}/tests/fixtures/live/{name}", env!("CARGO_MANIFEST_DIR")),
                body,
            )
            .unwrap();
        } else {
            assert_eq!(
                String::from_utf8_lossy(body),
                String::from_utf8_lossy(&fixture(name)),
                "Census's answer drifted from fixture {name} (rerun with UCG_LIVE_RECORD=1 to refresh)"
            );
        }
    }
}

fn same(case: &str, census: &Answer, proxy: &Answer) {
    assert_eq!(proxy.status, census.status, "{case}: status");
    assert_eq!(
        proxy.content_type, census.content_type,
        "{case}: content-type"
    );
    assert_eq!(
        String::from_utf8_lossy(&proxy.body),
        String::from_utf8_lossy(&census.body),
        "{case}: body"
    );
}

#[tokio::test]
async fn live_census_behaviour() {
    if std::env::var("UCG_LIVE_TESTS").as_deref() != Ok("1") {
        eprintln!("skipped: set UCG_LIVE_TESTS=1 to run against census.gov");
        return;
    }
    let live = Live::new();
    let one = b"1,4600 Silver Hill Rd,Washington,DC,20233\n";

    // §11.5: parameter errors share one text/plain body.
    for (path, fields) in [
        (
            "geographies/addressbatch",
            vec![("benchmark", "4"), ("vintage", "99999")],
        ),
        ("geographies/addressbatch", vec![("benchmark", "4")]),
        ("locations/addressbatch", vec![("benchmark", "99999")]),
        ("locations/addressbatch", vec![]),
    ] {
        let c = live.census_batch(path, "addressFile", one, &fields).await;
        assert_eq!(c.status, 400, "{path} {fields:?}");
        live.check_fixture("batch_err_params.txt", &c.body);
        let p = live.proxy_batch(path, "addressFile", one, &fields).await;
        same(&format!("params {path} {fields:?}"), &c, &p);
    }

    // §11.5: more than 10,000 records, rejected up front.
    let big: String = (1..=10_001).map(|i| format!("{i},,,,\n")).collect();
    let c = live
        .census_batch(
            "locations/addressbatch",
            "addressFile",
            big.as_bytes(),
            &[("benchmark", "4")],
        )
        .await;
    live.check_fixture("batch_err_toomany.txt", &c.body);
    let p = live
        .proxy_batch(
            "locations/addressbatch",
            "addressFile",
            big.as_bytes(),
            &[("benchmark", "4")],
        )
        .await;
    same("too many", &c, &p);

    // §11.5: an empty file is an empty 200.
    let c = live
        .census_batch(
            "locations/addressbatch",
            "addressFile",
            b"",
            &[("benchmark", "4")],
        )
        .await;
    let p = live
        .proxy_batch(
            "locations/addressbatch",
            "addressFile",
            b"",
            &[("benchmark", "4")],
        )
        .await;
    assert_eq!(c.status, 200);
    assert!(c.body.is_empty());
    same("empty file", &c, &p);

    // §11.5: extra columns are ignored (the proxy goes upstream for these rows).
    let wide = fixture("batch_widerow_in.csv");
    let c = live
        .census_batch(
            "locations/addressbatch",
            "addressFile",
            &wide,
            &[("benchmark", "4")],
        )
        .await;
    live.check_fixture("batch_widerow.csv", &c.body);
    let p = live
        .proxy_batch(
            "locations/addressbatch",
            "addressFile",
            &wide,
            &[("benchmark", "4")],
        )
        .await;
    same("wide rows", &c, &p);

    // §11.3: Tie rows are `"id","input","Tie"`. Batch Tie/No_Match answers are
    // load-dependent upstream, so this case compares row *shapes*, not bytes.
    let tie = fixture("batch_tie_in.csv");
    let c = live
        .census_batch(
            "locations/addressbatch",
            "addressFile",
            &tie,
            &[("benchmark", "4")],
        )
        .await;
    if live.record {
        live.check_fixture("batch_tie.csv", &c.body);
    }
    let p = live
        .proxy_batch(
            "locations/addressbatch",
            "addressFile",
            &tie,
            &[("benchmark", "4")],
        )
        .await;
    assert_eq!((c.status, p.status), (StatusCode::OK, StatusCode::OK));
    let shapes = |body: &[u8]| -> Vec<(usize, String)> {
        csv::ReaderBuilder::new()
            .has_headers(false)
            .flexible(true)
            .from_reader(body)
            .records()
            .map(|r| {
                let r = r.unwrap();
                (r.len(), r.get(2).unwrap_or_default().to_owned())
            })
            .collect()
    };
    for (len, kind) in shapes(&c.body) {
        if kind == "Tie" || kind == "No_Match" {
            assert_eq!(len, 3, "census {kind} row");
        }
    }
    assert_eq!(
        shapes(&p.body).len(),
        shapes(&c.body).len(),
        "tie: row count"
    );
    for (len, kind) in shapes(&p.body) {
        if kind == "Tie" || kind == "No_Match" {
            assert_eq!(len, 3, "proxy {kind} row");
        }
    }

    // §11.5: coordinates No_Match rows are `"id","x","y","No_Match"`.
    let coords = fixture("coords_nomatch_in.csv");
    let fields = [("benchmark", "4"), ("vintage", "4")];
    let c = live
        .census_batch(
            "geographies/coordinatesbatch",
            "coordinatesFile",
            &coords,
            &fields,
        )
        .await;
    live.check_fixture("batch_coords_nomatch.csv", &c.body);
    let p = live
        .proxy_batch(
            "geographies/coordinatesbatch",
            "coordinatesFile",
            &coords,
            &fields,
        )
        .await;
    same("coordinates no match", &c, &p);
}
