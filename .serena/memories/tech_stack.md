# Tech stack

- Rust edition 2024, rust-version 1.99 (toolchain 1.99.0 locally).
- axum 0.8 + tokio, clap 4 (derive), serde_json with `preserve_order` + `arbitrary_precision` + `raw_value` (needed for byte-faithful passthrough), toml, humantime, arc-swap, thiserror/anyhow, tracing.
- Store: redb 4.3 used directly (same redb version as dev-dep `cached` 4.0.2 — keep them aligned or the compat test breaks). rmp-serde + serde_bytes for values.
- zstd 0.14 (bulk API, prepared dictionaries); frame content size is relied on for decompression capacity.
- Later phases add: reqwest 0.13, governor, utoipa 6 / utoipa-axum / utoipa-swagger-ui, rust-embed, metrics, wiremock, criterion, calamine (feature `xlsx`, currently an empty placeholder feature).
- `cargo add` quirk seen here: it misreported serde_json features; edit Cargo.toml directly if feature flags are rejected.
- Edition 2024: `std::env::set_var` is unsafe — config loading takes env as an injected closure; tests never mutate process env.
