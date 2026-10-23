# Changelog

All notable changes to Reef. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/) once a release exists.

## [Unreleased]

Phase 1: leased copy-on-write workspaces. Start with the [quickstart](docs/quickstart.md).

### Added

- `reef-daemon`, a per-user service over a Unix socket (gRPC, `reef.daemon.v0`), with a SQLite state database, structured logs, and a graceful restart that keeps leases and mounts. Any `reef` command starts it.
- Workspaces on existing git and jj repositories: `reef ws create|list|inspect|release|move|gc`, each a jj workspace materialized copy-on-write (APFS clones, kernel overlayfs or fuse-overlayfs, reflinks on btrfs and XFS, or a plain copy).
- No setup step: the first `reef` command in a git checkout registers it and initializes jj alongside git (`.jj/` and one line in `.git/info/exclude`).
- Continuous snapshots driven by a filesystem watcher (notify; Watchman for large repositories when installed), with staleness and divergence handling that never discards an agent's edits.
- Leases with heartbeats, expiry, and reclaim with a final snapshot; `reef ws run` as the agent entry point.
- `reef init`, `reef guide`, and `reef here`: `AGENTS.md`, `CLAUDE.md`, and a Claude Code skill that start agents in a workspace on their own, and errors that carry a `hint` and the `next` command to run.
- `reef.toml` workspace spec: lease default, snapshot excludes, and shared dependency stores built once per lockfile and mounted read-only (`reef store build|list`).
- `reef backup` and `reef restore`, and a quiesce RPC for filesystem snapshots.
- Prebuilt binaries for macOS and Linux on arm64 and x86_64, a one-line installer, a Homebrew tap, and `cargo binstall` metadata.
- Benchmarks for workspace creation and a load test at 200 workspaces ([docs/benchmarks.md](docs/benchmarks.md), [docs/load-tests.md](docs/load-tests.md)).

### Known limitations

- Watcher snapshots are local: they stay in the repository on this machine and are never pushed or exported to git.
- Two workspaces editing one change make it divergent. Nothing is lost; resolving it is a jj operation for now.
- New workspaces start on the `main` bookmark. In a repository without one they start on the checkout's own working-copy change; pass `--new-on <branch>`.
- In a git checkout, Reef imports branches but does not export: an agent's change is a jj change, not a git branch, until you make it one.
- A workspace directory has neither `.git` nor `.jj`, so `git` and `jj` commands inside it fail: agents work on files, and you run `git` and `jj` in your checkout.
- `reef.toml` is read from the commit a workspace starts on, not from disk: commit it before it takes effect.
- A shared store is read-only, so a tool that writes a cache into it fails (Vitest's `node_modules/.vitest-cache`; Vite's `cacheDir` setting moves it).
- On macOS, and on Linux without `CAP_SYS_ADMIN`, a store is protected by file permissions only: a process running as the same user could make it writable again.
- Reef is built on jj-lib 0.46. A newer `jj` may upgrade the repository's on-disk formats when you run it there.
- Tools that do not read `.git/info/exclude` see the `.jj/` directory.
- The binaries are not signed or notarized. Windows is not supported.
