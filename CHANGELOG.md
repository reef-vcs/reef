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
- `reef init`, `reef guide`, and `reef here`: `AGENTS.md`, `CLAUDE.md`, and a Claude Code skill that start agents in a workspace on their own, and errors that carry a `hint` and the `next` command to run. `reef status` and similar guesses at "where am I" point at `reef here`.
- `reef.toml` workspace spec: lease default, snapshot excludes, and shared dependency stores built once per lockfile and mounted read-only (`reef store build|list`).
- `reef backup` and `reef restore`, and a quiesce RPC for filesystem snapshots.
- Prebuilt binaries for macOS and Linux on arm64 and x86_64, a one-line installer, a Homebrew tap, and `cargo binstall` metadata.
- Benchmarks for workspace creation and a load test at 200 workspaces ([docs/benchmarks.md](docs/benchmarks.md), [docs/load-tests.md](docs/load-tests.md)).
- An agent onboarding benchmark: what a Claude Code agent new to Reef spends to reach a working `reef ws run`, in four scenarios, run in CI on demand ([docs/onboarding-benchmark.md](docs/onboarding-benchmark.md)).
- A threat model ([docs/threat-model.md](docs/threat-model.md)) and the fixes from its review: the daemon refuses a state directory another user owns and keeps its state `0700` and its database `0600`; registration refuses a repository another user owns (`[repo] allow_foreign_owner`); `reef store build` prints the command it runs, and `[store] allow_builds = false` turns builds off; a root daemon warns at start; a commit with a tree entry jj cannot read is refused instead of crashing the repository; the release workflow attests each tarball's build provenance (`gh attestation verify`), first exercised by 0.1.0's release.

### Known limitations

- Watcher snapshots are local: they stay in the repository on this machine and are never pushed or exported to git.
- Two workspaces editing one change make it divergent. Nothing is lost; resolving it is a jj operation for now.
- New workspaces start on `main`, `origin/HEAD`'s branch, `master` or `trunk`. In a repository with none of these they start on the checkout's own working-copy change, and a `jj` command in the checkout rebases them; pass `--new-on <branch>`.
- In a git checkout, Reef imports branches but does not export: an agent's change is a jj change, not a git branch, until you make it one.
- A workspace directory has neither `.git` nor `.jj`, so `git` and `jj` commands inside it fail: agents work on files, and you run `git` and `jj` in your checkout.
- `reef.toml` is read from the commit a workspace starts on, not from disk: commit it before it takes effect.
- On Linux a new workspace's watch attaches in the background after `reef ws create` returns, and `reef ws inspect` shows `notify (attaching)` meanwhile; writes made before it is up reach the store with its first snapshot.
- A shared store is read-only, so a tool that writes a cache into it fails (Vitest's `node_modules/.vitest-cache`; Vite's `cacheDir` setting moves it).
- On macOS, and on Linux without `CAP_SYS_ADMIN`, a store is protected by file permissions only: a process running as the same user could make it writable again.
- A workspace is not a sandbox: an agent runs as you and can do anything you can, including through the daemon's socket, and a harness sandbox that lets it reach the socket does not confine what the daemon does for it.
- `reef store build` runs the repository's declared command as you, with network access.
- The daemon's state directory and socket directory must be your own: a `REEF_SOCKET` directly in `/tmp`, or a state directory another user created, is refused at start.
- Reef is built on jj-lib 0.46. A newer `jj` may upgrade the repository's on-disk formats when you run it there.
- Tools that do not read `.git/info/exclude` see the `.jj/` directory.
- The binaries are not signed or notarized. Windows is not supported.
