# uscensus-caching-geocoder

`ucgeocoder` is a caching, rate-limited, wire-compatible proxy for the free
[US Census Geocoder](https://geocoding.geo.census.gov/geocoder/). It speaks the Census API
unchanged, so any client (qsv, curl, R and Python packages) can use it by changing the base
URL from `https://geocoding.geo.census.gov/geocoder` to `http://<host>:8080/geocoder`.

- **Fast repeats.** Answers are kept in an embedded [redb](https://github.com/cberner/redb)
  database. A cache hit takes about 20–30 µs end to end in-process (median, Apple M4 Max,
  `cargo bench`).
- **Polite upstream use.** One deployment never exceeds a conservative request rate
  (5 single-record requests/s, one batch upload at a time), with retries, `Retry-After`
  handling and a circuit breaker.
- **Batch and background jobs.** Batch uploads send only uncached rows upstream. Durable
  background jobs grow the cache under the same rate limit and resume after a restart.
- **Admin UI, API and metrics.** An embedded web UI, an OpenAPI-documented admin API,
  Prometheus metrics and offline maintenance commands.

> **Rate-limit disclaimer.** Census publishes no numeric rate limit. The defaults here are
> deliberately conservative guesses, not a documented allowance; heavy or shared-IP users
> can still be throttled or blocked by Census. Backoff and the circuit breaker are the
> safety net. Please keep the defaults unless you have a reason not to.

## Quick start

Requires Rust 1.99 or later.

```sh
cargo build --release
./target/release/ucgeocoder serve            # listens on 0.0.0.0:8080, data in ./data
```

Then use it exactly like Census:

```sh
curl 'http://localhost:8080/geocoder/locations/onelineaddress?address=4600+Silver+Hill+Rd%2C+Washington%2C+DC+20233&benchmark=Public_AR_Current&format=json'
curl 'http://localhost:8080/geocoder/geographies/coordinates?x=-76.92744&y=38.84601&benchmark=4&vintage=4&format=json'
curl --form addressFile=@addresses.csv --form benchmark=4 \
  http://localhost:8080/geocoder/locations/addressbatch
```

The first lookup of an address answers `X-Cache: MISS` (fetched upstream); repeating it
answers `X-Cache: HIT` from the cache.

Set an admin token before exposing the server:

```sh
UCG_ADMIN_TOKEN=$(openssl rand -hex 32) ./target/release/ucgeocoder serve
```

Without a token the admin API is open to anyone who can reach the server (a warning is
logged at startup); `UCG_ADMIN_REQUIRE_TOKEN=true` refuses to start without one.

## Census-compatible endpoints

| Method | Path | Notes |
|---|---|---|
| GET | `/geocoder/{locations\|geographies}/{onelineaddress\|address\|addressPR}` | single record |
| GET | `/geocoder/geographies/coordinates` | coordinates lookup |
| POST | `/geocoder/{locations\|geographies}/addressbatch` | multipart `addressFile`, `benchmark`, `vintage` |
| POST | `/geocoder/geographies/coordinatesbatch` | multipart `coordinatesFile`, `benchmark`, `vintage` |
| GET | `/geocoder/benchmarks`, `/geocoder/vintages?benchmark=` | proxied, cached 24 h |

Fidelity. These paths, parameters and responses follow Census; the details are pinned by
golden tests against responses captured from census.gov (`tests/fixtures/live/`).

- **Single-record JSON.** The upstream `addressMatches`/`geographies` segments are served
  byte-for-byte. The surrounding `result.input` is rebuilt from your request.
  `format=jsonp&callback=` works.
- **Batch output.** CSV rows match Census byte-for-byte: no header, every field quoted,
  `\n` line endings, `text/plain` with `Content-Disposition: attachment; filename=GeocodeResults.csv`.
- **Batch errors.** Errors match too: Census's `text/plain` bodies for bad benchmark/vintage
  and for files over 10,000 rows. An empty file gets an empty 200, and columns beyond the
  expected width are ignored.
- **Not mirrored on purpose.** A missing file gets a JSON 400 (Census serves its HTML form
  there). Broken CSV quoting gets a 400 naming the rows (Census answers 500).
- **Single-record errors.** Requests the proxy cannot key (unknown benchmark/vintage,
  missing fields, `format` other than `json`/`jsonp`) are passed through to Census uncached
  and answered verbatim with `X-Cache: BYPASS`.

### Response headers

| Header | Meaning |
|---|---|
| `X-Cache: HIT` | answered from the cache (batch: every row) |
| `X-Cache: MISS` | fetched upstream and stored |
| `X-Cache: STALE` | upstream failed; an expired entry was served (`cache.serve_stale_on_error`) |
| `X-Cache: BYPASS` | not cacheable; forwarded to Census as is |
| `X-UCG-Cache-Hits` / `X-UCG-Cache-Misses` | batch rows answered from cache / upstream |
| `X-UCG-Heuristic-Count` | batch rows synthesized from single-record entries (match type guessed, see below) |

### Extension: `flat=1`

For clients that cannot index JSON arrays (qsv's `json_dotpath`), add `flat=1` (or the
header `X-UCG-Flatten: 1`) to a single-record geographies request. The response then gains
`result.flat.<layer>.<attribute>`: the scalar attributes of the first feature of each layer,
e.g. `result.flat."Census Tracts".GEOID`.

## How caching works

- **One fetch serves every view.** Each single-record lookup fetches upstream once with
  `returntype=geographies&layers=all`. The resulting entry answers `locations`,
  `geographies` with any `layers` subset (an omitted `layers` gives the vintage's default
  set) and batch rows. `cache.fetch_geographies_always=false` fetches only what was asked.
- **Keys.** Keys are canonicalized: whitespace collapsed, uppercased, trailing `.`/`,`
  stripped, aliases resolved to ids. Coordinates are rounded to 6 decimals. One-line and
  parsed addresses are cached separately. A `locations` lookup shares its entry with
  `geographies` on the benchmark's default vintage.
- **Batch rows.** Rows share entries with single records. Only rows that miss go upstream,
  deduplicated and chunked (`batch.chunk_size`).
  - A row answered from a single-record entry gets a heuristic match type: `Exact` when the
    canonical input equals the matched address, else `Non_Exact`. Those rows are counted in
    `X-UCG-Heuristic-Count`.
  - A batch No_Match never answers a single lookup: Census's batch matching is
    load-dependent, so single lookups ask again.
- **Lifetimes.**

  | Answer | Current-class benchmarks (Current, ACS, LUCA) | Fixed (e.g. Census2020) |
  |---|---|---|
  | Match | 180 days | never expires |
  | Tie | 30 days | 30 days |
  | No_Match from a single lookup | 30 days | 30 days |
  | No_Match from a batch or job | 24 h, and re-checked | same |

- **Re-checking No_Match.** Batch and job No_Match answers are queued for re-checking an
  hour later (`retry.negative_delay`). The `refresh-negatives` job does this every 6 h
  (`retry.auto_refresh_interval`) or on demand. After `retry.negative_attempts` (3) repeated
  No_Match answers, the entry is kept as a confirmed No_Match for 30 days.
- **Compression.** Entries are compressed with zstd and a dictionary trained on Census
  responses (about 6× smaller).

## Background jobs

`POST /api/v1/jobs` accepts the same files as the batch endpoints (multipart `addressFile`
or `coordinatesFile` plus `returntype`, `benchmark`, `vintage`, optional
`searchtype=onelineaddress|coordinates` and `priority`), or JSON:

```json
{"returntype": "locations", "benchmark": "Public_AR_Current",
 "rows": [{"id": 1, "street": "4600 Silver Hill Rd", "city": "Washington", "state": "DC", "zip": "20233"}]}
```

Rows may also be `{"id", "x", "y"}` or `{"id", "address"}`, one shape per job. Rows already
cached are answered at submit.

- **How it runs.** A single background worker drains the rest at the lowest priority. It
  yields to live requests and never exceeds the shared rate limit. Each chunk's answers and
  progress commit in one transaction, and a job interrupted by a restart resumes where it
  stopped.
- **Oneline rows are slow.** Census has no one-line batch endpoint, so those rows go one
  at a time at the single-record rate.
- **Results.** `GET /api/v1/jobs/{id}/results?format=csv|json&only=unmatched` joins the
  rows with the cache as it is when you download. A later refresh-negatives upgrade shows
  up; a row whose entry was purged keeps only its recorded status (a Match then has empty
  details). Download promptly if you need a fixed record.
- **Control.** Jobs can be paused, resumed, cancelled and deleted.

## Admin UI, API and metrics

| Path | Auth | |
|---|---|---|
| `/admin/` | public shell | Dashboard, Entries, Jobs, Config and Docs tabs; asks for the token once (kept in the browser) |
| `/admin/docs` | public | Swagger UI (bundled; no external requests) |
| `/api-docs/openapi.json` | public | OpenAPI 3.1 document |
| `/api/v1/...` | bearer token | jobs, cache stats/entries/purge/export/import/compact, catalog, runtime config, upstream state |
| `/metrics` | bearer token, unless `admin.metrics_public` | Prometheus: `ucg_requests_total{endpoint,cache}`, `ucg_breaker_open`, cache and queue gauges |
| `/healthz` | public | `{status, breaker, db_ok, upstream_last_ok_at}` |

Runtime config. `PUT /api/v1/config` with `{"throttle.single_rps": "2"}` changes a setting
immediately, including rebuilding the rate limiter. The change persists across restarts
until removed with `DELETE /api/v1/config/{key}`. Only the `cache`, `ttl`, `retry`,
`batch`, `jobs` and `throttle` sections can be changed this way.

## Offline commands

These open the database directly. They fail fast with "locked … is the server running?"
while the server holds it, so stop the server first.

```sh
ucgeocoder stats [--json] [--full]
ucgeocoder purge --scope all|expired|benchmark|vintage|status|prefix [--value V] [--dry-run]
ucgeocoder export [--prefix 'v1|address|4|'] -o cache.jsonl      # -o - for stdout
ucgeocoder import cache.jsonl                                     # restore an export
ucgeocoder import rows.csv --benchmark 4 [--returntype geographies --vintage 4]   # queue a warming job
ucgeocoder compact
ucgeocoder catalog show|refresh [--json]
ucgeocoder check-config [--json]
```

- **Export format.** Exports are decoded JSON lines, so they import into any build. An
  import keeps each entry's original age and never replaces a newer entry.
- **CSV import.** Importing a CSV creates a cache-warming job; it runs when the server
  next starts.
- **Over HTTP.** The same operations exist on `/api/v1/cache/*`. The HTTP import is
  limited by `server.max_body_mb`; the command-line import is not.

### Runbook: Census data refresh

`Public_AR_Current` and the ACS benchmarks change twice a year behind the same ids.
Matches expire after `ttl.match_current` (180 days) on their own. To start fresh at once
after a release:

```sh
ucgeocoder purge --scope benchmark --value Public_AR_Current --dry-run   # how many
ucgeocoder purge --scope benchmark --value Public_AR_Current
ucgeocoder catalog refresh        # pick up renamed vintages
ucgeocoder compact
```

To drop only one vintage, use `--scope vintage --value 4:Current_Current`. To drop only
answers past their lifetime, use `--scope expired`.

## The database file and qsv

Data lives in `<store.db_dir>/<store.cache_name>_v3.redb`, by default
`./data/geocode-census_v3.redb`. The entries table follows the on-disk convention of the
[`cached`](https://crates.io/crates/cached) crate's `RedbCache`, so qsv and other `cached`
tooling can read it offline. Each value is the compressed envelope described in
`docs/PLAN.md` §5.3.

redb holds an exclusive lock: such tools (and the offline commands above) can open the file
only while the server is stopped.

For qsv today, point any Census-based client at the proxy's base URL. A native
`qsv geocode census` backend, with `flat=1` support for `%dyncols`, is planned as a separate
qsv change (see `docs/PLAN.md` §12).

## Configuration

Precedence, lowest to highest: built-in defaults, a TOML file (`--config` or `UCG_CONFIG`),
`UCG_*` environment variables, command-line flags (`--bind`, `--db-dir`, `--log`, and
`--set key=value` for any key), then runtime overrides from the admin API.

- `ucgeocoder check-config` prints the effective value and source of every key.
- `ucgeocoder.example.toml` lists them all with their defaults.
- Durations use humantime (`90s`, `24h`, `180d`); `0` means never/off.

| Key | Env | Default | Meaning |
|---|---|---|---|
| `server.bind` | `UCG_BIND` | `0.0.0.0:8080` | Listen address. |
| `server.max_body_mb` | `UCG_MAX_BODY_MB` | `16` | Maximum request body size (batch uploads), MiB. |
| `store.db_dir` | `UCG_DB_DIR` | `./data` | Directory holding `<cache_name>_v3.redb`. |
| `store.cache_name` | `UCG_CACHE_NAME` | `geocode-census` | Cache name; file is `<cache_name>_v3.redb` (cached-compatible). |
| `store.db_cache_mb` | `UCG_DB_CACHE_MB` | `256` | redb page cache size, MiB. |
| `store.writer_flush_ms` | `UCG_WRITER_FLUSH_MS` | `50` | Writer batching window, ms. |
| `store.writer_max_batch` | `UCG_WRITER_MAX_BATCH` | `500` | Maximum puts per write transaction. |
| `store.durable_commit_secs` | `UCG_DURABLE_COMMIT_SECS` | `30` | Interval between durable (fsync) commits, seconds. |
| `store.hitcount_flush_secs` | `UCG_HITCOUNT_FLUSH_SECS` | `60` | Interval for flushing in-memory hit counters, seconds. |
| `upstream.url` | `UCG_UPSTREAM_URL` | `https://geocoding.geo.census.gov/geocoder` | Census Geocoder base URL. |
| `upstream.tigerweb_url` | `UCG_TIGERWEB_URL` | `https://tigerweb.geo.census.gov/arcgis/rest/services/TIGERweb` | TIGERweb REST services base URL. |
| `upstream.user_agent` | `UCG_USER_AGENT` | `ucgeocoder/<version> (+repo URL)` | User-Agent sent upstream. |
| `upstream.timeout_secs` | `UCG_UPSTREAM_TIMEOUT_SECS` | `30` | Single-record request timeout, seconds. |
| `upstream.batch_timeout_secs` | `UCG_UPSTREAM_BATCH_TIMEOUT_SECS` | `600` | Batch upload timeout, seconds. |
| `cache.fetch_geographies_always` | `UCG_FETCH_GEOGRAPHIES_ALWAYS` | `true` | Always fetch `geographies&layers=all` upstream and serve every view from it. |
| `cache.serve_stale_on_error` | `UCG_SERVE_STALE_ON_ERROR` | `true` | Serve expired entries (`X-Cache: STALE`) when upstream fails. |
| `throttle.single_rps` | `UCG_SINGLE_RPS` | `5` | Single-record requests per second. |
| `throttle.single_burst` | `UCG_SINGLE_BURST` | `1` | Single-record burst size. |
| `throttle.single_concurrency` | `UCG_SINGLE_CONCURRENCY` | `4` | Concurrent single-record requests. |
| `throttle.batch_concurrency` | `UCG_BATCH_CONCURRENCY` | `1` | Concurrent batch uploads. |
| `throttle.batch_min_interval_secs` | `UCG_BATCH_MIN_INTERVAL_SECS` | `2` | Minimum gap between batch uploads, seconds. |
| `throttle.background_poll_ms` | `UCG_BACKGROUND_POLL_MS` | `100` | Background (job) acquire poll interval, ms. |
| `throttle.max_retries` | `UCG_MAX_RETRIES` | `3` | Retries on transient upstream errors. |
| `throttle.retry_base_ms` | `UCG_RETRY_BASE_MS` | `1000` | Exponential backoff base, ms. |
| `throttle.retry_max_secs` | `UCG_RETRY_MAX_SECS` | `60` | Backoff / Retry-After cap, seconds. |
| `throttle.breaker_threshold` | `UCG_BREAKER_THRESHOLD` | `10` | Consecutive transient failures that open the breaker. |
| `throttle.breaker_open_secs` | `UCG_BREAKER_OPEN_SECS` | `60` | Initial breaker open period, seconds (doubles up to 15 min). |
| `ttl.match_current` | `UCG_TTL_MATCH_CURRENT` | `180d` | Match on a `Current`-class benchmark. |
| `ttl.match_fixed` | `UCG_TTL_MATCH_FIXED` | `0` | Match on a `Fixed`-class benchmark (e.g. Census2020). |
| `ttl.tie` | `UCG_TTL_TIE` | `30d` | Tie. |
| `ttl.nomatch_single` | `UCG_TTL_NOMATCH_SINGLE` | `30d` | NoMatch from a single-record lookup. |
| `ttl.nomatch_batch` | `UCG_TTL_NOMATCH_BATCH` | `24h` | NoMatch from a batch or job (also queued for retry). |
| `ttl.catalog` | `UCG_TTL_CATALOG` | `24h` | Benchmarks / vintages / layers catalog. |
| `retry.negative_delay` | `UCG_NEGATIVE_RETRY_DELAY` | `1h` | Delay before a batch NoMatch is retried. |
| `retry.negative_attempts` | `UCG_NEGATIVE_RETRY_ATTEMPTS` | `3` | Maximum retry attempts per NoMatch entry. |
| `retry.auto_refresh_interval` | `UCG_NEGATIVE_AUTO_REFRESH` | `6h` | Interval of the automatic refresh-negatives job (`0` = off). |
| `batch.chunk_size` | `UCG_BATCH_CHUNK_SIZE` | `2000` | Rows per upstream batch upload. |
| `batch.max_rows` | `UCG_BATCH_MAX_ROWS` | `10000` | Maximum rows accepted per request. |
| `batch.upgrade_when_idle` | `UCG_BATCH_UPGRADE_WHEN_IDLE` | `false` | Send heuristically-matched rows upstream when the limiter is idle. |
| `batch.serve_partial_json_from_batch` | `UCG_SERVE_PARTIAL_JSON_FROM_BATCH` | `false` | Synthesize partial single-record JSON from batch-only entries. |
| `jobs.chunk_size` | `UCG_JOBS_CHUNK_SIZE` | `2000` | Rows per job chunk. |
| `jobs.chunk_max_attempts` | `UCG_JOBS_CHUNK_MAX_ATTEMPTS` | `3` | Attempts per chunk before its rows are marked failed. |
| `jobs.lease_secs` | `UCG_JOBS_LEASE_SECS` | `900` | In-flight chunk lease, seconds. |
| `jobs.single_record_upgrade` | `UCG_JOBS_SINGLE_RECORD_UPGRADE` | `false` | Re-fetch batch matches via single-record when idle to fill the JSON view. |
| `admin.token` | `UCG_ADMIN_TOKEN` | `—` | Bearer token for `/api/v1`, `/admin`, `/metrics`; unset = open. |
| `admin.require_token` | `UCG_ADMIN_REQUIRE_TOKEN` | `false` | Refuse to start without a token. |
| `admin.metrics_public` | `UCG_METRICS_PUBLIC` | `false` | Serve `/metrics` without a token. |
| `log.level` | `UCG_LOG` | `info` | tracing-subscriber EnvFilter directive. |
| `log.format` | `UCG_LOG_FORMAT` | `text` | `text` or `json`. |

## Docker

```sh
docker build --build-arg UCG_GIT_SHA=$(git rev-parse --short=12 HEAD) -t ucgeocoder .
docker run -p 8080:8080 -v ucg-data:/data -e UCG_ADMIN_TOKEN=... ucgeocoder
```

The Dockerfile (static musl build on a distroless image) has **not been built or tested
yet**. In particular, the default TLS backend (aws-lc-rs) may need extra build tools on
musl. Prefer the release binaries or `cargo build --release` until it is verified.

## Development

```sh
cargo fmt --check && cargo clippy --all-targets --all-features -- -D warnings && cargo test
cargo test --features xlsx                      # .xls/.xlsx batch uploads
UCG_LIVE_TESTS=1 cargo test --test live         # against census.gov (serial, throttled)
cargo bench --bench hot_path                    # cache-hit latency; read the median
```

- **Live tests.** These compare Census's current answers with the captured fixtures, and
  the proxy's answers with Census's. `UCG_LIVE_RECORD=1` refreshes the fixtures.
- **Compression dictionary.** `scripts/capture.py samples` (throttled) followed by
  `ucgeocoder train-dict --samples resources/samples` retrains `resources/census.dict`. The
  previous dictionary is kept for decoding.
- **Design.** `docs/PLAN.md` holds the design, the decisions and the progress log.

## License

AGPL-3.0-only. See [LICENSE](LICENSE).
