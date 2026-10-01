set unstable
set shell := ["bash", "-eu", "-o", "pipefail", "-c"]

mod rust '.just/rust.just'

default:
    @just --list

# Install local git hooks.
hooks:
    lefthook install

# Format Rust sources.
fmt: rust::fmt

# Check Rust formatting without changing files.
fmt-check: rust::fmt-check

# Run clippy with the same policy as CI.
clippy: rust::clippy

# Run the unit test suite.
test: rust::test

# Generate an HTML coverage report.
coverage: rust::coverage

# Fast local confidence check.
check: fmt-check clippy test

# Build the debug binary.
build: rust::build

# Build the release binary.
build-release: rust::build-release

# Run sofka against the current kube context.
run resource="pods":
    just rust run {{ resource }}

# Headless cluster connectivity check.
smoke: rust::smoke

# Render one headless UI snapshot.
snapshot resource="pods":
    just rust snapshot {{ resource }}
