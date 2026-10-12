# ucgeocoder core

- Greenfield Rust server: caching, rate-limited, wire-compatible proxy for the US Census Geocoder. Binary/crate `ucgeocoder`.
- Authoritative design + phase plan: `docs/PLAN.md` (§2 Decisions are closed; §13 Progress log is updated after every phase, one commit per phase).
- Ground truth for wire formats: `tests/fixtures/live/` (captured from census.gov; recapture commands in PLAN §0.2).
- Stack/versions: `mem:tech_stack`. Commands: `mem:suggested_commands`. Done-checklist: `mem:task_completion`. Code patterns: `mem:conventions`.

## Source map
- `src/config.rs` — `define_config!` table is the single source of every config key (path, `UCG_*` env, default). Layers applied via `Settings::set`.
- `src/cli.rs` — clap subcommands (`serve`, `check-config`, `train-dict`, more in later phases).
- `src/http/` — axum router + `serve` loop.
- `src/cache/` — `key` (key grammar), `codec` (zstd + embedded dict), `envelope` (17-byte header + rmp payload, cached-compatible stored value), `policy` (TTL/freshness).
- `src/catalog.rs` — benchmarks/vintages/layers; embedded fallback from `resources/*.json`.
- `src/dict/train.rs` — dictionary training from `resources/samples/` (gitignored).
- `scripts/capture.py` — throttled live capture (`catalog` regenerates resources JSON, `samples` for dict training).

## Invariants
- Store file must remain readable by `cached::RedbCache<String, Vec<u8>>` (table `cached_disk_cache`, file `<name>_v3.redb`, value = positional rmp `[bin(envelope), SystemTime]`); guarded by `tests/compat_cached.rs`.
- Upstream raw JSON segments (`addressMatches` array / coordinates `geographies` object) are stored byte-exact; `result.input` is always rebuilt from the request + catalog raw objects.
- `resources/census.dict` is `include_bytes!`ed; an empty file only bootstraps `train-dict`. Retraining changes `dict_id`; old ids need the previous dict kept in `Codec` or entries become misses.
