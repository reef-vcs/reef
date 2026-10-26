# Changelog

All notable changes to Reef. The format follows [Keep a Changelog](https://keepachangelog.com/en/1.1.0/); versions follow [Semantic Versioning](https://semver.org/) once a release exists.

## [Unreleased]

### Added

- The read side of the API ([ADR 0030](docs/adr/0030-reads-over-jj-state.md)): `GetStatus` (the repository's trunk, active workspaces, mutable heads, conflicted and divergent changes, and a workspace's changed and conflicted files), `ListChanges` (the log, with graph edges, over jj's revset language), `GetChange`, `GetDiff` (summary, line counts, or unified hunks, with renames and copies), `ListOperations` and `GetOperation`. Revisions are jj revsets with Reef's `trunk()`, `immutable_heads()`, `immutable()` and `mutable()`; reads can be made at any operation, and reads of a workspace's `@` snapshot it first. The CLI's commands for them come with R2.05.
- Every operation a response reports carries its effects: changes created, rewritten, hidden or abandoned, bookmarks moved, working copies moved. Operations Reef did not just write are classified by kind.
- The write side of the API ([ADR 0031](docs/adr/0031-writes-over-jj-state.md)): `DescribeChange` sets a commit's description; `AbandonChange` abandons a commit, and on one commit of a divergent change resolves the divergence, handing its descendants, bookmarks and working copies to the surviving commit; `UndoOperation` reverts exactly one operation, refusing what it cannot revert exactly (workspace creation, release and moves, git ref imports, operations later ones changed again). Each takes an optional `expected_commit_id` (describe, abandon) and returns the operation it recorded, with effects. Workspaces a write moves are held still and, for abandon and undo, snapshotted first, so a write never overwrites unsnapshotted edits; they take the result afterwards. The CLI's commands for them come with R2.05.
- Every operation Reef records carries `reef.kind`, `reef.workspace`, `reef.agent` and, for snapshots, `reef.snapshot-kind` attributes.
- The `reef-agent` gRPC request metadata names the agent behind a write; operations record it as their agent instead of the workspace's owner. `Client::with_agent` sends it.
- API revision 2: `OperationEffects.change_count` and `bookmark_count`, `CHANGE_EFFECT_KIND_HIDDEN`, `BookmarkEffect.conflicted`, `WorkspaceStatus.conflicted_path_count`, and `FileDiff`'s `old_no_final_newline`, `new_no_final_newline`, `too_large` and `invalid_utf8`.

### Changed

- An operation's `kind`, `snapshot_kind`, workspace and `agent` come from its `reef.*` attributes when it has them; `import git head` at registration is now `OTHER` rather than `EXTERNAL`.
- The daemon's API is `reef.api.v1`, in the new `reef-api` crate: typed enums instead of strings, every list paginated, every write returning the jj operation it recorded, errors as `google.rpc.Status` with a `reef.api.v1.ErrorDetail` that stock gRPC tooling decodes, and every RPC Phase 2 implements declared up front ([ADR 0029](docs/adr/0029-daemon-api-v1.md)). `reef.daemon.v0` is gone except `Daemon/Status`, which a 0.1 `reef daemon status` and `stop` still use.
- A daemon that cannot serve a request as asked says so: exit code 9, kind `unsupported`. The CLI's JSON error object carries the daemon's `reason` when there is one, such as `API_VERSION_MISMATCH`.
- `reef ws list` asks the daemon for workspaces newest first, a page at a time.
- `ws create --change`/`--new-on`, `ws move`, and `store build --rev` take a jj revset naming one commit, resolved in jj's order: tags, bookmarks, commit id prefixes, then change id prefixes. A bookmark whose name is also an id prefix (`cafe`) now names the bookmark, and hex text that prefixes both a commit id and a change id names the commit. A divergent change id or a conflicted bookmark is now `invalid_argument` (exit 2, reason `AMBIGUOUS_REVISION`, with the commits in the hint) instead of `conflict` (exit 5).

### Upgrading from 0.1

A 0.1 daemon keeps running after the binaries are replaced. The first `reef` command restarts it when it has no active workspaces. Otherwise the command fails with exit code 9 and `next` set to `reef daemon stop`: stop it once its `ws run` sessions end, and the next command starts the new one. A 0.1 `reef` against the new daemon is told to upgrade (exit 5); its `daemon status` and `daemon stop` keep working.

## [0.1.0] - 2026-10-25

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
