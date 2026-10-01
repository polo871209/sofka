# Contributing to sofka

You can help with bug reports, bug fixes, documentation, tests, and feature ideas.
Read [AGENTS.md](AGENTS.md) for the project rules and
[Architecture](docs/architecture.md) for the code layout.

## Discuss new features first

As described in the [pinned issue](https://github.com/nklmilojevic/sofka/issues/368),
propose new features in [GitHub Discussions](https://github.com/nklmilojevic/sofka/discussions)
before you open a feature issue or pull request.

Describe the problem, how you handle it now, and how the feature would help.
You do not need code or a complete design to start a discussion. Wait for a
maintainer to agree on the scope before you start implementation. Then link the
agreed discussion in the feature issue and pull request.

A feature issue or pull request does not replace this discussion.

## Report or fix a bug

Bug reports and bug fix pull requests do not need a discussion first. Search
the existing issues and pull requests for the same problem before you start.

[Open an issue](https://github.com/nklmilojevic/sofka/issues/new).
Include steps to reproduce the problem, the expected result, and the actual
result. Add your operating system, terminal, and the output from `sofka info`. Use
`sofka info --offline` if you cannot connect to the cluster. Check the report
for private information before you share it.

## Set up for development

Fork the repository, clone your fork, and create a branch from `main`.
The project uses Rust edition 2024 and tests with stable Rust in CI.

Install Rust, Cargo, Clippy, rustfmt, CMake, just, and lefthook. The hooks also
use `oxfmt` for Markdown and YAML, and `zizmor` for GitHub Actions.

Install the Git hooks once after cloning:

```sh
just hooks
```

Use the [Justfile](Justfile) for local commands:

```sh
just build         # Build the debug binary.
just fmt           # Format Rust code.
just check         # Check Rust formatting, run Clippy, and run tests.
just test          # Run tests without a cluster.
just run pods      # Run against the current Kubernetes context.
just smoke         # Check cluster connectivity without a terminal UI.
```

`just run` and `just smoke` use your current Kubernetes context. Use a test
cluster for manual checks that change resources.

Let the formatters control formatting. For Markdown or YAML changes, run
`oxfmt` with the changed file paths. The pre-commit hook runs the checks that
apply to the staged files and stages formatter changes. Include these changes
in your commit. Do not skip the hook with `--no-verify`.

## Tests and documentation

Use the simplest test that checks the behavior. Tests must not need a cluster.
Application tests are in `src/app/tests.rs`; other modules keep tests in a
`mod tests` block. Use `Cluster::fake()` and the existing `apply` helper to
supply test objects.

Every change to behavior visible to the user needs a test through `handle_key`.
For a bug fix, add a test that reproduces the failure and checks the result.

Include related documentation in the same pull request:

- Keys and built-in commands: update `docs/keys.md`, the `?` help in `src/ui.rs`,
  and `docs/features.md`.
- Configuration fields: update `docs/configuration.md`.
- Built-in drill-down behavior: keep `src/app/navigation.rs` and
  `views::BUILTIN_DRILLS` in sync.

See [test coverage](docs/architecture.md#test-coverage) if you need to find
missing tests.

## Open a pull request

- Target `main` and keep each pull request to one logical change. Keep unrelated
  cleanup, file moves, and formatting changes separate.
- Explain the problem, the resulting behavior, and the checks you ran. Link the
  agreed discussion for a feature. Use `Closes #N` when the change resolves an issue.
- Use a conventional commit prefix: `feat:`, `fix:`, `docs:`, `refactor:`, `test:`,
  or `chore:`. State what changed in the subject and why in the body.
- Run `just check` before submission. Rust formatting, Clippy with
  `-D warnings`, and `cargo test --locked` must pass. All required CI checks
  must also pass before the pull request is ready.
- Do not edit `Cargo.lock` by hand. If a manifest change requires a lockfile
  update, generate it with Cargo and include it. Renovate manages dependency bumps.

## Use of coding agents

Read [AGENTS.md](AGENTS.md) before work starts. Agents must check for a feature
discussion and maintainer agreement before implementation. If either is missing,
they must show the warning in that file and ask for the agreed discussion.

Review and understand generated code. Run the required checks. You are
responsible for the change you submit. The same contribution process applies
with or without an agent.
