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
- `src/store/` — redb `Store` (RwLock only for compact), single writer thread (`writer.rs`, `Schedule`), `entries` (incl. `put_entry_unless`, `put_entries_with` = decide inside write txn), `stats`, `catalog`, `retry_queue`, `config_overrides`; `jobs.rs` not yet written (Phase 7).
- `src/upstream/` — `Upstream::execute` (throttle per attempt, retries, breaker), `throttle` (Sync/Background priorities), `backoff`, `single`, `batch`, `catalog`.
- `scripts/capture.py` — throttled live capture (`catalog`, `samples`).

## Invariants
- Store file readable by `cached::RedbCache<String, Vec<u8>>` (table `cached_disk_cache`, `<name>_v3.redb`, value positional rmp `[bin(envelope), SystemTime]`); guarded by `tests/compat_cached.rs`.
- Upstream raw JSON segments stored byte-exact; `result.input` rebuilt from request + catalog raw objects.
- Read-then-write is never used for cache decisions that race: decide inside the writer txn (`put_entry_unless`, `put_entries_with`).
- Batch-only NoMatch never answers single lookups; batch results never replace a fresh Match; incomplete/garbled upstream batch output → 502, never fabricated No_Match.
- `resources/census.dict` is `include_bytes!`ed; `train-dict` moves the outgoing dict to `resources/census.prev.dict` (decode-only). One previous generation only.
