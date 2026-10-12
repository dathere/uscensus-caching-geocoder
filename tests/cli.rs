//! Offline CLI subcommands (PLAN.md §8.2) against a seeded store, through the binary.

use std::{
    path::Path,
    time::{Duration, SystemTime},
};

use predicates::{prelude::PredicateBooleanExt, str::contains};
use serde_bytes::ByteBuf;
use ucgeocoder::{
    cache::{
        codec::Codec,
        envelope::{BatchView, Entry, EntryPayload, EntrySource, Status, Views, encode_stored},
    },
    config::Config,
    jobs::JobKind,
    store::{Store, StoreOptions},
};

const BIN: &str = env!("CARGO_BIN_EXE_ucgeocoder");

fn cmd(dir: &Path) -> assert_cmd::Command {
    let mut c = assert_cmd::Command::new(BIN);
    c.env_clear()
        .arg("--db-dir")
        .arg(dir)
        .arg("--log")
        .arg("warn");
    c
}

fn open(dir: &Path) -> Store {
    Store::open(&StoreOptions::from(&Config::default().store).with_dir(dir)).unwrap()
}

fn entry(status: Status, source: EntrySource) -> Entry {
    let single = source == EntrySource::Single;
    Entry {
        status,
        source,
        views: if single { Views::JSON } else { Views::BATCH },
        verified_at: 1,
        payload: EntryPayload {
            json: single.then(|| ByteBuf::from(br#"[{"matchedAddress":"X"}]"#.to_vec())),
            batch: (!single).then(|| BatchView {
                matched_address: (status == Status::Match).then(|| "1 A ST".to_owned()),
                ..BatchView::default()
            }),
            benchmark_name: "Public_AR_Current".into(),
            vintage_name: "Current_Current".into(),
        },
    }
}

/// Three entries; the store is closed again on return (the CLI needs the lock).
async fn seed(dir: &Path) {
    let store = open(dir);
    let now = SystemTime::now();
    let items = [
        (
            "v1|address|4|4|1 A ST;;;",
            Status::Match,
            EntrySource::Batch,
            now,
        ),
        (
            "v1|address|4|4|NOWHERE;;;",
            Status::NoMatch,
            EntrySource::Batch,
            now - Duration::from_secs(3 * 86_400),
        ),
        (
            "v1|coordinates|4|4|1.000000,2.000000",
            Status::Tie,
            EntrySource::Batch,
            now,
        ),
    ]
    .map(|(k, status, source, at)| {
        let e = entry(status, source);
        (
            k.to_owned(),
            encode_stored(&e.encode(Codec::embedded()).unwrap(), at),
        )
    });
    store.put_entries(items.to_vec()).await.unwrap();
}

fn stats_json(dir: &Path) -> serde_json::Value {
    let out = cmd(dir).args(["stats", "--json"]).output().unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    // stdout is data only (logs go to stderr).
    serde_json::from_slice(&out.stdout).unwrap()
}

#[test]
fn fails_fast_while_the_store_is_locked() {
    let dir = tempfile::tempdir().unwrap();
    let _held = open(dir.path());
    for args in [
        vec!["stats"],
        vec!["purge", "--scope", "all"],
        vec!["compact"],
    ] {
        cmd(dir.path())
            .args(&args)
            .timeout(Duration::from_secs(20))
            .assert()
            .failure()
            .stderr(contains("locked").and(contains("is the server running?")));
    }
}

#[tokio::test]
async fn stats_purge_export_import_compact() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path()).await;
    let d = dir.path();

    let s = stats_json(d);
    assert_eq!(s["entries"], 3);
    let out = cmd(d).args(["stats", "--full"]).output().unwrap();
    assert!(String::from_utf8_lossy(&out.stdout).contains("by status"));

    cmd(d)
        .args([
            "purge",
            "--scope",
            "status",
            "--value",
            "NoMatch",
            "--dry-run",
        ])
        .assert()
        .success()
        .stdout(contains("1 entries match (dry run"));
    assert_eq!(stats_json(d)["entries"], 3);
    cmd(d)
        .args(["purge", "--scope", "expired"])
        .assert()
        .success()
        .stdout(contains("deleted 1 entries"));
    assert_eq!(stats_json(d)["entries"], 2);
    cmd(d)
        .args(["purge", "--scope", "vintage", "--value", "4"])
        .assert()
        .failure()
        .stderr(contains("benchmark:vintage"));

    // Export to a file and to stdout (data only).
    let file = d.join("export.jsonl");
    cmd(d)
        .args(["export", "-o"])
        .arg(&file)
        .assert()
        .success()
        .stderr(contains("exported 2 entries"));
    let text = std::fs::read_to_string(&file).unwrap();
    assert_eq!(text.lines().count(), 2);
    let out = cmd(d)
        .args(["export", "--prefix", "v1|coordinates|", "-o", "-"])
        .output()
        .unwrap();
    let lines: Vec<serde_json::Value> = String::from_utf8(out.stdout)
        .unwrap()
        .lines()
        .map(|l| serde_json::from_str(l).unwrap())
        .collect();
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["status"], "Tie");

    // Restore into an empty store.
    let other = tempfile::tempdir().unwrap();
    cmd(other.path())
        .arg("import")
        .arg(&file)
        .assert()
        .success()
        .stderr(contains("restored 2 entries"));
    assert_eq!(stats_json(other.path())["entries"], 2);

    // An invalid line: valid records are kept, the exit status reports the rest.
    let bad = d.join("bad.jsonl");
    std::fs::write(&bad, format!("{text}{{not json}}\n")).unwrap();
    cmd(other.path())
        .arg("import")
        .arg(&bad)
        .assert()
        .failure()
        .stderr(contains("line 3").and(contains("1 invalid record")));

    cmd(d)
        .arg("compact")
        .assert()
        .success()
        .stdout(contains("bytes"));
}

#[tokio::test]
async fn csv_import_queues_a_warm_job_for_the_server() {
    let dir = tempfile::tempdir().unwrap();
    seed(dir.path()).await;
    let csv = dir.path().join("rows.csv");
    std::fs::write(&csv, "a,1 A St,,,\nb,2 B St,Washington,DC,\n").unwrap();
    let out = cmd(dir.path())
        .arg("import")
        .arg(&csv)
        .args(["--benchmark", "Public_AR_Current"])
        .output()
        .unwrap();
    assert!(
        out.status.success(),
        "{}",
        String::from_utf8_lossy(&out.stderr)
    );
    let id = String::from_utf8(out.stdout).unwrap().trim().to_owned();
    assert!(String::from_utf8_lossy(&out.stderr).contains("1 already cached"));

    let store = open(dir.path());
    let job = store.job_get(&id).unwrap().expect("job persisted");
    assert_eq!(job.kind, JobKind::Warm);
    assert_eq!((job.totals.total, job.totals.cached), (2, 1));
    drop(store);

    cmd(dir.path())
        .arg("import")
        .arg(&csv)
        .args(["--returntype", "geographies", "--benchmark", "4"])
        .assert()
        .failure()
        .stderr(contains("Vintage must be specified"));
}

#[test]
fn catalog_show() {
    let dir = tempfile::tempdir().unwrap();
    let out = cmd(dir.path())
        .args(["catalog", "show", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success());
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(
        v["benchmarks"]
            .as_array()
            .unwrap()
            .iter()
            .any(|b| b["name"] == "Public_AR_Current")
    );
    cmd(dir.path())
        .args(["catalog", "show"])
        .assert()
        .success()
        .stdout(contains("Current_Current"));
}
