set shell := ["bash", "-euo", "pipefail", "-c"]
set positional-arguments

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
    if command -v cargo-nextest >/dev/null; then cargo nextest run --workspace "$@"; else cargo test --workspace "$@"; fi
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
    cargo bench --workspace "$@"

# Generate a synthetic repo under target/synth/<preset> (idempotent). Presets: small, medium, large, xl.
synth preset="large":
    cargo run -p reef-testing --bin reef-synth --release -- gen --out "target/synth/$1" --preset "$1" --seed 42

# SHA-256 over the small seed-1 tree's files and symlinks. Must print the same digest on every
# platform and match crates/reef-testing/synth-small-seed1.sha256; CI's synth-determinism job
# checks both.
synth-digest:
    cargo run -q -p reef-testing --bin reef-synth -- gen --out target/synth/determinism --preset small --seed 1 --force >/dev/null
    cd target/synth/determinism && { find . -type f | LC_ALL=C sort | xargs shasum -a 256; find . -type l | LC_ALL=C sort | while read -r link; do echo "$link -> $(readlink "$link")"; done; } | shasum -a 256 | cut -d' ' -f1

# Everything CI runs, in CI order.
ci: fmt-check lint build test doc deny

clean:
    cargo clean
