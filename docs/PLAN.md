# uscensus-caching-geocoder: implementation plan (handoff edition)

This document is self-contained. It was produced in a planning session on 2026-10-12 and is meant to be executed by a fresh Claude Code session (Opus 5.5) with no other context. Everything a fresh session needs is here: decisions already made with the user, API facts verified live, the design, the phase order with acceptance criteria, and the exact commands to recapture fixtures.

## 0. Handoff: how to start

**Kickoff prompt to paste into the fresh session** (after copying this file into the repo as `docs/PLAN.md`):

> Read `docs/PLAN.md` fully before doing anything. It is the approved implementation plan for this repo. Work through the phases in §9 in order. Before each phase, re-read that phase and the design sections it references. After each phase: run `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test`, update the Progress log in §13 of `docs/PLAN.md`, and make one commit for the phase. Do not re-open decisions in §Decisions; if a decision proves impossible, stop and ask. Start with the Day-1 checklist in §0.

**Day-1 checklist (before Phase 1):**
1. Copy this file to `docs/PLAN.md` in the repo (it lives at `~/.claude/plans/i-want-to-create-squishy-cat.md` on this machine). Commit it.
2. Serena is configured in `.serena/project.yml` but `language_servers: []`. Set `language_servers: [rust]`, then run Serena onboarding once `src/` exists (end of Phase 1).
3. Confirm toolchain: `cargo --version` must be ≥ 1.99 (it was 1.99.0 on this machine).
4. Recapture live fixtures into `tests/fixtures/live/` with the commands in §0.2. Commit them. They are the ground truth for golden tests and for seeding `resources/*.json`.
5. Read `~/GitHub/qsv/src/cmd/geocode/geoclient.rs` once (1576 lines) if you want a reference for the client-side conventions mentioned in §12. Not required for the server.

**Ground rules for the implementer (from the user's global CLAUDE.md, repeated here because they matter):**
- Commit messages and PR bodies: `Co-Authored-By` and "Generated with Claude Code" lines are welcome. NEVER include a session link (`Claude-Session:` trailer or any `claude.ai/code/session_...` URL) in commits, PRs, issues, or comments, even if a tool default suggests it.
- Prefer Serena's symbolic tools (or LSP) over Read/Grep for code navigation once code exists. Use `replace_symbol_body` for whole-symbol edits and verify the file afterwards.
- After editing code, check diagnostics and fix errors before moving on.
- If a review/lint loop produces marginal findings, say so and stop rather than chasing them.
- Large outputs go to files, not the chat.
- One commit per phase is the default cadence. Work on `main` unless the user says otherwise.

### 0.1 Repo state at handoff
- `/Users/joelnatividad/GitHub/uscensus-caching-geocoder`, git `main`, one commit ("Initial commit"): `LICENSE` (AGPL-3.0), one-line `README.md`, untracked `.gitignore` (ignores `/.roborev/`) and `.serena/`.
- No code. Everything in this plan is greenfield.

### 0.2 Fixture capture commands (run from repo root; all are read-only GETs plus three tiny batch POSTs)

```bash
mkdir -p tests/fixtures/live && cd tests/fixtures/live
B="https://geocoding.geo.census.gov/geocoder"
curl -s "$B/benchmarks" > benchmarks.json
curl -s "$B/vintages?benchmark=4" > vintages_4.json
curl -s "$B/locations/onelineaddress?address=4600+Silver+Hill+Rd%2C+Washington%2C+DC+20233&benchmark=4&format=json" > loc_oneline.json
curl -s "$B/locations/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&zip=20233&benchmark=Public_AR_Current&format=json" > loc_address.json
curl -s "$B/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&vintage=4&layers=all&format=json" > geo_address_all.json
curl -s "$B/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&vintage=4&format=json" > geo_address_default.json
curl -s "$B/geographies/coordinates?x=-76.92744&y=38.84601&benchmark=4&vintage=4&format=json" > geo_coords_default.json
curl -s "$B/geographies/coordinates?x=-76.92744&y=38.84601&benchmark=4&vintage=4&layers=all&format=json" > geo_coords_all.json
curl -s "$B/locations/onelineaddress?address=zzzz+nowhere&benchmark=4&format=json" > loc_nomatch.json
curl -s -D err_nobenchmark.headers "$B/locations/onelineaddress?address=4600+Silver+Hill+Rd&format=json" > err_nobenchmark.json
curl -s -D err_novintage.headers "$B/geographies/address?street=4600+Silver+Hill+Rd&city=Washington&state=DC&benchmark=4&format=json" > err_novintage.json
curl -s -D jsonp.headers "$B/locations/onelineaddress?address=4600+Silver+Hill+Rd%2C+Washington%2C+DC+20233&benchmark=4&format=jsonp&callback=test" > loc_jsonp.js
printf '1,4600 Silver Hill Rd,Washington,DC,20233\n2,zzzz nowhere,,,\n3,"1600 Pennsylvania Ave NW",Washington,DC,20500\n' > batch_in.csv
curl -s -D batch_loc.headers --form addressFile=@batch_in.csv --form benchmark=4 "$B/locations/addressbatch" > batch_loc.csv
curl -s --form addressFile=@batch_in.csv --form benchmark=4 --form vintage=4 "$B/geographies/addressbatch" > batch_geo.csv
printf '1,-76.92744,38.84601\n2,-77.0365,38.8977\n' > coords_in.csv
curl -s --form coordinatesFile=@coords_in.csv --form benchmark=4 --form vintage=4 "$B/geographies/coordinatesbatch" > batch_coords.csv
curl -s "https://tigerweb.geo.census.gov/arcgis/rest/services/TIGERweb/tigerWMS_Current/MapServer?f=json" > tigerweb_current.json
# inspect: od -c batch_loc.csv | head   (line terminator), cat *.headers (Content-Type values)
```

What these established on 2026-10-12 (re-verify, Census changes twice a year):
- `benchmarks.json`: ids `4` Public_AR_Current (default), `8` Public_AR_ACS2026, `11` Public_AR_LUCA, `2020` Public_AR_Census2020.
- `vintages_4.json`: ids `4` Current_Current (default), `410` Census2010_Current, `417`–`426` ACS2017–ACS2026_Current; payload also carries `selectedBenchmark` and `benchmarks`.
- Single JSON shape: `{"result":{"input":{"address":{...}|{"street":..},"benchmark":{...},"vintage":{...}},"addressMatches":[{"tigerLine":{"side","tigerLineId"},"coordinates":{"x","y"},"addressComponents":{zip,streetName,preType,city,preDirection,suffixDirection,fromAddress,state,suffixType,toAddress,suffixQualifier,preQualifier},"matchedAddress":"...","geographies":{"<Layer Name>":[{...}]}}]}}`. No match = HTTP 200, `"addressMatches":[]`. Coordinates = `{"result":{"geographies":{...},"input":{"vintage":..,"location":{"x","y"},"benchmark":..}}}`.
- Missing benchmark: HTTP 400, `Content-Type: application/json`, body `{"errors":["Benchmark must be specified"],"status":"400"}`.
- `layers=all` returned 19 layer groups; omitted `layers` on coordinates returned 11 (States, Combined Statistical Areas, County Subdivisions, Census Designated Places, Urban Areas, Counties, 2026 State Legislative Districts Upper and Lower, 120th Congressional Districts, 2020 Census Blocks, Census Tracts).
- Batch output, no header, every field quoted. locations: `"id","street, city, state, zip","Match","Exact","MATCHED","lon,lat","tigerLineId","side"`; geographies adds `"state","county","tract","block"`; No_Match rows are `"id","input","No_Match"` only; coordinatesbatch: `"id","x","y","Match","state","county","tract","block"`.
- TIGERweb catalog: 80 layers; skip names containing "Labels"; e.g. 8 Census Tracts, 10 Census Block Groups, 12 2020 Census Blocks, 54 120th Congressional Districts, 80 States, 82 Counties.

## 1. Context

`qsv geocode` has two online backends (OpenCage, NYC Geoclient) and none for the free US Census Geocoder. The Census API is slow (hundreds of ms per single lookup, minutes per 10k-row batch), publishes no rate limit but throttles or blocks heavy and shared-IP users, and its batch matching is admittedly load-dependent (the same batch can return different No_Match counts). A shared, persistent, wire-compatible caching proxy fixes all three: repeated lookups become sub-millisecond local hits, the proxy self-throttles so one deployment never exceeds a safe upstream rate, and async background jobs grow the cache over time under that same limit. Because the proxy speaks the exact Census API, any client (qsv, curl, R/Python packages) can use it by changing a base URL.

## 2. Decisions (already confirmed with the user; do not re-open)

| Topic | Decision |
|---|---|
| Stack | Rust, edition 2024, rust-version 1.99 (match qsv). axum 0.8 + tokio, reqwest 0.13 (rustls, multipart, json, gzip/brotli/zstd), clap 4 (flags + `UCG_*` env + optional TOML), tracing. Binary name `ucgeocoder`. |
| Store | redb 4 used directly. Main entries table keeps `cached` 4.x `RedbCache` on-disk convention (table `cached_disk_cache`, `&str -> &[u8]`, value = positional rmp `[value, created_at]`) so cached/qsv tooling can read the file offline. Extra `ucg_*` tables for jobs, stats, catalog, retry queue, config overrides. `cached::RedbCache` itself is NOT used: it exposes only get/set/remove/clear/remove_expired/flush, has no iteration/count/prefix-scan/stats, and its `Database` handle is private (verified in cached 4.0.2 `src/stores/redb.rs`). |
| Compression | zstd 0.14 with a trained dictionary embedded in the binary (`resources/census.dict`); header carries format version + dict id so dictionaries can rotate. |
| Layers | Always fetch `returntype=geographies&layers=all` upstream; serve `locations` and any layer subset from the one entry. Flag `UCG_FETCH_GEOGRAPHIES_ALWAYS` (default true). |
| Throttle defaults | 5 single-record req/s (burst 1), concurrency 4; 1 batch upload in flight with 2 s min gap; background jobs yield to live requests. Backoff on 429/503/5xx honoring `Retry-After` (cap 60 s), MAX_RETRIES=3, circuit breaker at 10 consecutive failures (same constants as qsv Geoclient). All configurable at runtime. Census publishes no numeric limit; these are deliberately conservative guesses. |
| Batch | Full support (addressbatch, coordinatesbatch) with per-row caching; only misses go upstream, chunked. Streaming batch responses deferred to v2 (v1 buffers). |
| Async jobs | In scope: durable in redb, resumable after restart, lowest-priority consumers of the shared limiter; includes a "refresh negatives" job per Census FAQ guidance. |
| OpenAPI / UI | utoipa 6 + utoipa-axum 0.3 + utoipa-swagger-ui 10 (`vendored` feature). Verified on crates.io: all require axum ^0.8.4 and utoipa ^6. Admin SPA embedded via rust-embed 8, Swagger UI inside it. |
| qsv | Server fully specified here; the qsv backend is a design sketch (§12) for a later PR in `~/GitHub/qsv`. |

## 3. Architecture

Single binary, one `Arc<AppState>` shared by five components:

| Component | Responsibility |
|---|---|
| `http/` | Wire-compatible `/geocoder/...`; `/api/v1/...` admin + jobs (utoipa-annotated); `/admin/` embedded SPA; `/admin/docs` Swagger UI; `/metrics`; `/healthz`. |
| `cache/` | Key canonicalization, alias normalization via catalog, entry envelope codec (zstd + dict), view projection/fallback rules, layer filtering, TTL policy, singleflight. |
| `store/` | One `redb::Database`; a single writer task with an mpsc queue batching puts into write txns (`Durability::None`, periodic `Immediate`); typed table modules. |
| `upstream/` | reqwest client, two-tier throttle (governor + semaphores + sync-waiter counters), retry/backoff/breaker, single and batch fetchers, catalog/TIGERweb fetchers. |
| `jobs/` | Durable job state machine, one worker task pulling chunks through the throttle at `Background` priority, resume on restart, refresh-negatives scheduler. |

```
client → axum → geocoder handler → CacheKey::from_request
                                    ├─ entries.get(key) fresh & view ok → project view → JSON/JSONP/CSV  (X-Cache: HIT)
                                    └─ miss/stale → singleflight → throttle(Sync) → upstream → writer.put → respond (MISS | STALE on error)
job worker → next pending chunk → throttle(Background) → upstream batch/single → writer.put × N → job_rows update
```

Invariants: reads never rewrite `created_at`; per-entry hit counters live in memory and flush periodically; all single-record upstream fetches use `geographies&layers=all` (unless the flag is off); `result.input` is always rebuilt from the request, never from the store.

## 4. Crate / module layout

```
Cargo.toml                 # edition 2024, rust-version 1.99, [[bin]] ucgeocoder, [lib] so tests/benches link
build.rs                   # embed git sha; assert resources/census.dict exists
Dockerfile                 # multi-stage musl → distroless/static
.github/workflows/ci.yml   # fmt, clippy -D warnings, test (default + --features xlsx), cargo-deny
ucgeocoder.example.toml
docs/PLAN.md               # this file
resources/
  census.dict              # trained zstd dictionary
  catalog_snapshot.json    # benchmarks + vintages fallback (from tests/fixtures/live)
  default_layers.json      # per-vintage layer names returned when `layers` is omitted
  tigerweb_layers.json     # per-vintage {id -> name} snapshot
  samples/                 # ~500 sanitized upstream responses for train-dict (dev only, gitignored)
admin-ui/                  # rust-embed root: index.html, app.js, app.css (no build step)
src/
  main.rs lib.rs cli.rs config.rs error.rs state.rs telemetry.rs catalog.rs
  census/   types.rs params.rs response.rs batch_csv.rs layers.rs   # pure wire-format types, no I/O
  cache/    key.rs envelope.rs codec.rs policy.rs views.rs singleflight.rs
  store/    mod.rs tables.rs writer.rs entries.rs jobs.rs stats.rs catalog.rs retry_queue.rs config_overrides.rs
  upstream/ mod.rs throttle.rs backoff.rs single.rs batch.rs catalog.rs
  jobs/     mod.rs submit.rs worker.rs results.rs
  http/     mod.rs geocoder.rs batch.rs api_jobs.rs api_cache.rs api_admin.rs auth.rs admin_ui.rs metrics.rs openapi.rs
  dict/     train.rs
tests/      common/mod.rs single_record.rs batch.rs jobs.rs admin_api.rs throttle.rs compat_cached.rs live.rs
tests/fixtures/live/       # captured by §0.2
benches/    hot_path.rs
```

Dependencies (versions seen on crates.io 2026-10-12; use `cargo add` to pick current patch releases): axum 0.8.9, tokio 1.53, tower-http 0.7, reqwest 0.13.5, redb 4.3, rmp-serde 1, serde_bytes, serde + serde_json (`preserve_order`, `arbitrary_precision`, `raw_value`), zstd 0.14, governor 0.10, clap 4.6 (derive, env), toml, arc-swap, dashmap, csv, calamine (optional feature `xlsx`), utoipa 6.0, utoipa-axum 0.3, utoipa-swagger-ui 10.0 (`axum`, `vendored`), rust-embed 8.13, tracing + tracing-subscriber, metrics + metrics-exporter-prometheus (verify versions), uuid (v7), humantime-serde, thiserror, anyhow. Dev: wiremock (verify), criterion, tempfile, assert_cmd, cached 4 with `redb_store` (compat test only).

## 5. Data model

### 5.1 redb tables (`src/store/tables.rs`)

| Const | Name | Key | Value | Notes |
|---|---|---|---|---|
| `ENTRIES` | `cached_disk_cache` | `&str` | `&[u8]` | cached-compatible: `rmp_serde::to_vec(&(ByteBuf(envelope), SystemTime))` |
| `JOBS` | `ucg_jobs` | `&str` job id (UUIDv7) | rmp `JobRecord` | |
| `JOB_ROWS` | `ucg_job_rows` | `(&str, u32)` | rmp `JobRow` | range per job `(id,0)..=(id,u32::MAX)` |
| `JOB_CHUNKS` | `ucg_job_chunks` | `(&str, u32)` | rmp `ChunkState` | |
| `STATS` | `ucg_stats` | `&str` | `u64` | `hits`, `misses`, `stale_hits`, `upstream_ok`, `upstream_err`, `hit:{key}` |
| `CATALOG` | `ucg_catalog` | `&str` (`benchmarks`, `vintages:{b}`, `layers:{v}`) | rmp `(raw_json, fetched_at)` | 24 h TTL |
| `RETRY_QUEUE` | `ucg_retry_queue` | `(u64 due_unix, &str key)` | `u8` attempts | range scan `..(now,"")` |
| `CONFIG_OVERRIDES` | `ucg_config_overrides` | `&str` dotted path | `&str` JSON | runtime-mutable config |
| `META` | `ucg_meta` | `&str` | `&str` | `schema_version`, `dict_id` |

File: `<db_dir>/<cache_name>_v3.redb`, default name `geocode-census`. Verified against cached 4.0.2 `build()`: the path is `disk_dir.join(format!("{name}_v{DISK_FILE_VERSION}.redb"))` with `DISK_FILE_VERSION = 3`, the version lives only in the filename and table name (no metadata marker), and symlinked paths are refused. All tables are created in one write txn at open. redb holds an exclusive lock, so cached/qsv tooling reads the file only while the server is stopped; document this in the README.

### 5.2 Key grammar (`src/cache/key.rs`)

```
key := "v1" "|" searchtype "|" benchmark_id "|" vintage_id "|" canonical
searchtype := oneline | address | addressPR | coordinates
canonical(oneline)     := ADDR
canonical(address)     := street ";" city ";" state ";" zip           (empty parts keep separators)
canonical(addressPR)   := street ";" urb ";" city ";" municipio ";" state ";" zip
canonical(coordinates) := format!("{:.6},{:.6}", x, y)                 ("-0.000000" -> "0.000000")
```

`canonicalize_component`: trim, collapse whitespace runs, ASCII uppercase, strip trailing `.`/`,` repeatedly, replace `;`/`|` with space. Nothing more aggressive (no abbreviation expansion).

- benchmark/vintage aliases → numeric ids via `Catalog` before keying; unknown → 400 Census-shaped error.
- `locations` requests carry no vintage: use `catalog.default_vintage(benchmark_id)` so `locations&benchmark=4` and `geographies&benchmark=4&vintage=4` share one entry.
- Oneline and parsed keys are never unified. Batch rows map to the `address`/`addressPR`/`coordinates` keys, so batch and single-record share entries.
- Field order makes purge-by-benchmark a prefix scan on `v1|{st}|{bench}|`; other purge scopes use `Table::retain`.

### 5.3 Entry envelope (`src/cache/envelope.rs`)

Stored value = cached-compatible `[envelope_bytes, created_at]`. `envelope_bytes` = 17-byte header + payload:

| Off | Size | Field |
|---|---|---|
| 0 | 1 | `fmt_ver` = 1 |
| 1 | 4 | `dict_id` u32 LE (0 = none) |
| 5 | 1 | `codec`: 0 raw, 1 zstd+dict, 2 zstd |
| 6 | 1 | `status`: 0 Match, 1 NoMatch, 2 Tie |
| 7 | 1 | `source`: 0 Single, 1 Batch, 2 Job, 3 Import |
| 8 | 1 | `views` bitflags: 0x01 JSON (addressMatches), 0x02 GEO (geographies), 0x04 BATCH |
| 9 | 8 | `verified_at` u64 unix secs (last status re-confirmation) |
| 17 | n | payload |

Payload = `rmp_serde::to_vec_named(EntryPayload { json: Option<Bytes> /* raw upstream addressMatches array or geographies object, byte-exact */, batch: Option<BatchView> /* match_type, matched_address, lon, lat, tiger_line_id, side, state, county, tract, block */, benchmark_name, vintage_name })`. Raw JSON bytes (not `Value`) preserve number formatting; layer filtering parses with `preserve_order` + `arbitrary_precision` at response time. If zstd output ≥ raw, store `codec=0`. Decoder keeps current + previous dictionaries keyed by `dict_id`; unknown id → treat as miss and delete.

### 5.4 View projection and fallback (`src/cache/views.rs`)

| Entry has | Request | Result |
|---|---|---|
| JSON+GEO | single `geographies` | HIT; filter to requested layers (omitted → `default_layers.json[vintage]`) |
| JSON+GEO | single `locations` | HIT; drop `geographies` and `vintage` |
| JSON only | `geographies` | MISS → upgrade fetch, merge |
| BATCH only | any single JSON request | MISS → upstream single fetch, merge (flag `serve_partial_json_from_batch` synthesizes a partial JSON with `X-UCG-Partial` header instead) |
| JSON (±GEO) | batch row | HIT; `match_type` = stored BATCH view if present, else heuristic `Exact` iff canonical input == canonical `matchedAddress` else `Non_Exact` (documented; `X-UCG-Heuristic-Count` header). Geographies columns from `2020 Census Blocks[0]` STATE/COUNTY/TRACT/BLOCK. Flag `batch_upgrade_when_idle` sends such rows upstream instead when the limiter has headroom. |
| any | NoMatch / Tie | batch: `No_Match`/`Tie` row; JSON: `addressMatches: []` (Tie JSON shape is open question §11.3) |

### 5.5 TTL policy (`src/cache/policy.rs`)

| status / source | benchmark class `Current` (names with Current/ACS/LUCA) | class `Fixed` (Census2020 etc.) |
|---|---|---|
| Match | `ttl.match_current` 180 d | `ttl.match_fixed` 0 = never |
| Tie | `ttl.tie` 30 d | same |
| NoMatch from Single | `ttl.nomatch_single` 30 d | same |
| NoMatch from Batch/Job | `ttl.nomatch_batch` 24 h **and** enqueue in `RETRY_QUEUE` at `now + retry.negative_delay` (1 h), max 3 attempts | same |

Freshness: Fresh / Stale (expired but served when upstream fails and `serve_stale_on_error`) / Expired (miss, overwrite on success).

### 5.6 Job records

```rust
struct JobRecord { id, kind: Geocode|RefreshNegatives|Warm, state: Queued|Running|Paused|Cancelled|Completed|Failed,
    return_type, search_type, benchmark_id, vintage_id, created_at, updated_at, started_at, finished_at,
    totals: Counts { total, cached, matched, no_match, tie, pending, failed }, chunk_size, chunks_total,
    original_filename, error, priority }
struct JobRow   { user_id, input: Address{..}|AddressPR{..}|Oneline{..}|Coord{x,y}, key, state: Pending|Cached|Done|Failed, status, attempts }
struct ChunkState { first_row, last_row, state: Pending|InFlight|Done|Failed, lease_until, attempts, last_error }
```

## 6. Request flows

### 6.1 Single record `GET /geocoder/{returntype}/{searchtype}`
1. Parse/validate query → unknown returntype/searchtype 404; missing `benchmark` → 400 `Benchmark must be specified`; `geographies` without `vintage` → 400 (mirror text from `err_novintage.json`); `coordinates` with `locations` → 400; `format=jsonp` requires `callback` matching `^[A-Za-z_$][\w$.]*$`.
2. Resolve benchmark/vintage ids via `Catalog`; build `CacheKey`.
3. `entries.get` → decode → freshness + view check (§5.4). Fresh → step 7, `X-Cache: HIT`.
4. Else `singleflight.join(key)`: followers await the leader's `watch` channel.
5. Leader: `throttle.acquire_single(Sync)` → `upstream.fetch_single` (`geographies&layers=all&format=json`) with retry/backoff → build payload (status from `addressMatches.len()`), `writer.put`, publish. `X-Cache: MISS`.
6. Upstream failure: stale entry + `serve_stale_on_error` → `X-Cache: STALE`; else 502/503 with Census-shaped error and `Retry-After` when breaker open. Upstream 400 bodies are forwarded verbatim.
7. Assemble `{"result":{"input":{<rebuilt from request>,"benchmark":{...},"vintage":{...}},"addressMatches":[...filtered...]}}` (or the `geographies` form for coordinates). JSONP wraps as `callback(<json>);`. Content-Type values come from the captured `*.headers` files.
8. Counters + metrics (`ucg_requests_total{endpoint,cache}`).

### 6.2 Batch `POST /geocoder/{rt}/addressbatch`, `POST /geocoder/geographies/coordinatesbatch`
1. Body limit 16 MiB; multipart fields `addressFile`/`coordinatesFile`, `benchmark`, `vintage`. CSV via `csv` crate (no headers, flexible); xls/xlsx via calamine behind feature `xlsx`.
2. >10,000 rows → 400 (mirror upstream message if capturable, else `{"errors":["Batch file exceeds 10,000 records"],"status":"400"}`). Malformed rows → 400 listing row numbers (upstream behavior unknown, §11.5).
3. Rows get an internal `idx`; user ids pass through untouched (duplicates allowed). Compute keys; one read txn partitions hits vs misses.
4. Misses chunked (`batch.chunk_size` default 2000, max 9999), processed sequentially in input order: `throttle.acquire_batch(Sync)` → multipart POST with internal `idx` as the id column → parse result CSV → store `EntryPayload { batch: Some(view) }` (NoMatch also enqueued for retry).
5. v1 response is buffered: all chunks complete, then rows streamed in input order, `Content-Type: text/csv`, `Content-Disposition: attachment; filename="GeocodeResults.csv"`, `QuoteStyle::Always`, line terminator as captured in `batch_loc.csv`. Any chunk failure after retries → 502.
6. Headers: `X-Cache: HIT` when zero misses, else `X-UCG-Cache-Hits`/`X-UCG-Cache-Misses`.

### 6.3 Async jobs (`src/jobs/worker.rs`)
1. `POST /api/v1/jobs`: multipart (same fields as batch + `returntype`, optional `searchtype`) or JSON `{returntype, benchmark, vintage, rows:[{id,street,city,state,zip}|{id,x,y}|{id,address}]}`. Rows keyed and looked up; fresh rows marked `Cached` at submit; job + rows + chunks persisted with `Durability::Immediate`. Returns `201 {id, counts}`.
2. `JobRunner` loop (`Notify` + 5 s tick): oldest runnable job → next `Pending` chunk → mark `InFlight` with lease.
3. `throttle.acquire_batch(Background)` / `acquire_single(Background)`: proceeds only when no sync waiters exist and the limiter has tokens; breaker open → sleep until `open_until`.
4. Parsed-address and coordinate rows → upstream batch; oneline rows have no batch endpoint → single-record path one by one. Optional `jobs.single_record_upgrade` re-fetches matches via single-record to fill the JSON view when idle.
5. Results → writer (entries) + `JOB_ROWS`; chunk `Done`; counts recomputed. Chunk failures retried up to 3, then rows `Failed`.
6. Job → `Completed` when no chunks pending (`Failed` only if all failed). Pause/resume/cancel checked between chunks.
7. `RefreshNegatives` kind: rows from `RETRY_QUEUE` due now (or filtered by benchmark); Match → upgrade entry and dequeue; repeat NoMatch → `verified_at` updated, attempts++; after max attempts the entry gets `nomatch_single` TTL and leaves the queue. Periodic scheduler `retry.auto_refresh_interval` (6 h, 0 = off) creates it automatically.
8. Restart: scan `JOBS` for `Running`, reset `InFlight` chunks with expired leases to `Pending`, continue.
9. `GET /api/v1/jobs/{id}/results` streams rows joined with current entries as batch CSV or JSON lines; `only=unmatched` supports Census's "resubmit unmatched" guidance.

## 7. Throttling (`src/upstream/throttle.rs`, `backoff.rs`)

```rust
pub struct Throttle {
    limiter: DefaultDirectRateLimiter,      // Quota::per_second(single_rps).allow_burst(single_burst); defaults 5 / 1
    single_sem: Semaphore,                  // single_concurrency, default 4
    batch_sem: Semaphore,                   // batch_concurrency, default 1; plus batch_min_interval 2 s between uploads
    sync_waiters: AtomicUsize, batch_sync_waiters: AtomicUsize,
    breaker: Mutex<CircuitBreaker { consecutive_failures: u32, open_until: Option<Instant> }>,
}
pub enum Priority { Sync, Background }
// Sync:       waiters += 1; sem.acquire().await; limiter.until_ready().await; waiters -= 1
// Background: loop { if sync_waiters == 0 && sem.try_acquire() ok && limiter.check() ok { break } sleep(background_poll_ms = 100) }
```

Retry: attempts 0..=3 on network error/408/429/5xx; delay = `Retry-After` (secs or HTTP-date, cap 60 s) else `retry_base_ms * 2^attempt` + jitter, cap 60 s. 400/404 never retried. Breaker: 10 consecutive transient failures → open for `breaker_open_secs` (60, doubling to a 15 min cap); any success resets. Open breaker: Sync returns stale-or-503 with `Retry-After`; Background sleeps. State visible in `/healthz`, `/api/v1/upstream`, gauge `ucg_breaker_open`. Limiter is rebuilt via `ArcSwap` when config changes at runtime.

## 8. API surface

Auth: `token` routes require `Authorization: Bearer $UCG_ADMIN_TOKEN` when the token is configured (unset → open with a startup warning; `UCG_ADMIN_REQUIRE_TOKEN=true` refuses to start without one). Geocoder routes are always public.

| Method | Path | Auth | Summary |
|---|---|---|---|
| GET | `/geocoder/{locations\|geographies}/{onelineaddress\|address\|addressPR}` | none | Census-compatible single record |
| GET | `/geocoder/geographies/coordinates` | none | geoLookup by x,y |
| POST | `/geocoder/{locations\|geographies}/addressbatch` | none | multipart → CSV |
| POST | `/geocoder/geographies/coordinatesbatch` | none | multipart → CSV |
| GET | `/geocoder/benchmarks`, `/geocoder/vintages?benchmark=` | none | proxied, 24 h TTL |
| GET | `/healthz` | none | `{status, breaker, db_ok, upstream_last_ok_at}` |
| GET | `/metrics` | token (`UCG_METRICS_PUBLIC`) | Prometheus |
| GET | `/api-docs/openapi.json` | none | utoipa spec |
| GET | `/admin/docs`, `/admin/{*path}` | token | Swagger UI, SPA |
| POST/GET | `/api/v1/jobs` | token | submit (multipart or JSON) / list (`state`, `limit`, `cursor`) |
| GET/DELETE | `/api/v1/jobs/{id}` | token | status + progress % + ETA / delete |
| POST | `/api/v1/jobs/{id}/{pause\|resume\|cancel}` | token | transitions |
| GET | `/api/v1/jobs/{id}/results?format=csv\|json&only=unmatched` | token | download |
| POST | `/api/v1/jobs/refresh-negatives` | token | `{benchmark?, vintage?, limit?}` |
| GET | `/api/v1/cache/stats` | token | counts by status/searchtype/benchmark, hit ratio, file bytes, compression ratio, redb stats |
| GET | `/api/v1/cache/entries?prefix&searchtype&benchmark&status&limit&cursor` | token | paginated listing |
| GET/DELETE | `/api/v1/cache/entries/{key}` | token | decoded entry / delete |
| POST | `/api/v1/cache/purge` | token | `{scope: all\|expired\|benchmark\|vintage\|status\|prefix, value?, dry_run?}` |
| GET | `/api/v1/cache/export?prefix&format=jsonl` | token | streaming JSON lines |
| POST | `/api/v1/cache/import` | token | CSV → `Warm` job; JSONL with payloads → direct restore |
| POST | `/api/v1/cache/compact` | token | `Database::compact()` |
| GET/POST | `/api/v1/catalog`, `/api/v1/catalog/refresh` | token | benchmarks/vintages/layers |
| GET/PUT | `/api/v1/config`, DELETE `/api/v1/config/{path}` | token | effective config with provenance; runtime overrides (ttl.*, throttle.*, batch.*, jobs.*, serve_stale, fetch_geographies_always) |
| GET | `/api/v1/upstream`, POST `/api/v1/upstream/breaker/reset` | token | limiter/in-flight/breaker/last-errors ring |

Extension for qsv: `flat=1` query param (or `X-UCG-Flatten: 1`) on single geographies requests adds `"flat": {"<layer>.GEOID": ..., ...}` (first element of each layer) because qsv's `json_dotpath` cannot index arrays.

All `/api/v1` types derive `utoipa::ToSchema`; handlers use `#[utoipa::path]` and `OpenApiRouter::routes(routes!(...)).split_for_parts()`; `SwaggerUi::new("/admin/docs").url("/api-docs/openapi.json", api)` is merged before the `/admin/{*path}` fallback. A `bearerAuth` security scheme is declared so Swagger's Authorize button works.

### 8.1 Admin UI (`admin-ui/`, rust-embed)

Vanilla JS SPA, no build step. Nav: Dashboard · Entries · Jobs · Config · Docs. Docs tab is an `<iframe src="/admin/docs">` so Swagger shares the shell. Token stored in `localStorage` after a one-time prompt. Dashboard polls stats + upstream every 5 s (entries, hit ratio, db size, compression ratio, limiter/breaker, active jobs). Entries: filters, cursor pagination, decoded-entry panel, delete. Jobs: upload form + JSON paste, progress bars, pause/resume/cancel/download. Config: form from `/api/v1/config` with provenance, PUT on save. SPA fallback to `index.html`; `prefers-color-scheme`.

### 8.2 Config reference (env ← TOML ← flags ← redb overrides; `ArcSwap<Config>`)

| Env | Default | Meaning |
|---|---|---|
| `UCG_CONFIG`, `UCG_BIND` | —, `0.0.0.0:8080` | TOML path, listen addr |
| `UCG_DB_DIR`, `UCG_CACHE_NAME`, `UCG_DB_CACHE_MB` | `./data`, `geocode-census`, 256 | store |
| `UCG_UPSTREAM_URL`, `UCG_TIGERWEB_URL`, `UCG_USER_AGENT` | census.gov URLs, `ucgeocoder/<ver> (+repo url)` | upstream |
| `UCG_UPSTREAM_TIMEOUT_SECS`, `UCG_UPSTREAM_BATCH_TIMEOUT_SECS` | 30, 600 | |
| `UCG_FETCH_GEOGRAPHIES_ALWAYS`, `UCG_SERVE_STALE_ON_ERROR` | true, true | |
| `UCG_SINGLE_RPS`, `UCG_SINGLE_BURST`, `UCG_SINGLE_CONCURRENCY` | 5, 1, 4 | throttle |
| `UCG_BATCH_CONCURRENCY`, `UCG_BATCH_MIN_INTERVAL_SECS`, `UCG_BACKGROUND_POLL_MS` | 1, 2, 100 | |
| `UCG_MAX_RETRIES`, `UCG_RETRY_BASE_MS`, `UCG_RETRY_MAX_SECS`, `UCG_BREAKER_THRESHOLD`, `UCG_BREAKER_OPEN_SECS` | 3, 1000, 60, 10, 60 | |
| `UCG_TTL_MATCH_CURRENT`, `UCG_TTL_MATCH_FIXED`, `UCG_TTL_TIE`, `UCG_TTL_NOMATCH_SINGLE`, `UCG_TTL_NOMATCH_BATCH`, `UCG_TTL_CATALOG` | 180d, 0, 30d, 30d, 24h, 24h | humantime |
| `UCG_NEGATIVE_RETRY_DELAY`, `UCG_NEGATIVE_RETRY_ATTEMPTS`, `UCG_NEGATIVE_AUTO_REFRESH` | 1h, 3, 6h (0 = off) | |
| `UCG_BATCH_CHUNK_SIZE`, `UCG_BATCH_MAX_ROWS`, `UCG_BATCH_UPGRADE_WHEN_IDLE`, `UCG_SERVE_PARTIAL_JSON_FROM_BATCH` | 2000, 10000, false, false | |
| `UCG_JOBS_CHUNK_SIZE`, `UCG_JOBS_CHUNK_MAX_ATTEMPTS`, `UCG_JOBS_LEASE_SECS`, `UCG_JOBS_SINGLE_RECORD_UPGRADE` | 2000, 3, 900, false | |
| `UCG_MAX_BODY_MB`, `UCG_ADMIN_TOKEN`, `UCG_ADMIN_REQUIRE_TOKEN`, `UCG_METRICS_PUBLIC` | 16, —, false, false | |
| `UCG_WRITER_FLUSH_MS`, `UCG_WRITER_MAX_BATCH`, `UCG_DURABLE_COMMIT_SECS`, `UCG_HITCOUNT_FLUSH_SECS` | 50, 500, 30, 60 | writer |
| `UCG_LOG`, `UCG_LOG_FORMAT`, `UCG_LIVE_TESTS` | info, text, — | |

CLI: `ucgeocoder serve` (default) · `stats [--json]` · `purge --scope … [--dry-run]` · `export [--prefix] -o f.jsonl` · `import f.{csv,jsonl}` · `compact` · `catalog refresh|show` · `train-dict --samples DIR -o resources/census.dict` · `check-config`. Offline subcommands open the db directly and fail fast if the server holds the lock.

## 9. Implementation phases

Each phase: implement, then `cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test` green, update §13, one commit. "Done when" lines are the acceptance criteria.

1. **Skeleton + config + telemetry.** Creates `Cargo.toml`, `src/{main,lib,cli,config,error,state,telemetry}.rs`, `/healthz`, `Dockerfile`, `.github/workflows/ci.yml`, `ucgeocoder.example.toml`, `docs/PLAN.md`. Done when: `ucgeocoder serve` binds and answers `/healthz`; `check-config` prints effective config with provenance; config precedence unit tests (default < TOML < env < flag) pass; `assert_cmd` smoke test passes; CI workflow runs locally via `act` or is at least syntactically valid.
2. **Catalog + key + envelope.** Creates `catalog.rs`, `cache/{key,codec,envelope,policy}.rs`, `dict/train.rs`, `resources/{catalog_snapshot,default_layers,tigerweb_layers}.json` seeded from `tests/fixtures/live`, and `resources/census.dict` trained from ~500 live samples (write a one-off throttled capture script under `scripts/`, 2 req/s, varied addresses from several states plus NoMatch and coordinates; gitignore `resources/samples/`). Done when: alias→id tests, key grammar round-trip, coordinate rounding, exact envelope byte-layout test, raw fallback, unknown-dict-id handling, and a cached-compat test that writes an entry with our code and reads it back via dev-dep `cached::RedbCache::<String, Vec<u8>>::builder("geocode-census").disk_dir(dir).build()` all pass. Run Serena onboarding at the end of this phase.
3. **Store + writer.** Creates `store/*.rs`. Done when: concurrent put/get test, prefix-scan ordering test, restart-persistence test, compact test, stats-flush test pass.
4. **Upstream + throttle + backoff.** Creates `upstream/{mod,throttle,backoff,single,catalog}.rs`. Done when wiremock tests pass for: 429 with `Retry-After`; 503 backoff timing with `UCG_RETRY_BASE_MS=1`; breaker opens after 10 failures and resets on success; a `Background` acquire does not proceed while a `Sync` waiter exists (assert ordering with a paused mock).
5. **Single-record endpoints.** Creates `census/{types,params,response,layers}.rs`, `cache/{views,singleflight}.rs`, `http/{mod,geocoder}.rs`. Done when: golden tests against `tests/fixtures/live` pass, byte-exact for the passthrough segments (`addressMatches` / `geographies` raw bytes) and structural (parsed with `arbitrary_precision`) for the rebuilt `result.input` envelope (the Census PDF says clients must not assume whitespace/format); locations-from-geographies projection test; JSONP test; N concurrent identical misses produce exactly 1 upstream call; `X-Cache` HIT/MISS/STALE tests; `/benchmarks` and `/vintages` proxy with 24 h TTL.
6. **Batch endpoints.** Creates `census/batch_csv.rs`, `upstream/batch.rs`, `http/batch.rs`. Done when: input order preserved with shuffled upstream output; duplicate user ids pass through; No_Match and Tie rows reproduced; mixed hit/miss stitching; >10k rejected; batch No_Match populates `RETRY_QUEUE`; output quoting and terminator match `batch_loc.csv` byte-for-byte for the captured rows.
7. **Jobs.** Creates `jobs/*.rs`, `http/api_jobs.rs`. Done when: submit (multipart and JSON) dedupes cached rows; worker drains under the limiter; pause/resume/cancel; results download (csv/json, `only=unmatched`); kill-and-reopen harness shows a `Running` job resuming; refresh-negatives upgrades a NoMatch entry when the mock later matches; scheduler creates the job on interval.
8. **Admin API + OpenAPI + auth + metrics.** Creates `http/{api_cache,api_admin,auth,metrics,openapi}.rs`. Done when: `openapi.json` snapshot test; 401 without token and 200 with; every purge scope tested incl. `dry_run`; export/import round trip; `/metrics` exposes `ucg_requests_total` and `ucg_breaker_open`; `PUT /api/v1/config` changes the live limiter.
9. **Admin UI.** Creates `admin-ui/*`, `http/admin_ui.rs`. Done when: asset routes 200, SPA fallback serves `index.html`, `/admin/docs` serves Swagger with the vendored assets (no network), and a manual walkthrough of all five tabs works against a running server.
10. **Hardening.** Creates `benches/hot_path.rs`, `tests/live.rs` (gated by `UCG_LIVE_TESTS=1`, serial, throttled), README (usage, config, qsv compat, redb lock note, rate-limit disclaimer), release workflow. Done when: criterion shows HIT path p50 < 200 µs on a warm db; live suite resolves every §11 item it can and records answers in §11; README complete.

## 10. Verification (end-to-end, after Phase 10)

1. `ucgeocoder serve` with an empty `./data`.
2. Run the three curl examples from the Census API PDF against `http://localhost:8080/geocoder/...`: JSON must be structurally equal to the live fixtures with passthrough segments byte-identical; batch CSV rows identical. First call `X-Cache: MISS`, second `HIT`.
3. `curl -s localhost:8080/api/v1/cache/stats` shows the entries; `/admin/` dashboard shows the same.
4. Submit a 50-row job via Swagger at `/admin/docs`, watch it drain at ≤ 5 req/s in `/api/v1/upstream`, download results.
5. Stop the server mid-job, restart, confirm the job resumes.
6. Stop the server; from a scratch Rust program (or the Phase 2 compat test), open `./data/geocode-census_v3.redb` with `cached::RedbCache` and read one key.

## 11. Open questions (resolve from fixtures / live suite; record answers here)

1. Default layer subset when `layers` omitted: seed `default_layers.json` for `Current_Current` from `geo_coords_default.json` / `geo_address_default.json` (11 groups observed, listed in §0.2) plus the PDF's named default set (also Incorporated Places, Urbanized Areas, Urban Clusters); capture other vintages live. Semantics: a filter over stored layers, not a guaranteed list (Incorporated Places vs CDPs are mutually exclusive, so only one appears in any given response).
2. TIGERweb MapServer per vintage: derive `tigerWMS_<suffix>` from `vintageName` (Current → `tigerWMS_Current`, ACS2026 → `tigerWMS_ACS2026`, Census2020 → `tigerWMS_Census2020`), snapshot fallback, admin override.
3. Tie semantics in JSON (multiple `addressMatches`?) and the batch `Tie` row column set: store `Tie` only from batch until confirmed.
4. Byte fidelity depends on raw passthrough + `arbitrary_precision` + `preserve_order`; golden tests are byte-exact only for passthrough segments.
5. Exact Content-Types, CSV line terminator, and 400 texts for missing vintage / >10k rows / malformed rows: read from the captured `*.headers` and `err_*.json` files; capture a >10k and a malformed-row case in the live suite.
6. `Public_AR_Current` data drifts twice a year behind the same id: class-aware TTL + refresh-negatives + a documented `purge --scope benchmark` runbook.
7. Rate-limit defaults are guesses (nothing published anywhere); backoff + breaker are the safety net.
8. redb exclusive lock vs qsv offline tooling: documented.
9. Oneline rows in jobs cannot use upstream batch: slow path, documented.
10. `metrics-exporter-prometheus` / `wiremock` versions: verify at `cargo add`.
11. Dictionary must be trained across vintages/layers/NoMatch/coordinates; retrain (new `dict_id`) when Census renames layers (e.g. the yearly "2026 State Legislative Districts" rename).

## 12. qsv integration sketch (later PR in `~/GitHub/qsv`; not part of this repo's phases)

Findings from exploring qsv on 2026-10-12 (`src/cmd/geocode.rs`, 5655 lines; `src/cmd/geocode/geoclient.rs`, 1576 lines):
- Online backends are subcommand pairs (`opencage`/`opencagenow`, `geoclient`/`geoclientnow`); there is no `--backend` option. Geoclient is the template: async reqwest client with `util::set_user_agent`, `governor` limiter from `--rate-limit`, `cached::RedbCache<String,String>` disk cache named `geocode-geoclient` with `--cache-ttl` (default 14 d) and `--no-cache`, cache key `"{CACHE_KEY_VERSION}|{base_url}|{endpoint}|{sorted params}"`, value encoding `F<json>` / `R<json>` / `N`, retries MAX_RETRIES=3 with `Retry-After`, CIRCUIT_BREAKER_THRESHOLD=10, base URL override via `QSV_GEOCLIENT_API_URL`.
- Integration touchpoints in `src/cmd/geocode.rs`: `mod census;` next to `mod geoclient;` (~line 777); usage lines (~441–460); `Args` `cmd_*` fields and `enum GeocodeSubCmd` (~1153); validation in `run()` (~1175); early-return dispatch in `geocode_main` (~1410–1420) mirroring `geoclient::run_geoclient(args, now_cmd, &cache_dir)`; provider list in `run_cache_mgmt` (~2515–2518); `docs/ENVIRONMENT_VARIABLES.md` (~64–70). `docs/help/geocode.md` is generated from the USAGE text. Tests in `tests/test_geocode.rs` use an in-process actix-web mock pointed at by the base-URL env var.
- `json_dotpath` (~3839) walks object keys only, so `geographies.<layer>[0].GEOID` cannot resolve; hence the server's `flat=1` extension.

Proposed qsv design: `src/cmd/geocode/census.rs`; subcommands `qsv geocode census [--formatstr=<string>] [options] <column> [<input>]` and `censusnow [options] <location>`; env `QSV_CENSUS_GEOCODER_URL` (default `https://geocoding.geo.census.gov/geocoder`, so qsv works without the proxy; URL is part of the cache key); options `--returntype`, `--benchmark`, `--vintage`, `--layers`, `--searchtype oneline|address|coordinates`, `--address-columns street,city,state,zip`; files ≥ 50 rows use `addressbatch` chunks mapped back by internal row id; formatstr presets `%lat-long`, `%location`, `%matched-address`, `%json`, and `%census-geoids` expanding to `%dyncols:` over `flat.<layer>.GEOID` (client-side flatten when talking to census.gov directly). When pointed at `ucgeocoder`, qsv may default to `--no-cache` semantics since the proxy is the shared cache.

## 13. Progress log (implementer updates this)

| Phase | Status | Commit | Notes |
|---|---|---|---|
| 0 Day-1 | done | (this commit) | cargo 1.99.0; Serena already `[rust]`; fixtures recaptured 2026-10-12 and match §0.2. New facts: batch responses are `Content-Type: text/plain` with `Content-Disposition: attachment; filename=GeocodeResults.csv` (unquoted), `\n` terminator; JSONP body is `/**/cb(<json>);` with `application/javascript`; missing vintage → 400 `{"errors":["Vintage must be specified"],"status":"400"}`. TIGERweb: 80 layers, 40 non-Labels. |
| 1 | not started | | |
| 2 | not started | | |
| 3 | not started | | |
| 4 | not started | | |
| 5 | not started | | |
| 6 | not started | | |
| 7 | not started | | |
| 8 | not started | | |
| 9 | not started | | |
| 10 | not started | | |
