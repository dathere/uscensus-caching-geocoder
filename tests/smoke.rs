use std::{
    io::{BufRead, BufReader, Read, Write},
    net::TcpStream,
    process::{Child, Command, Stdio},
    sync::mpsc,
    thread,
    time::Duration,
};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use predicates::str::contains;
use tower::ServiceExt;
use ucgeocoder::{
    config::{self, Layers},
    http::router,
    state::AppState,
    store::{Store, StoreOptions},
    upstream::Upstream,
};

const BIN: &str = env!("CARGO_BIN_EXE_ucgeocoder");

fn cmd() -> assert_cmd::Command {
    let mut c = assert_cmd::Command::new(BIN);
    c.env_clear();
    c
}

#[tokio::test]
async fn healthz_via_router() {
    let settings = config::load(&Layers {
        toml: None,
        env: &|_| None,
        flags: &[],
    })
    .unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store =
        Store::open(&StoreOptions::from(&settings.config.store).with_dir(dir.path())).unwrap();
    let upstream = Upstream::new(&settings.config).unwrap();
    let resp = router(AppState::new(settings, store, upstream))
        .oneshot(Request::get("/healthz").body(Body::empty()).unwrap())
        .await
        .unwrap();
    assert_eq!(resp.status(), 200);
    let body = resp.into_body().collect().await.unwrap().to_bytes();
    let v: serde_json::Value = serde_json::from_slice(&body).unwrap();
    assert_eq!(v["status"], "ok");
    assert_eq!(v["breaker"]["open"], false);
    assert!(v["upstream_last_ok_at"].is_null());
    assert_eq!(v["db_ok"], true);
}

#[test]
fn help_lists_subcommands() {
    cmd()
        .arg("--help")
        .assert()
        .success()
        .stdout(contains("serve"))
        .stdout(contains("check-config"));
}

#[test]
fn check_config_reports_provenance() {
    let dir = tempfile::tempdir().unwrap();
    let toml = dir.path().join("ucg.toml");
    std::fs::write(&toml, "[throttle]\nsingle_concurrency = 3\n").unwrap();

    let out = cmd()
        .env("UCG_SINGLE_RPS", "2")
        .env("UCG_ADMIN_TOKEN", "s3cret")
        .args(["check-config", "--json", "--set", "ttl.tie=1h", "--config"])
        .arg(&toml)
        .assert()
        .success()
        .get_output()
        .stdout
        .clone();
    let rows: Vec<serde_json::Value> = serde_json::from_slice(&out).unwrap();
    let row = |p: &str| rows.iter().find(|r| r["path"] == p).unwrap().clone();
    assert_eq!(row("throttle.single_rps")["value"], "2");
    assert_eq!(row("throttle.single_rps")["source"], "env");
    assert_eq!(row("throttle.single_concurrency")["source"], "toml");
    assert_eq!(row("ttl.tie")["value"], "1h");
    assert_eq!(row("ttl.tie")["source"], "flag");
    assert_eq!(row("throttle.single_burst")["source"], "default");
    assert_eq!(row("admin.token")["value"], "***");
    assert!(!String::from_utf8_lossy(&out).contains("s3cret"));

    cmd()
        .args(["check-config", "--bind", "127.0.0.1:9999"])
        .assert()
        .success()
        .stdout(contains("server.bind"))
        .stdout(contains("127.0.0.1:9999"));
}

#[test]
fn check_config_rejects_bad_values() {
    cmd()
        .env("UCG_SINGLE_RPS", "lots")
        .arg("check-config")
        .assert()
        .failure()
        .stderr(contains("UCG_SINGLE_RPS"));
    cmd()
        .args(["check-config", "--set", "throttle.nope=1"])
        .assert()
        .failure()
        .stderr(contains("unknown config key"));
}

/// Kills the server when the test ends, pass or fail.
struct KillOnDrop(Child);

impl Drop for KillOnDrop {
    fn drop(&mut self) {
        let _ = self.0.kill();
        let _ = self.0.wait();
    }
}

#[test]
fn serve_binds_and_answers_healthz() {
    let dir = tempfile::tempdir().unwrap();
    let mut child = KillOnDrop(
        Command::new(BIN)
            .env_clear()
            .env("UCG_DB_DIR", dir.path())
            .env("UCG_LOG_FORMAT", "text")
            .args(["serve", "--bind", "127.0.0.1:0"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap(),
    );

    let stdout = child.0.stdout.take().unwrap();
    let (tx, rx) = mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if line.contains("listening") {
                let _ = tx.send(line);
                break;
            }
        }
    });
    let line = rx
        .recv_timeout(Duration::from_secs(30))
        .expect("server did not log its address");
    let addr = line
        .split_whitespace()
        .find_map(|t| t.strip_prefix("addr="))
        .expect("addr= field in log line");

    let mut stream = TcpStream::connect(addr).unwrap();
    stream
        .set_read_timeout(Some(Duration::from_secs(10)))
        .unwrap();
    stream
        .write_all(b"GET /healthz HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n")
        .unwrap();
    let mut resp = String::new();
    stream.read_to_string(&mut resp).unwrap();
    assert!(resp.starts_with("HTTP/1.1 200"), "{resp}");
    assert!(resp.contains(r#""status":"ok""#), "{resp}");
    assert!(dir.path().join("geocode-census_v3.redb").exists());
}
