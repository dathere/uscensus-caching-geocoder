# Commands

- Run: `cargo run -- serve --bind 127.0.0.1:8080` (default subcommand is `serve`).
- Effective config with provenance: `cargo run -- check-config [--json] [--set key=value ...]`.
- Retrain dictionary: `python3 -I scripts/capture.py samples` (≈4 min, 2 req/s, live census.gov) then `cargo run -- train-dict --samples resources/samples -o resources/census.dict`.
- Refresh catalog resources: `python3 -I scripts/capture.py catalog`.
- Regenerate example config after adding keys: build the key list from `check-config --json` (see how `ucgeocoder.example.toml` is laid out); test `example_toml_is_complete_and_matches_defaults` enforces it.
- Workflow lint: `actionlint .github/workflows/ci.yml` (installed via Homebrew). `cargo-deny`, `act`, Docker are not installed locally.
- Darwin: BSD `sed` needs `sed -i ''`.
