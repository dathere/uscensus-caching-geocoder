# Conventions

- New config keys: add one line to the `define_config!` table in `src/config.rs` (doc comment, type, default string, `UCG_*` env) and to `ucgeocoder.example.toml`. Defaults are strings parsed by `ConfigValue`; `Duration` uses humantime and `0` = `Duration::ZERO` = never/off.
- Errors: library modules use `thiserror` enums; CLI/top-level uses `anyhow`. HTTP errors render Census-shaped `{"errors":[...],"status":"NNN"}` via `error::AppError`.
- Tests: unit tests in-module; integration tests in `tests/` call the binary via `env!("CARGO_BIN_EXE_ucgeocoder")` with `env_clear()`, never bind fixed ports (`--bind 127.0.0.1:0`, server logs `listening addr=...`).
- Fixtures are read from `tests/fixtures/live/` via `CARGO_MANIFEST_DIR`/`include_bytes!`; compare passthrough JSON byte-exact, rebuilt envelopes structurally.
- No blanket `#[allow(dead_code)]`; keep `clippy -D warnings` clean.
- Commits: one per PLAN phase; `Co-Authored-By` trailer allowed, never a session link.
