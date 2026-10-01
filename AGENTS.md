# AGENTS.md

Rules for AI agents and humans working on sofka. `docs/architecture.md` explains the module layout; this file is about how to change the repo without making a mess.

## Local commands

Use the `Justfile`; it matches CI exactly.

```sh
just check          # fmt-check + clippy (-D warnings) + cargo test
just fmt            # cargo fmt --all
just clippy         # cargo clippy --locked --all-targets -- -D warnings
just test           # cargo test --locked
just run <resource> # cargo run -- <resource> against the current kube context
just smoke          # headless connectivity check (cargo run -- --check)
just snapshot <res> # render one headless UI snapshot (cargo run -- <res> --snapshot)
just coverage       # HTML coverage report (cargo llvm-cov --locked --html)
just build          # debug binary; just build-release for the release binary
```

CI in `.github/workflows/ci.yaml` runs on pull requests only. The `rust` job runs `cargo fmt --all -- --check`, `cargo clippy --locked --all-targets -- -D warnings`, and `cargo test --locked`. The `coverage` job runs `cargo llvm-cov --locked --html`. Both jobs must pass before a PR is ready. The locked checks reject dependency changes without a matching `Cargo.lock` update. Regenerate the lockfile with Cargo and commit it with the manifest change.

## Code

- Clippy runs with `-D warnings`. A new warning is a build failure.
- Tests live next to the code: `src/app/tests.rs` and the topic files in `src/app/tests/` for application behaviour, `mod tests` at the bottom of other modules. Tests never need a cluster; `Cluster::fake()` provides the kind registry and `apply(&mut app, json!(...))` feeds objects through the same message path a watch would.
- Every user-visible behaviour change gets a test that drives it through `handle_key`, not by calling the internal method directly.
- Things that must stay in sync:
  - the `match` in `drill` in `src/app/navigation.rs` and `views::BUILTIN_DRILLS` in `src/views.rs`
  - a new key action: its entry in the `actions!` list in `src/keymap.rs` (the `?` help reads the description from there), its rows in `docs/keys.md` and the action reference in `docs/keybindings.md`, and `docs/features.md`
  - a new built-in command and its rows in `docs/keys.md` and `docs/features.md`
  - a new config field and `docs/configuration.md`
- Do not edit `Cargo.lock` by hand. Renovate owns dependency bumps.
- Do not add comments or doc comments to code you are not otherwise changing.
