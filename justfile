set shell := ["bash", "-euo", "pipefail", "-c"]

default:
    @just --list

# One-time contributor setup: hooks, nextest.
setup:
    git config core.hooksPath .githooks
    cargo install cargo-nextest --locked || true
    cargo install cargo-deny --locked || true
    cargo install just --locked || true

build:
    cargo build --workspace --all-targets

# Unit and integration tests via nextest when present, else cargo test. Doctests always via cargo test.
test *ARGS:
    if command -v cargo-nextest >/dev/null; then cargo nextest run --workspace {{ARGS}}; else cargo test --workspace {{ARGS}}; fi
    cargo test --workspace --doc

lint:
    cargo clippy --workspace --all-targets --all-features -- -D warnings

fmt:
    cargo fmt --all

fmt-check:
    cargo fmt --all -- --check

doc:
    RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items

deny:
    cargo deny check licenses bans sources

deny-advisories:
    cargo deny check advisories

bench *ARGS:
    cargo bench --workspace {{ARGS}}

# Everything CI runs, in CI order.
ci: fmt-check lint build test doc deny

clean:
    cargo clean
