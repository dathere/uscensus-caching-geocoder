# When a task/phase is done

1. `cargo fmt && cargo fmt --check`
2. `cargo clippy --all-targets --all-features -- -D warnings`
3. `cargo test`
4. Update `docs/PLAN.md` §13 Progress log (status, commit hash of the previous phase, notes on deviations/new facts).
5. Stage files by name (no `git add -A`), one commit per phase, no session links in messages.
