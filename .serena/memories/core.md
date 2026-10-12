# ucgeocoder core

- Rust server: caching, rate-limited, wire-compatible proxy for the US Census Geocoder. Binary/crate `ucgeocoder`.
- Authoritative design + phase plan: `docs/PLAN.md` (§2 Decisions are closed; §13 Progress log per phase; §13.1 resume notes + working agreement incl. roborev review loop).
- Ground truth for wire formats: `tests/fixtures/live/` (captured from census.gov; recapture commands in PLAN §0.2).
- Stack/versions: `mem:tech_stack`. Commands: `mem:suggested_commands`. Done-checklist: `mem:task_completion`. Code patterns: `mem:conventions`.

## Source map
- `src/config.rs` — `define_config!` table is the single source of every config key; layers applied via `Settings::set`.
- `src/cli.rs` — subcommands `serve`, `check-config`, `train-dict`.
- `src/state.rs` — `AppState` (settings ArcSwap, store, upstream, catalog ArcSwap, singleflight, codec, stats); `load_catalog`.
- `src/http/` — `mod.rs` router + serve; `geocoder.rs` single-record + `/benchmarks` `/vintages` (FetchJob, singleflight, passthrough/BYPASS); `batch.rs` batch endpoints (`Row` trait, `run_batch`).
- `src/census/` — pure wire formats: `params` (parse/validate; unknown cases → Passthrough), `layers` (RawValue-preserving filter), `response` (render, JSONP, flat), `batch_csv` (input/output/upstream CSV, heuristics), `types`.
- `src/cache/` — `key`, `codec` (zstd + embedded dict + prev dict), `envelope` (17-byte header, `UpstreamSingle::from_response`), `policy` (TTL), `views` (single/batch classification, `merge_batch`), `singleflight`.
- `src/store/` — redb `Store` (RwLock only for compact), single writer thread (`writer.rs`, `Schedule`), `entries` (incl. `put_entry_unless`, `put_entries_with(items, merge, retries)` = decide inside write txn), `stats`, `catalog`, `retry_queue` (`retry_items_for` full scan), `config_overrides`, `jobs` (typed `WriteCtx` helpers `job_record/put_job_*`, `job_create` + flush, `job_update`, `job_delete`).
- `src/batch.rs` — shared batch engine: `Row` trait, `lookup` (classify_batch off-thread), `fetch_answers` (upload + strict parse, stores nothing), `fetch_chunk` (HTTP: answers + store + retries in one txn).
- `src/jobs/` — `mod.rs` types (JobRecord/JobRow/ChunkState/Counts, RowInput), `submit` (build_rows, cache precheck, create), `worker` (single runner; claim → execute → `commit` writes entries + retry queue + rows + chunk + totals in ONE txn; startup resets all InFlight), `refresh` (refresh-negatives jobs revalidated against live queue; scheduler; `AppState.refresh_lock`), `results` (join with current entries, status fallback).
- `src/http/` admin side: `openapi.rs::api()` = every `/api/v1` route as one utoipa `OpenApiRouter` (+ `bearerAuth`), wrapped with body limit + `auth::require_token` in `router()`; spec public at `/api-docs/openapi.json` (snapshot `tests/snapshots/openapi.json`, refresh with `UCG_UPDATE_SNAPSHOTS=1`). `api_jobs.rs`, `api_cache.rs` (stats/entries/purge/export/import/compact), `api_admin.rs` (catalog, config overrides, upstream), `metrics.rs` (global Prometheus recorder, `ucg_requests_total` via route_layer over geocoder routes, gauges at scrape, own guard).
- `src/http/admin_ui.rs` + `admin-ui/` (vanilla SPA, rust-embed; debug builds read files from disk, release embeds): `/admin`, `/admin/`, `/admin/{*path}` (asset or index fallback; ext 404; no traversal); Swagger UI at `/admin/docs` configured to the existing public spec route (does not serve it), `validator_url("none")`. SPA rules: no native dialogs, render data via textContent, 401 → re-prompt token.
- `src/offline.rs` + `cli.rs`: offline subcommands (stats/purge/export/import/compact/catalog) over `cache/admin.rs` (shared with `http/api_cache.rs`); open store directly (lock = fail fast), flush before exit, logs to stderr.
- Batch wire fidelity (Phase 10, live-verified): Census `text/plain` bodies for param errors / >10k (`census/batch_csv.rs` UPSTREAM_PARAM_ERROR, upstream_too_many), empty file → empty 200, extra columns ignored (MAX_INPUT_FIELDS 64). `tests/live.rs` (UCG_LIVE_TESTS=1) compares proxy vs census byte-for-byte; `benches/hot_path.rs` HIT medians ~18–28 µs.
- `src/cache/maintenance.rs` — purge scopes (header-only predicate shared by dry run + write-txn delete), `ExportRecord` (decoded; json as RawValue) import/export. Import validation is deliberately bounded (user decision after 3 review rounds): views vs payload presence, non-empty Match, JSON shape the renderer reads (address matches array; layers = arrays of objects), checked timestamps. Beyond that, import (admin-only) trusts its own export format — close deeper import-validation findings as marginal.
- Runtime config: `AppState` keeps base `Settings`; `Settings::with_overrides` (config.rs) applies `CONFIG_OVERRIDES` as `Source::Override`; `AppState::apply_settings` swaps + `Upstream::reconfigure`; overridable sections cache/ttl/retry/batch/jobs/throttle.
- `src/upstream/` — `Upstream::execute` (throttle per attempt, retries, breaker), `throttle` (Sync/Background priorities), `backoff`, `single`, `batch`, `catalog`.
- `scripts/capture.py` — throttled live capture (`catalog`, `samples`).

## Invariants
- Store file readable by `cached::RedbCache<String, Vec<u8>>` (table `cached_disk_cache`, `<name>_v3.redb`, value positional rmp `[bin(envelope), SystemTime]`); guarded by `tests/compat_cached.rs`.
- Upstream raw JSON segments stored byte-exact; `result.input` rebuilt from request + catalog raw objects.
- Read-then-write is never used for cache decisions that race: decide inside the writer txn (`put_entry_unless`, `put_entries_with`).
- Batch-only NoMatch never answers single lookups; batch results never replace a fresh Match; incomplete/garbled upstream batch output → 502, never fabricated No_Match.
- `resources/census.dict` is `include_bytes!`ed; `train-dict` moves the outgoing dict to `resources/census.prev.dict` (decode-only). One previous generation only.
