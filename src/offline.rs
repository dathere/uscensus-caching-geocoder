//! Offline subcommands (PLAN.md §8.2): open the store directly (failing fast with
//! "locked … is the server running?" while the server holds it), run one maintenance
//! operation through the same code as the admin API, flush durably, and exit.

use std::{
    io::Write,
    path::{Path, PathBuf},
};

use anyhow::{Context, bail};

use crate::{
    cache::{
        admin::{cache_stats, compact, export_pages, parse_scope, purge_context, store_imported},
        maintenance::{parse_import, purge},
    },
    census::types::ReturnType,
    config::Settings,
    http::{api_admin::catalog_view, batch::resolve_target, geocoder::refresh_catalog},
    jobs::{
        JobKind,
        submit::{self, JobSpec},
    },
    state::{AppState, SharedState},
    store::{Store, StoreOptions},
    upstream::Upstream,
};

/// `a` exists and is the same file as `b` (same device and inode on Unix, so hard and
/// symbolic links are caught; the same canonical path elsewhere).
fn is_same_file(a: &Path, b: &Path) -> bool {
    let (Ok(ma), Ok(mb)) = (std::fs::metadata(a), std::fs::metadata(b)) else {
        return false;
    };
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ma.dev() == mb.dev() && ma.ino() == mb.ino()
    }
    #[cfg(not(unix))]
    {
        let _ = (ma, mb);
        matches!((a.canonicalize(), b.canonicalize()), (Ok(x), Ok(y)) if x == y)
    }
}

/// Open the store and build the app state (no server, no job runner).
fn open(settings: Settings) -> anyhow::Result<SharedState> {
    let store = Store::open(&StoreOptions::from(&settings.config.store))?;
    let upstream = Upstream::new(&settings.config)?;
    Ok(AppState::new(settings, store, upstream))
}

/// Run `f` against an offline state, then make every write durable before the state
/// (and with it the store's writer) is dropped.
fn with_state<T>(
    settings: Settings,
    f: impl AsyncFnOnce(&SharedState) -> anyhow::Result<T>,
) -> anyhow::Result<T> {
    let rt = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()?;
    rt.block_on(async move {
        let st = open(settings)?;
        let out = f(&st).await;
        st.store.flush().await.context("flushing the store")?;
        drop(st);
        out
    })
}

pub fn stats(settings: Settings, json: bool, full: bool) -> anyhow::Result<()> {
    with_state(settings, async |st| {
        let s = cache_stats(st, full)?;
        if json {
            println!("{}", serde_json::to_string_pretty(&s)?);
            return Ok(());
        }
        println!("entries          {}", s.entries);
        println!("file bytes       {}", s.file_bytes);
        println!("retry queue      {}", s.retry_queue_len);
        for (k, v) in &s.counters {
            println!("{k:<16} {v}");
        }
        if let Some(r) = s.hit_ratio {
            println!("hit ratio        {:.1}%", r * 100.0);
        }
        for (state, n) in &s.jobs {
            println!("jobs {state:<11} {n}");
        }
        if let Some(d) = &s.detail {
            println!("by status        {:?}", d.by_status);
            println!("by searchtype    {:?}", d.by_searchtype);
            println!("by benchmark     {:?}", d.by_benchmark);
            if let Some(c) = &d.compression {
                println!("compression      {:.2}x (sample of {})", c.ratio, c.sampled);
            }
        }
        Ok(())
    })
}

pub fn purge_cmd(
    settings: Settings,
    scope: &str,
    value: Option<&str>,
    dry_run: bool,
) -> anyhow::Result<()> {
    with_state(settings, async |st| {
        let scope_v = parse_scope(&st.catalog.load(), scope, value).map_err(anyhow::Error::msg)?;
        let n = purge(&st.store, scope_v, purge_context(st), dry_run).await?;
        if dry_run {
            println!("{n} entries match (dry run, nothing deleted)");
        } else {
            println!("deleted {n} entries");
        }
        Ok(())
    })
}

/// Export to `output` (`-` = stdout).
pub fn export(settings: Settings, prefix: &str, output: &Path) -> anyhow::Result<()> {
    with_state(settings, async |st| {
        if output != Path::new("-") && is_same_file(output, st.store.path()) {
            bail!(
                "refusing to export into the database file {}",
                st.store.path().display()
            );
        }
        let mut out: Box<dyn Write + Send> = if output == Path::new("-") {
            Box::new(std::io::BufWriter::new(std::io::stdout()))
        } else {
            Box::new(std::io::BufWriter::new(
                std::fs::File::create(output)
                    .with_context(|| format!("creating {}", output.display()))?,
            ))
        };
        let mut io_err = None;
        let records = export_pages(&st.store, st.codec, prefix, |chunk| {
            match out.write_all(&chunk) {
                Ok(()) => true,
                Err(e) => {
                    io_err = Some(e);
                    false
                }
            }
        })?;
        if let Some(e) = io_err {
            return Err(e).context("writing the export");
        }
        out.flush()?;
        eprintln!("exported {records} entries");
        Ok(())
    })
}

#[derive(Debug, Clone, Default)]
pub struct CsvImport {
    pub returntype: String,
    pub benchmark: Option<String>,
    pub vintage: Option<String>,
    pub searchtype: String,
}

/// Restore JSON lines (whole file in memory; no body limit here), or create a `warm`
/// job from a CSV that the server runs when it next starts.
pub fn import(settings: Settings, file: &PathBuf, csv: Option<CsvImport>) -> anyhow::Result<()> {
    // The store is opened (and its lock taken) before the possibly large file is read.
    with_state(settings, async |st| {
        let data = std::fs::read(file).with_context(|| format!("reading {}", file.display()))?;
        import_data(st, file, &data, csv.as_ref()).await
    })
}

async fn import_data(
    st: &SharedState,
    file: &Path,
    data: &[u8],
    csv: Option<&CsvImport>,
) -> anyhow::Result<()> {
    match csv {
        None => {
            let parsed = parse_import(data, st.codec, 20);
            let records = parsed.records.len();
            store_imported(st, parsed.records).await?;
            eprintln!("restored {records} entries");
            for (line, error) in &parsed.errors {
                eprintln!("line {line}: {error}");
            }
            if parsed.invalid > 0 {
                bail!("{} invalid record(s) were skipped", parsed.invalid);
            }
            Ok(())
        }
        Some(c) => {
            let rt = ReturnType::parse(&c.returntype)
                .context("returntype must be locations or geographies")?;
            let (bench, vintage) = resolve_target(
                &st.catalog.load(),
                c.benchmark.as_deref(),
                c.vintage.as_deref(),
                rt,
            )
            .map_err(anyhow::Error::msg)?;
            let name = file.file_name().map(|n| n.to_string_lossy().into_owned());
            let inputs = submit::inputs_from_file(data, name.as_deref(), &c.searchtype)
                .map_err(anyhow::Error::msg)?;
            let spec = JobSpec {
                kind: JobKind::Warm,
                return_type: rt,
                bench,
                vintage,
                original_filename: name,
                priority: -1,
            };
            let (search_type, rows) = submit::build_rows(&spec, inputs)?;
            let job = submit::create(st, spec, search_type, rows, true).await?;
            println!("{}", job.id);
            eprintln!(
                "created warm job {}: {} rows, {} already cached; it runs when the server next starts",
                job.id, job.totals.total, job.totals.cached
            );
            Ok(())
        }
    }
}

pub fn compact_cmd(settings: Settings) -> anyhow::Result<()> {
    with_state(settings, async |st| {
        let store = st.store.clone();
        let r = tokio::task::spawn_blocking(move || compact(&store)).await??;
        println!(
            "{} ({} -> {} bytes, {} ms)",
            if r.compacted {
                "compacted"
            } else {
                "nothing to compact"
            },
            r.file_bytes_before,
            r.file_bytes_after,
            r.millis
        );
        Ok(())
    })
}

pub fn catalog(settings: Settings, refresh: bool, json: bool) -> anyhow::Result<()> {
    with_state(settings, async |st| {
        if refresh {
            let failures = refresh_catalog(st.clone()).await;
            if !failures.is_empty() {
                bail!("catalog refresh failed: {}", failures.join("; "));
            }
        }
        let view = catalog_view(st);
        if json {
            println!("{}", serde_json::to_string_pretty(&view)?);
            return Ok(());
        }
        for b in &view.benchmarks {
            println!(
                "{:>5}  {}{}  ({})",
                b.id,
                b.name,
                if b.is_default { " [default]" } else { "" },
                b.class
            );
            for v in &b.vintages {
                println!(
                    "       {:>5}  {}{}",
                    v.id,
                    v.name,
                    if v.is_default { " [default]" } else { "" }
                );
            }
        }
        Ok(())
    })
}
