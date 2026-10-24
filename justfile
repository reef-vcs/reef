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

# The committed AGENTS.md, CLAUDE.md, and Claude Code skill are what `reef init` writes (R1.27).
# Fix a failure with `cargo run -p reef-vcs -- init --agents claude --no-register` and commit.
agent-files:
    cargo run -q -p reef-vcs -- init --agents claude --no-register --check -o text

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

# Benchmarks on the REEF_BENCH_PRESET synthetic repo (default medium). See docs/benchmarks.md.
bench *ARGS:
    cargo bench --workspace --bench '*' "$@"

# Workspace creation benchmarks alone (R1.12) on a preset, large by default. `just bench` includes
# them; this is the exit-criterion run. See docs/benchmarks.md.
bench-ws preset="large":
    REEF_BENCH_PRESET="$1" cargo bench -p reef-daemon --bench workspace_create -- --sample-size 10

# Quick local benchmark on the small preset.
bench-quick:
    REEF_BENCH_PRESET=small cargo bench --workspace --bench '*' -- --sample-size 10 --warm-up-time 1 --measurement-time 5

# Generate a synthetic repo under target/synth/<preset> (idempotent). Presets: small, medium, large, xl.
synth preset="large":
    cargo run -p reef-testing --bin reef-synth --release -- gen --out "target/synth/$1" --preset "$1" --seed 42

# SHA-256 over the small seed-1 tree's files and symlinks. Must print the same digest on every
# platform and match crates/reef-testing/synth-small-seed1.sha256; CI's synth-determinism job
# checks both.
synth-digest:
    cargo run -q -p reef-testing --bin reef-synth -- gen --out target/synth/determinism --preset small --seed 1 --force >/dev/null
    cd target/synth/determinism && { find . -type f | LC_ALL=C sort | xargs shasum -a 256; find . -type l | LC_ALL=C sort | while read -r link; do echo "$link -> $(readlink "$link")"; done; } | shasum -a 256 | cut -d' ' -f1

# Print reef's shell completions (bash, zsh, fish, elvish, powershell). Write them where the shell
# looks, for example:
#   zsh:  just completions zsh > ~/.zfunc/_reef        (with `fpath+=~/.zfunc` before compinit)
#   bash: just completions bash > ~/.local/share/bash-completion/completions/reef
#   fish: just completions fish > ~/.config/fish/completions/reef.fish
completions shell="zsh":
    cargo run -q -p reef-vcs -- completions "$1"

# install.sh and the release scripts (R1.28): shellcheck, then a parse by dash, the strictest
# POSIX sh at hand. Needs shellcheck (`brew install shellcheck`, `apt-get install shellcheck`).
installer-lint:
    shellcheck install.sh scripts/*.sh
    if command -v dash >/dev/null; then dash -n install.sh; else sh -n install.sh; fi

# install.sh against a tarball of the debug build, into a scratch HOME (R1.28). Run `just build`
# first.
installer: installer-lint
    scripts/test-install.sh --build-dir "${CARGO_TARGET_DIR:-target}/debug"

# Agent onboarding benchmark (RX.07): a real Claude Code agent per trial, so it needs `claude` and
# ANTHROPIC_API_KEY, and each trial may spend up to $2. Takes the brief's `scenario=A trials=1`
# form or the binary's flags; results in crates/reef-testing/onboarding/results. See
# docs/onboarding-benchmark.md.
onboarding *ARGS:
    cargo build -p reef-vcs -p reef-daemon -p reef-testing --bins
    args=(run --reef-bin-dir "${CARGO_TARGET_DIR:-target}/debug"); \
    for arg in "$@"; do \
        case "$arg" in \
            scenario=*) args+=(--scenario "${arg#scenario=}") ;; \
            trials=*) args+=(--trials "${arg#trials=}") ;; \
            *) args+=("$arg") ;; \
        esac; \
    done; \
    "${CARGO_TARGET_DIR:-target}/debug/reef-onboarding" "${args[@]}"

# Every relative link in the repository's Markdown resolves (R1.24).
doc-links:
    scripts/doc-links.sh

# Everything CI runs, in CI order.
ci: fmt-check doc-links lint build agent-files installer test doc deny

clean:
    cargo clean
