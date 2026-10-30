# Reef development phases

*Companion to [design.md](design.md). Last updated 2026-10-25.*

This document breaks the design into phases and the phases into tickets. Each ticket row is meant to become one issue. Phases map to GitHub milestones, and a phase's exit criteria are the milestone's acceptance test.

**How to read the tables**

- **ID** goes in the issue title, for example `R1.09 overlayfs backend`. IDs become stable once a phase's tickets are filed as issues; until then, inserting a ticket renumbers the rest of its phase and this document is the only place the numbers live.
- **Size** is a rough estimate for one engineer with heavy agent assistance: S is up to two days, M is up to a week, L is two to three weeks. Anything bigger is split.
- **Depends on** lists tickets that must land first. Tickets with no dependencies inside a phase can run in parallel.
- Workstreams group tickets that one person would naturally own.

Phases are sequential at the milestone level, but later phases contain tickets that can start early once their dependencies exist. Those are marked.

---

## Phase 0: Foundations

**Goal.** A repository where a contributor can clone, build, test, and benchmark on macOS and Linux in under ten minutes, with jj-lib confirmed as a viable foundation.

**Exit criteria.**

- CI runs build, test, clippy, and rustfmt on macOS and Linux on every push.
- A spike proves the daemon can open an existing jj repository through jj-lib, create a snapshot, and read the operation log.
- A synthetic repository generator produces a 100k-file tree for benchmarks.

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R0.01 | Workspace layout and crate roles | Settle the Cargo workspace: `reef-lib` (data model, workspaces, merge), `reef-daemon` (service, API), `reef-vcs` (the `reef` binary and umbrella), and an unpublished `reef-testing` (synthetic repository generator, fixtures) that published crates reach only as a dev-dependency. Document crate boundaries in a short ADR. | S | |
| R0.02 | Toolchain pinning and dev tooling | `rust-toolchain.toml`, a `justfile` or equivalent for build, test, lint, and bench, rustfmt and clippy config, pre-commit hook. | S | R0.01 |
| R0.03 | CI on macOS and Linux | GitHub Actions matrix: build, test, clippy, fmt. Cache cargo. Fail on warnings. | S | R0.02 |
| R0.04 | jj-lib embedding spike | Pin a jj-lib version. Open an existing jj repo, snapshot the working copy, list changes, read the op log, undo one op. Record what jj-lib exposes and what it does not in an ADR. | M | R0.01 |
| R0.05 | Synthetic repository generator | A tool that generates a repo with N files, configurable depth and file sizes, plus a few large binaries, for benchmarks and tests. | S | R0.01 |
| R0.06 | Benchmark harness | Criterion setup with one benchmark that snapshots the synthetic repo. Results published as CI artifacts. | S | R0.04, R0.05 |
| R0.07 | ADR directory and template | `docs/adr/` with a template. Backfill ADRs for the choices already made in design.md: jj-lib, SQLite, gRPC, Tauri, CEL, and the hash function and object backend by phase (git backend with SHA-1 through phase three, a Reef backend with BLAKE3 and chunked blobs from phase four). | S | |
| R0.08 | Contributor docs | `CONTRIBUTING.md`, issue and PR templates, code of conduct, DCO or CLA decision. | S | |
| R0.09 | Error and logging conventions | Choose error types (`thiserror` in lib, `anyhow` at edges), `tracing` setup, structured log format. Document in an ADR. | S | R0.01 |

---

## Phase 1: Leased copy-on-write workspaces

**Goal.** A daemon and CLI that replace git worktrees for agents on existing jj repositories: workspaces that cost only what the agent changed, are snapshotted continuously, and reclaim themselves when the agent goes away.

**Exit criteria.**

- Twenty workspaces on the 100k-file synthetic repo, each created in under one second on APFS and overlayfs, with disk usage per workspace proportional to modified files. Met on APFS on the M2 Max (962 and 969 ms per create in R1.29 and R1.32) and on the `ubuntu-24.04` runner since R1.32: 19.5 ms per create on kernel overlayfs (1,006 ms before) and 22.3 ms on `fuse-overlayfs`, with a Linux workspace's notify watch attached after the create returns ([`benchmarks.md`](benchmarks.md#linux-workspace-creation-off-the-inotify-watch-path-r132)).
- Killing an agent process mid-edit leaves a snapshot in the store and no directory on disk after the lease expires.
- Two workspaces on the same change work without interfering.
- A shared dependency store is mounted read-only into every workspace and verified against a lockfile hash.
- Works on macOS and Linux with the same CLI.
- A workspace moves to another change without losing its lease or leaving a directory behind.
- Two hundred workspaces with active watchers on one machine, each on a 10,000-file repository (the `medium` preset), with a write reaching the store as a snapshot within five seconds at that load. Revised by R1.23 and R1.30, and stated as measured. Since R1.30 a notify workspace's watcher snapshot scans only the paths its events named (ADR 0026), and the target holds on a 12-core M2 Max on `medium` (p95 729 ms) and on 100,000-file repositories (`large`, p95 794 ms; 174 s before R1.30, when each notify snapshot scanned every tracked file), and on a 4-vCPU `ubuntu-24.04` runner on `medium` (p95 under 0.8 s). It now holds on a 3-vCPU `macos-15` runner on `medium` as well (p95 854 ms, from 149 s), and `load.yml` gates on it again (`docs/load-tests.md`).
- A backup taken while agents are writing restores to a daemon that serves the same changes and operation log, with no workspace lost and no lease reclaimed on the first tick.
- On the onboarding benchmark's scenario A (RX.07), an agent with no Reef configuration and only `reef` on `PATH` reaches a successful `reef ws run` in at most eight tool calls and with at most two failed `reef` invocations, median of three trials. Met by the first baseline (2026-10-24, macOS, Claude Code 2.1.310, `claude-sonnet-5-5`): a median of 4 calls and 1 failed `reef` invocation, all three trials successful ([`onboarding-benchmark.md`](onboarding-benchmark.md)).

### Workstream: workspace model and daemon skeleton

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.01 | Repository and workspace schema in SQLite | One SQLite database per machine under the daemon state directory. Tables for repositories (id, root path, jj repo path, registered at), workspaces (id, repository id, owner, change ID, purpose, backend, path, lease expiry, created, last snapshot), leases, and reclaim log. Every table carries a repository id; the daemon serves many repositories. Owner is an opaque string the client supplies, `user@host:pid` by default from `reef ws run`, and is replaced by the session identity from R3.03 without a schema change. Migrations. | S | R0.04 |
| R1.02 | Materialization backend trait and base cache | `Backend` trait: materialize a base at a path, report disk usage, swap the base, tear down. jj-lib's working copy scans the merged directory itself, so backends capture nothing. Capability detection per platform and filesystem. Base cache: a tree materialized once per machine through jj-lib, keyed by tree id, that every backend materializes from. | M | R1.01 |
| R1.03 | Daemon skeleton over a Unix socket | Long-running `reef-daemon` with tonic gRPC over a Unix socket, health check, graceful shutdown, single-instance lock, structured logs. | M | R1.01, R0.09 |
| R1.04 | Daemon configuration and state directory layout | Where the daemon keeps its state (`$XDG_STATE_HOME/reef` or platform equivalent, overridable), the socket path, the SQLite database, per-repository subdirectories for base caches and materialized workspaces, and log files. A `config.toml` with lease defaults, watcher settings, log options, and the backup retention window from the design's durability section, with environment overrides and a `reef config show` that prints the effective configuration. Repositories are registered by path; jj user configuration is not read. | S | R1.03, R0.09 |
| R1.05 | Workspace service: create, list, release | gRPC methods and protobuf messages for registering, listing, and forgetting repositories, and for creating a workspace on a change, listing with owner and lease state, inspecting, snapshotting, and releasing (through the reclaim pipeline that lease expiry also uses, R1.15). Every request names a registered repository. Protobuf schema lives in `reef-daemon/proto`. | M | R1.02, R1.03, R1.07 |
| R1.06 | Move a workspace between changes | Point an existing workspace at a different change without tearing it down: final snapshot of the current change, then the backend swaps the base to the target change's snapshot and keeps the writable layer empty. The lease and owner carry over. Refuses if the workspace has an unsnapshotted write in flight. API, CLI (`reef ws move`), and a conformance test per backend. | S | R1.05, R1.13, R1.11 |

### Workstream: materialization backends

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.07 | Plain copy fallback backend | Full copy materialization. Exists so every platform works and as the baseline for benchmarks. | S | R1.02 |
| R1.08 | APFS clone backend (macOS) | One `clonefile` of the base cache entry for the whole tree, per-file clones as the fallback, and incremental base materialization by cloning the nearest existing base. Eviction is R1.17. | M | R1.02 |
| R1.09 | overlayfs backend (Linux) | Read-only lower from a materialized base, writable upper per workspace. Kernel overlayfs when the daemon has `CAP_SYS_ADMIN`, fuse-overlayfs otherwise. User namespaces are not used in this phase: their mounts are invisible outside the namespace. | L | R1.02 |
| R1.10 | Reflink backend (btrfs, XFS) | `FICLONE` per file. Shares the base cache with the APFS backend. | S | R1.08 |
| R1.11 | Backend conformance tests | One test suite every backend must pass: materialize, modify, capture, tear down, large files, symlinks, permissions, case sensitivity. | M | R1.07 |
| R1.12 | Workspace creation benchmarks | Time and disk for 1, 20, and 100 workspaces per backend on the synthetic repo. Published from CI on both platforms. | S | R1.11, R1.05, R0.06 |

### Workstream: snapshotting and leases

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.13 | Watcher-driven snapshots | `notify` watcher per workspace with debounce. Capture the writable layer and snapshot through jj-lib. Ignore rules from the repo plus a Reef-specific ignore file. | M | R1.05, R0.04 |
| R1.14 | Watchman backend (optional) | Use Watchman when present for large repos. Same interface as the notify watcher. | M | R1.13 |
| R1.15 | Lease manager | Heartbeat RPC, expiry loop, configurable lease length and grace. On expiry: final snapshot, op log entry, reclaim. Daemon restart preserves leases and applies a grace period before reclaiming anything. | M | R1.05, R1.13 |
| R1.16 | Local-only snapshot marking | Snapshots taken by the watcher are marked local and never exported or pushed until a change is explicitly published. | S | R1.13 |
| R1.17 | Garbage collection | Reclaim snapshots unreferenced by any change and older than the undo window. Reclaim base cache entries no workspace uses. Report what was freed. `reef ws gc`, added to the CLI skeleton from R1.22. | M | R1.15, R1.16, R1.22 |
| R1.29 | Workspace creation off the watcher's path | R1.12 found APFS creation through the daemon at 1.93 s per workspace against the 1 s criterion, all of the excess in the watcher: notify's file ID cache walks the new tree inside `Create`, and Watchman's crawl slows the next clone. Drop the cache, attach on the blocking pool, report per-step create timings, and defer Watchman subscriptions during a burst of creates if measurement still shows the crawl. | M | R1.12, R1.13, R1.14 |
| R1.30 | Incremental notify snapshots | R1.23 found 200 `large` workspaces at a 174 s p95 against the 5 s target, and the 3-vCPU `macos-15` runner failing `medium`, because every notify snapshot scans every tracked file. Measure first where a loaded scan's time goes (the tree-state reload or the walk) and whether Watchman's narrowing passes the same profile; if narrowing is the fix, keep each workspace's reported paths, scan them as prefixes, and fall back to a full scan on overflow, too many paths, or `.reefignore`. ADR. | L | R1.13, R1.14, R1.23 |
| R1.31 | A default start revision beyond `main` | A workspace with no start revision begins on `main`, else on the root checkout's own working-copy change, so in a repository whose default branch is `master` agents inherit the checkout's uncommitted edits and a `jj` command there rebases their changes (found by R1.24). Try `main`, then `origin`'s HEAD, then `master` and `trunk`, then the remote-tracking bookmarks, and say when it falls back. | S | R1.05, R1.20, R1.25 |
| R1.32 | Linux workspace creation off the inotify watch path | The first `large` bench on the `ubuntu-24.04` runner put `create_api/large/overlayfs/20` at 1.006 s against the 1 s criterion, 985 ms of it inotify adding a watch for each of the 8,336 directories inside `Create`. On Linux, queue the notify attach behind `Create`, one workspace at a time, and mark the workspace for one full scan when its watch comes up, so nothing written meanwhile is lost; `ws inspect` shows `attaching`; the bench and the load test wait for the attaches and record the drain. macOS keeps its inline 9 ms attach. ADR. | M | R1.12, R1.23, R1.29, R1.30 |
| R1.18 | Local backup and restore | Daemon backup operation per the design's durability section: hold the write lock briefly, SQLite online backup, copy immutable object and operation files, operation heads last. A standalone quiesce RPC with pause and resume so a filesystem snapshot can be taken between them. Restore loads from a copy, keeps workspace records, rematerializes directories on demand, and treats every lease as expired with a grace period. `reef backup` and `reef restore`. Round-trip test with agents writing during the backup. | M | R1.04, R1.15, R1.16, R1.17, R1.22 |

### Workstream: shared stores and agent integration

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.19 | Workspace spec file | `reef.toml` in the repo declaring shared stores to mount, build output paths to keep out of snapshots, and lease defaults. | S | R1.01 |
| R1.20 | Shared dependency store mounting | Mount or link declared stores into workspaces read-only. Verify against a lockfile hash before mounting. Daemon-only writes. | M | R1.19, R1.08, R1.09, R1.05, R1.13, R1.22 |
| R1.21 | `reef ws run` | Run a command inside a leased workspace with automatic heartbeat for the process lifetime and release on exit. The primary agent entry point for phase one. | S | R1.05, R1.15, R1.22 |
| R1.22 | CLI for workspaces | `reef ws create`, `list`, `release`, `inspect`, daemon lifecycle, repository registration, and `reef completions <shell>`, with JSON output when stdout is not a terminal. Thin client of the daemon. Subcommands owned by later tickets (`ws gc` is R1.17's) exist as stubs until those land. | M | R1.04, R1.05 |
| R1.23 | Crash, reclaim, and load integration tests | Kill agents mid-edit, kill the daemon, fill the disk, expire leases, and assert the invariants from the exit criteria. Run 200 workspaces with active watchers on one machine on both platforms, measure write-to-snapshot latency against the phase target, and record inotify and thread pool limits hit. The gated profile (`load.yml`) uses the `medium` preset, gating on `ubuntu-24.04` and reporting only on `macos-15`; 200 workspaces on `large` are opt-in and run by hand (`docs/load-tests.md`). | M | R1.15, R1.20 |
| R1.24 | Quickstart documentation | Install with the one-line installer, run an agent in a workspace in any git or jj repository with no setup step, set up the agent's harness, watch the workspace reclaim. | S | R1.18, R1.20, R1.21, R1.22, R1.25, R1.26, R1.27, R1.28 |
| R1.25 | Zero-step repository onboarding | Any command that resolves `--repo` by path registers the repository on the way, and registration initializes jj alongside an existing git checkout (colocated, `.jj` plus one `info/exclude` line). `--no-register` and `[repo] auto_register` opt out. ADR. | M | R1.05, R1.21, R1.22 |
| R1.26 | Agent-facing CLI contract | `reef guide` (and `reef` with no arguments) under 400 tokens, `hint` and `next` argv on every error with a known fix, `reef here` for orientation, aliases and prefix inference, `REEF_OUTPUT`, never interactive. The contract is an ADR and a test suite. | M | R1.05, R1.21, R1.22 |
| R1.27 | `reef init` agent configuration | Registers the repository (R1.25) and writes managed sections into `AGENTS.md` and `CLAUDE.md`, a Claude Code skill, and a Cursor rule from the same guide text; `--check`, `--print`, `--user`. Reef's own repository dogfoods it in CI. | S | R1.25, R1.26 |
| R1.28 | Prebuilt binaries and a one-line install | Release workflow on a version tag for four targets, `install.sh` with checksum verification and no `sudo`, a Homebrew tap formula, `cargo binstall` metadata, `reef --version` with commit and target. Unsigned until R4.17. | M | R0.03, R1.22 |

---

## Phase 2: Typed API, MCP server, and live state

**Goal.** Agents and humans use one typed API for everything, state streams in real time, and the first web UI shows what the fleet is doing.

**Exit criteria.**

- The CLI is a thin client with no direct jj-lib access.
- An MCP server exposes the same operations as the API.
- A client can subscribe to the operation log and presence channel and receive events within 100 ms, and that holds with 500 subscribers on one daemon at 1,000 operations per minute (measured by R2.22 at p99: 53 ms for operations, 57 ms for presence, on a 12-core Mac; `docs/load-tests.md`).
- Symbol-level diffs work for Python, TypeScript, Go, and Rust.
- A local web UI shows the smartlog, workspaces, and fleet panel live.
- Token cost to answer "what is the state of this repo" is measured and at least five times lower than parsing git output.
- Status on the 100k-file synthetic repository is answered from the incremental index without a tree walk.
- A daemon in a VM is reachable from another machine over TLS with a token, serving the same API and web UI.

### Workstream: API

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.01 | Core protobuf schema | Package `reef.api.v1` in a new `reef-api` crate: messages and services for status, snapshot, diff, log, changes, undo, and workspaces, and the declarations R2.04, R2.09, R2.10, and R2.11 implement, so later tickets only add. Phase 1's services ported to v1, and v0 retired behind a compatibility path that names a version mismatch instead of failing blank. Versioned, with conventions and an API review checklist in an ADR and `buf` lint and breaking-change checks in CI. Reviewed as an API, not generated from code. | L | R1.05 |
| R2.02 | Status, log, and change services | Implement the read side of the schema over jj-lib: status, the log and changes (revsets with Reef's `trunk()` and `immutable_heads()`, reads at an operation), diff, and the operation log with each operation's kind and effects. Structured, paginated responses. Workspace creation, moves, and store builds resolve revisions as revsets. | L | R2.01 |
| R2.03 | Snapshot, describe, and undo services | The write side: snapshot now, set change description, abandon a change (how a divergent change is resolved, R1.13), undo an operation, with each response returning the op log entry created. | L | R2.01, R2.02 |
| R2.04 | Virtual workspace API | Read a file or list a directory at a snapshot, apply edits (write, replace text, delete, copy or rename, set the executable bit) to a change or as a new change on one, and receive the new snapshot hash, without materializing a directory. Workspaces on an edited commit are snapshotted first and take the result. | L | R2.03 |
| R2.05 | CLI as thin client | Move the client, the error conventions and path resolution into `reef-client` and `reef-error`, and run `reef daemon run`, `config show` and `restore` through the `reef-daemon` binary, so `reef`'s dependency graph has no jj-lib (enforced by a test). An agent name for the CLI (`--agent`, `REEF_AGENT`). | M | R2.01 |
| R2.27 | VCS commands in the CLI | `reef status`, `log`, `show`, `diff`, `describe`, `abandon`, `undo`, `op log`, `op show`, `ws snapshot`, `file read` and `file list` over the v1 services, with JSON for each, the CLI's own hints and `next` for API error reasons, and `metadata` in error objects. Writes in a workspace return it, so a divergent result shows, and `reef here` points a divergent workspace at `reef log`; one daemon reason, `SNAPSHOT_FAILED`. | L | R2.04, R2.05 |
| R2.06 | Python SDK | Generated from the schema, published to PyPI as `reef-vcs`, with a thin idiomatic wrapper. | S | R2.01 |
| R2.07 | TypeScript SDK | Generated from the schema, published to npm, with a thin wrapper. | S | R2.01 |
| R2.08 | Remote daemon access | A second listener for the same gRPC services and the web UI over TCP with TLS and a bearer token, off by default and enabled by configuration or `reef daemon --remote`. Token issuance and rotation through the CLI. The Unix socket stays the local path and the remote listener never serves it without authentication. This is how a phone or another laptop reaches a daemon running in a cloud VM before a store exists. Threat model reviewed under RX.01. | M | R2.01, R1.04 |

### Workstream: live state

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.09 | Operation log subscription | Server-streaming RPC filtered by repository, change, workspace, or agent, woken by the repository thread for every operation Reef records and by a poll of the op heads for other jj writers. Resume from a given operation or a frame's cursor, at most 10,000 operations back. Server-side batching into frames; nothing dropped, a slow subscriber held back by flow control. Delivery within 100 ms for a few subscribers, with the per-subscriber log read behind the seam R2.22's broker replaces. The MCP wait tool moves onto the stream, and `reef op watch` prints it as JSON lines. | L | R2.02, R2.13, R2.27 |
| R2.10 | Presence channel | Ephemeral presence over the same connection, as a second stream beside the operation log's: `ReportPresence`, `ClearPresence` and `SubscribePresence`, entries with a TTL and caps, the current set then changes coalesced per entry, so a slow subscriber gets each entry's latest state. The daemon reports what only it knows: lease heartbeats (lapsing after two missed), files a workspace's snapshots change, and the progress of workspace creates and store builds. `reef presence watch`. Never persisted. | L | R2.09 |
| R2.11 | Agent presence reporting | `reef ws run` and `reef mcp` report presence automatically over R2.10's `ReportPresence`, under a reporting contract (in the API's comments, with a reference reporter in `reef-client`) that the SDKs follow. A presence API for agents that want to report finer activity: `reef presence report` and `clear`, with `--id` and `--id-file` so an entry outlives the process that reported it. `reef ws renew` renews a lease held outside `ws run`, and `reef here` suggests it in the grace period. | M | R2.10 |
| R2.12 | Replay | Reconstruct any live view at a past point from the operation log. Drives the scrubber in the UI. | M | R2.09 |

### Workstream: agent surface

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.13 | MCP server | `rmcp` server exposing the API as twenty tools named after their RPCs: workspace lifecycle (create, move, snapshot, release, with the leases of workspaces it creates heartbeated while it runs), status, log, changes, diff, files (read, list, edit), describe, abandon, the operation log and undo, and a wait tool over the operation log (polling until R2.09's subscription lands). No ADMIN RPC is a tool. Ships in the `reef` binary as `reef mcp`, zero-argument over stdio. Results are R2.27's JSON; errors are tool results carrying the daemon's `hint`, `next` and `metadata` (R1.26); `reef init` (R1.27) gains the `.mcp.json` renderer. The MCP registry listing is R2.25's. | L | R2.05, R2.27, R1.26, R1.27 |
| R2.14 | Symbol index ingestion | Tree-sitter tags for Python, TypeScript (with TSX and JavaScript), Go, and Rust extracted by the daemon with its own queries, in the background for the trees of new snapshots and edits and on demand, and SCIP documents uploaded for a revision, each checked against the file at that revision by digest. Definitions and references with ranges and enclosing definitions, stored per path and file content so unchanged files share entries across snapshots, with per-tree coverage, in a per-repository cache outside backup with a size cap and a GC step. `SymbolService` (index, upload, and list by path, name, or symbol) in a new `reef-symbols` crate and the daemon, `reef symbols index`, `upload`, and `list`; no MCP tool. ADR. | L | R2.02, R2.04, R2.27 |
| R2.15 | Symbol-level diff | `diff --symbols`: which symbols changed, added, removed, or moved, matched across the whole diff by symbol identity (renamed files included, up to 1,000 differing files), with each file's coverage (covered, unsupported, skipped) so callers know where the index is missing; which call sites are affected, as references in the new tree, exact through SCIP and by name through tags, bounded per symbol. Line-level on expand, one symbol's lines at a time. Fields on `GetDiff`, `reef diff --symbols` and `--expand`, and the MCP `get_diff` tool's `symbols` format and `expand` (its `tools/list` budget rises to 20,250 bytes). ADR. | L | R2.14, R2.13, R2.27 |
| R2.16 | Query layer | SQL-backed queries over changes, operations, and symbols: changes touching a symbol in a window, by agent, by outcome. A per-repository history index, outside backup like the symbol index, follows the operation log as it is recorded (with backfill, and GC's rewrite of operation ids) and keeps each change's outcome (open, landed on `trunk()`, abandoned, undone), its activity by agent and workspace, and the paths and symbols its net diff touches. `QueryChanges` in a new `HistoryService` filters by symbol, path, agent, workspace, change, outcome, and time window, pages, groups counts, and says what history it covers; `reef history` and an MCP `query_changes` tool (its `tools/list` budget rises to 21,250 bytes). `ListSymbols` by name reads in path order with its filters and cursor in SQL. ADR. | L | R2.14, R2.15, R2.09, R2.13 |
| R2.17 | Token-efficiency benchmark | Measure tokens an agent needs to answer standard questions (state, what changed, who touched this) via Reef versus git porcelain. Publish results, on RX.07's harness and results page. | S | R2.15, RX.07 |
| R2.24 | `reef schema` | The CLI's command tree and the API's services and messages as JSON Schema and a protobuf descriptor set, from one command, so a harness can generate tool definitions without a human writing them. | S | R2.01, R2.05, R2.27 |
| R2.25 | Harness packages | A Claude Code plugin (skill, MCP server entry, hooks) and the equivalents for Codex and opencode, built from R1.27's renderers and published by R1.28's release workflow, so one install line configures a harness for Reef. Lists `reef mcp` on the MCP registry from the same workflow: a `server.json` and a package the registry verifies (MCPB bundles on the GitHub release, or the crates on crates.io). | M | R1.27, R1.28, R2.13 |
| R2.26 | Workspace provider contract | A small documented interface (create, attach, release, with JSON in and out) and a reference adapter so a harness that creates git worktrees for isolation can ask Reef for a workspace instead; the upstream conversation with the first harness. | M | R2.13, R1.21 |

### Workstream: web UI v0

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.18 | Web UI scaffold | React and TypeScript app, served by the daemon, gRPC-web or WebSocket bridge for streams, state-color system from the design doc. | M | R2.09 |
| R2.19 | Smartlog view | Stacks and changes as a live graph. | M | R2.18 |
| R2.20 | Workspaces and fleet panel | Workspaces with owner, lease, disk, and presence. Agents and their current change. | M | R2.18, R2.10 |
| R2.21 | Activity feed with scrubber | Coalesced operation feed, counts always correct, replay via the scrubber. | M | R2.12, R2.18 |

### Workstream: scale

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.22 | Subscription load test and broker | A load generator, beside R1.23's, that opens N operation-log and presence subscribers over separate connections and drives M operations per minute through the daemon, measuring delivery latency per event from each operation's end and checking that nothing is missed, duplicated or reordered. Run at the phase target of 500 subscribers and 1,000 operations per minute, gated on p99 within 100 ms, in its own workflow. Put a broker between the operation log and the streams, in-process first, in its own crate with no jj-lib, so fan-out does not read the log per subscriber: one read per operation per repository into a bounded ring, sessions served from it and from the log only behind it, frames unchanged; the broker is the same component the store reuses in phase four. Presence keeps its in-memory hub, measured beside. The daemon raises its open-file limit and backs off accepts at it, so 500 clients can connect. Publish results, before and after the broker, with the benchmarks. | L | R2.10 |
| R2.23 | Incremental status index | Status answered from an index in SQLite that the watcher maintains per workspace, never from a tree walk: path, size, mtime, content hash when known, and dirty flag, updated from filesystem events and reconciled on snapshot. The status service reads the index and falls back to jj-lib's walk only when the index is marked stale, for example after a watcher overflow. Measured against jj-lib's walk on the large synthetic repository. | M | R2.02, R1.13 |

---

## Phase 3: Evidence, proposals, and landing

**Goal.** Changes carry signed evidence, proposals move through a lifecycle, a landing gate batches and verifies, conflicts are resolved by agents under policy, and humans work from a triage inbox.

**Exit criteria.**

- A proposal with valid evidence from a trusted attestor lands without human intervention under a permissive policy.
- A proposal with forged or untrusted evidence is refused with a structured reason.
- A batch containing one failing change bisects to the culprit and lands the rest.
- A stack lands as a prefix when a later change fails.
- Amending the bottom change of a stack rebases the changes above it, preserves their IDs, and leaves a conflict object on any that no longer applies.
- A conflict is resolved by an agent, enters the proposal flow, and lands under tier one policy.
- The triage inbox shows the needs-you-now bucket live, and the desktop app notifies on it.
- An agent's help request appears in the needs-you-now bucket and the human's reply resumes the agent's session.
- Landed history is pushed to an ordinary git remote while evidence, provenance, and conflicts stay local.
- A human on a phone receives a needs-you-now notification, opens the proposal, and approves it.

### Workstream: evidence and identity

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.01 | Evidence object format | in-toto attestation in a DSSE envelope: snapshot hash, attestor, kind, result, artifact hash. Stored content-addressed, linked from the change. | M | R2.03 |
| R3.02 | Evidence artifact store | Separate store for logs and build artifacts with a retention policy. Local implementation first. | M | R3.01 |
| R3.03 | Session keys and operator identity | Daemon issues short-lived ed25519 session keys bound to an operator or CI identity. Revocation. Sigstore keyless as an identity option. | L | R3.01 |
| R3.04 | Provenance records | Agent identity, model, session, task reference, instruction hash, signed with the session key, attached to snapshots. API and CLI to read them. | M | R3.03 |
| R3.05 | Executor protocol | The contract between the landing gate and an executor, written down before the reference implementation: a request names a tree hash, the check targets to run, the evidence kinds expected, and a budget; a response is a set of signed evidence objects plus artifacts; progress streams over the presence channel. Protobuf messages in the daemon schema. This is the protocol the design says Reef must get right so that sandboxing and remote execution stay outside Reef. | M | R3.01, R2.01 |
| R3.06 | Trusted executor reference implementation | Implements the executor protocol: runs a declared command on a tree hash in a sandbox, signs the result as evidence. Usable locally and in CI. | L | R3.05, R3.03 |

### Workstream: policy and proposals

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.07 | Policy file and CEL evaluation | `reef-policy.cel` or similar in the repo: trusted attestors, required evidence kinds, size limits, forbidden paths, sign-off areas, conflict tiers, attempt caps. Evaluated with `cel-rust`. | M | R3.01 |
| R3.08 | Policy simulation | Evaluate a proposed policy against recent landings and report what it would have blocked. API and CLI. | S | R3.07 |
| R3.09 | Change lifecycle states | Draft, proposed, queued, landed, declined, withdrawn, superseded, recorded as operations. Transitions in the API. | M | R2.03 |
| R3.10 | Stacks | Dependency edges on changes. Propose, review, and land a stack as one unit with prefix landing. | M | R3.09 |
| R3.11 | Dependent change lifecycle | A dependent proposed while its dependency is still proposed is a separate, linked proposal ordered by the edge at landing. Amending a dependency rebases dependents, preserving their IDs, and attaches a conflict object where the rebase does not apply. A declined or withdrawn dependency returns each dependent to its owner or the resolver queue with a conflict object; a superseded one rebases dependents onto the superseding change. | M | R3.10, R3.13 |
| R3.12 | Comments as instructions | Comment objects on proposals, marked as note or instruction. An instruction resumes the originating agent's session through a webhook or the MCP server and shows estimated cost before running. | M | R3.09, R2.13 |

### Workstream: landing

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.13 | Landing service, local | Lands onto a local main. Verifies evidence against policy, refuses conflicted changes, rebases, checks commutation. | L | R3.07, R3.09 |
| R3.14 | Batching and bisect | Batch rebased proposals, run the executor on the batch head, land on pass, bisect on failure. | L | R3.13, R3.06 |
| R3.15 | Evidence transfer rules | Policy-declared rules that accept evidence from the submitted tree for cheap checks when hunks commute. | S | R3.14 |
| R3.16 | Landed history projection | Squash and summarize by policy into the published history while preserving the operation log. | M | R3.13 |
| R3.17 | Degraded landing to a git remote | After a local landing, push the landed projection to a configured git remote through jj-lib's git support, so a team on GitHub sees landed history while evidence, provenance, and conflicts stay local. Refuses to push a conflicted change. This is the "degraded form against a git remote" the roadmap promises for phase three; the native wire protocol hardening is R4.14. | M | R3.16 |
| R3.18 | Minimal check set computation | Given the symbol-level diff, cached evidence for ancestors and siblings, and the check kinds policy requires, compute the smallest set of checks that satisfies policy and request only those from the executor. Fall back to the full set when the symbol index is missing. | L | R3.14, R2.15 |
| R3.19 | Per-target evidence | Evidence kinds scoped to a test or build target, so a passing target on a tree is one object and partial results compose across batches. | M | R3.01 |
| R3.20 | Compute budgets in policy | Executor time caps per proposal, per agent, and per repository per day. Exhausted budgets surface in the triage inbox. | M | R3.07, R3.14 |
| R3.21 | Landing queue priority | Proposals a human is waiting on are scheduled before proposals an agent is waiting on. Policy can add priority rules. | S | R3.14 |
| R3.22 | Behavioral diff evidence kind | The executor runs the suite on the base tree and on the proposal and attests to outcome changes: newly passing, newly failing, added, removed. Rendered in the proposal view. | M | R3.06, R3.19 |
| R3.23 | Test changes in the proposal view | Show tests added, changed, and deleted as a distinct section alongside the code, with deleted assertions called out. | M | R3.31 |
| R3.24 | Independent test authorship rules | Policy rules requiring, for designated paths, that tests come from a different change than the code, or that test changes need human sign-off. | S | R3.07, R3.10 |

### Workstream: conflicts

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.25 | Conflict objects in the API | Expose jj-lib's stored conflicts as structured records with base, sides, and each side's provenance. | M | R3.04 |
| R3.26 | Agent-scoped undo | Undo scoped to the operations an agent performed on its own changes, per the data model: an agent cannot undo another agent's operation without policy approval. Undoing an operation that another change has built on does not rewrite that change; it attaches a conflict object naming the undone operation to the dependent for its owner to resolve. Extends R2.03. | M | R3.25, R2.03 |
| R3.27 | Resolver assignment and queue | Assign to the originating agent of the later change by default. Resolver queue with priority for conflicts whose owner's session has ended. | M | R3.25 |
| R3.28 | Resolution tiers and attempt caps | Policy tiers: resolve and land, resolve and require approval, ask before attempting. Attempt cap per conflict. Resolutions enter the proposal flow marked as resolutions. | M | R3.27, R3.07 |
| R3.29 | Intent conflict escalation | Heuristics and an agent-facing prompt for detecting that two sides disagree on what to build, escalating to ask-first regardless of policy. | M | R3.28 |
| R3.30 | Backpressure | Refuse a second unresolved conflict ancestor without policy approval. | S | R3.28 |

### Workstream: human interface

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.31 | Proposal view | Intent, then evidence, then symbol-level diff with expand to lines. Accept, decline, comment. | L | R3.09, R2.15, R2.18 |
| R3.32 | Help requests from agents | An operation an agent calls to ask for a human: a reason, the change or conflict it concerns, what it tried, and an optional question. Recorded in the operation log, exposed through the API, CLI, and as an MCP tool, and resolved by a human reply that resumes the agent's session the way an instruction does. This is what produces the "agent asking for help" entries in the needs-you-now bucket. | S | R2.13, R3.12 |
| R3.33 | Triage inbox | Needs you now, watching, landed since you last looked. Live, with position held under the cursor and a visible new-items count. | M | R3.31, R3.32, R2.09 |
| R3.34 | Landing pipeline view | Queued, batched, testing, landed or bisecting, with batches as units. | M | R3.14, R2.18 |
| R3.35 | Conflict view | Base and both sides with provenance, resolver status, tier, attempts, and manual takeover. | M | R3.25, R2.18 |
| R3.36 | Map view with conflict warning | Repository tree as a surface with presence-driven activity, warning when two agents work in the same area. | L | R2.10, R2.18 |
| R3.37 | Audit and explain | For a line or symbol: task, agent, instruction, evidence, landing. Two history views. | M | R3.04, R2.16 |
| R3.38 | File browser with time axis and symbol search | Browse the tree at any snapshot, scrub along the operation log, and search by symbol using the symbol index. Each file links to its audit view. | M | R2.18, R2.14, R3.37 |
| R3.39 | Desktop shell | Tauri 2 app wrapping the web UI: bundled daemon, OS notifications for needs-you-now, menu bar fleet state, deep links, auto-update. macOS first. | L | R3.33 |
| R3.40 | Mobile shell and push notifications | The Tauri 2 shell built for iOS and Android with no bundled daemon, connecting to a remote daemon or, later, a store. Inbox, proposal view, help requests, instruction replies, and sign-offs laid out for phone width. Web Push from the daemon's remote listener for the needs-you-now bucket, with deep links into the exact proposal or conflict. Store-routed notifications arrive with R4.13. | L | R3.39, R3.33, R2.08 |
| R3.41 | Desktop shell on Linux and Windows | Platform ports, signing, and packaging. Windows depends on a workspace backend decision. | L | R3.39 |

---

## Phase 4: Shared lazy object store

**Goal.** A store server anyone can host, lazy-fetching clients, federation, hosted review, and hardened git interop so a team can share work without git in the middle.

**Exit criteria.**

- A fresh machine creates a workspace against a remote store without a full clone, fetching only touched files.
- Two stores federate: changes and evidence flow from a fork store to its upstream.
- Landing runs on the store with the same guarantees as phase three.
- A repository round-trips to git and back with landed history intact and change IDs stable across two importers.
- The web UI runs hosted on the store with authentication.
- A store backup taken while agents are active restores with no dangling blob reference, and garbage collection cannot remove a blob younger than the backup retention window.
- A daemon in ephemeral mode on a VM that is reclaimed mid-task loses no work: its drafts are in the store and the change resumes on a new machine.
- A sign-off from the hosted UI on a phone is signed with a store-issued identity and satisfies policy.
- One store serves 1,000 concurrent agents at 10,000 operations per minute with p99 API latency under 200 ms, and 50 agents proposing against one repository do not grow the landing queue without bound.

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R4.01 | Store protocol | Protobuf services for objects, changes, evidence, subscriptions, and landing between a daemon and a store. Versioned and documented. | M | R2.01 |
| R4.02 | Store server | Rust service, Postgres metadata, S3-compatible blob storage, the store protocol over gRPC. Health and readiness endpoints and operational metrics in Prometheus format. Docker image. | L | R4.01 |
| R4.03 | Reef object backend for jj-lib | A jj-lib `Backend` implementation over Reef's own object format: BLAKE3 identities, FastCDC-chunked blobs, packfile-style files on disk indexed in SQLite as the local cache, and the store protocol as the remote. Replaces the git backend for repositories that opt in or that are created against a store. Change IDs are unaffected. Migration of an existing git-backed repository rewrites commit identities once and records the mapping. | L | R4.01, R0.04 |
| R4.04 | Lazy fetch client cache | Daemon fetches trees on demand and blobs on first read, with a local cache and eviction. | L | R4.03 |
| R4.05 | Sparse profiles and prefetch hints | Repository-declared profiles that prefetch the subtrees a task will touch, so language servers and build tools do not defeat lazy fetch. | M | R4.04 |
| R4.06 | Lazy-fetch workspace backends | Workspace backends that materialize on first read instead of up front: NFS loopback via nfsserve on macOS, FUSE via fuser on Linux, and virtiofs for sandboxed microVMs, each implementing the backend trait from R1.02 and passing the conformance suite. Reads fall through to the lazy fetch cache. | L | R4.04, R1.11 |
| R4.07 | Publish and fetch proposals with evidence | Push a proposed change, its stack, evidence, and provenance to a store. Fetch others' proposals. | M | R4.02, R3.09 |
| R4.08 | Private draft changes and ephemeral daemon mode | Changes on the store carry a visibility: private drafts are visible only to the owner and never enter the landing queue until proposed. A daemon started in ephemeral mode streams every watcher snapshot to the store as a private draft and keeps its leases, workspace records, and operation log entries there, so a new machine resumes the change from the last snapshot. Draft retention and garbage collection on the store. Amends the local-only rule from R1.16 for this mode only. | L | R4.07, R1.16, R1.04 |
| R4.09 | Store authentication and authorization | Identity via the same operator identities as phase three. Per-repository roles. | M | R4.02, R3.03 |
| R4.10 | Store-issued session keys | The store issues short-lived session keys bound to an operator identity for humans acting from a device with no daemon, so sign-offs from the hosted web UI or the mobile app are signed and satisfy policy the same way a daemon-issued key would. Revocation, and the same key format and verification path as R3.03. | M | R4.09, R3.03 |
| R4.11 | Landing service on the store | Move the landing service from local to the store. Executor integration over the network. | L | R4.07, R3.14 |
| R4.12 | Federation | A store tracks another store. Proposals and evidence flow between them with signatures intact. | L | R4.11 |
| R4.13 | Hosted web UI | Serve the web UI from the store with authentication, pointing at store subscriptions. Push notifications routed through the store to the mobile shell, replacing the daemon-sourced Web Push from R3.40 when a store is present. | M | R4.09, R3.33, R3.40 |
| R4.14 | Git wire protocol hardening via gitoxide | Fetch and push against git remotes natively through gitoxide, which jj-lib already uses, with no subprocess path: SSH and HTTPS authentication, credential helpers, large-repository performance, partial clones, and retries. R3.17 delivers the first push; this makes it production grade. | L | R3.17 |
| R4.15 | Deterministic change IDs on import | Derive change IDs from git commit hashes so two importers agree. | S | R4.14 |
| R4.16 | Clean export of landed history | Export the landed projection, not the operation log, to git. Round-trip test suite on real repositories. | L | R4.14, R3.16 |
| R4.17 | Signing and release engineering | Code signing and notarization of the binaries R1.28 already builds and publishes (release workflow, installer, Homebrew tap, binstall), and App Store and Play Store distribution for the mobile shell from R3.40, including signing, review submission, and update cadence. Can start during phase two; the mobile part waits for R3.40. | M | R1.28, R2.05, R3.40 |
| R4.18 | Store backup and restore | Documented and scripted procedure: object storage first, then a Postgres snapshot, so no backup contains a dangling reference. Point-in-time recovery and bucket versioning configured in the Docker and reference deployment. A backup retention window setting that garbage collection enforces by never deleting a blob younger than it. The store's landing signing key included in the metadata backup. Restore test with writers active during the backup, asserting every referenced blob is present. | M | R4.02 |
| R4.19 | Store load test | Drive the phase target against a store: 1,000 agents across repositories at 10,000 operations per minute, measuring p99 API latency, subscription delivery latency through the broker from R2.22, and landing queue depth with 50 agents proposing against one repository. Publish results and use them to decide whether repository sharding across database instances is needed before the first public store. | M | R4.11, R2.22 |

---

## Phase 5: Structural merge

**Goal.** The merge engine resolves most agent concurrency without conflict objects, with measured fallback rates per language.

**Exit criteria.**

- Patch commutation merges disjoint changes with no inspection.
- Mergiraf-based structural merge handles overlapping insertions in Python, TypeScript, Go, and Rust.
- Lockfile conflicts are resolved by regeneration.
- Fallback rate per language is reported and under a target the team sets after measurement.

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R5.01 | Merge engine interface | Tiered merge pipeline with a result type that is either a merged tree or a conflict object. Pluggable tiers. | M | R3.25 |
| R5.02 | Patch commutation tier | Detect disjoint hunks and merge without inspection. Property-based tests for commutation. | M | R5.01 |
| R5.03 | Mergiraf integration | Structural merge as a tier. Vendor or depend on Mergiraf, map its output into Reef's conflict objects. | L | R5.01 |
| R5.04 | Generated file drivers | Lockfile driver that merges manifests and regenerates for npm, pnpm, Cargo, Go modules, uv, and others declared in the workspace spec. | M | R5.01 |
| R5.05 | Formatting before hashing | Opt-in per repository: run the declared formatter before snapshotting so whitespace churn never reaches merge. | S | R1.13 |
| R5.06 | Fallback telemetry | Count tier outcomes per language and surface them in the query layer and the fleet panel. | S | R5.03, R2.16 |
| R5.07 | Agent-pattern merge corpus | Collect real overlapping-insertion cases from agent output and use them as the regression suite for tuning rules. | M | R5.03 |
| R5.08 | Language rule tuning | Per-language passes for Python, TypeScript, Go, and Rust against the corpus. One ticket per language once R5.07 exists. | M each | R5.07 |

---

## Phase 6: Protocol specification and conformance

**Goal.** Alternative stores and clients can exist. The protocol is the product.

**Exit criteria.**

- A written specification for the daemon API, the store protocol, evidence, and policy that a second implementer could build from.
- A conformance suite that the reference daemon and store pass, runnable against any implementation.
- Governance documented: how decisions are made, how the spec changes.

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R6.01 | Daemon API specification | Normative description of every service, message, and error, derived from the protobuf schema plus semantics the schema cannot express. | L | R4.17 |
| R6.02 | Store protocol specification | Same for the store protocol and federation. | L | R4.12 |
| R6.03 | Evidence and policy specification | Attestation format, trust model, policy language surface, and the executor protocol from R3.05. | M | R3.07, R3.05 |
| R6.04 | Conformance suite | Black-box tests against any daemon or store endpoint. Reference implementations pass in CI. | L | R6.01, R6.02 |
| R6.05 | Governance documents | Decision process, spec change process, maintainer roles, security disclosure policy. | S | |
| R6.06 | Second-implementer guide | What a minimal conformant client and store need. Written by attempting one. | M | R6.04 |

---

## Cross-cutting tickets

These do not belong to a phase and should be scheduled alongside the phase where they first matter.

| ID | Title | Scope | Size | First needed |
| --- | --- | --- | --- | --- |
| RX.01 | Security review of workspace isolation | The Phase 1 threat model (`docs/threat-model.md`, ADR 0027): one uid is one trust domain, so the boundaries are other local users, repository content, and the installer, not a same-uid agent. Fixed before 0.1.0: state-directory ownership and modes, registration refusing repositories owned by another user, store builds announced and switchable off, a root daemon warning, release provenance, and, found by its hostile-content tests, commits with tree entries that panic jj-lib refused before jj reads them and no snapshot through a workspace root replaced by a symlink; documented as limitations: lease ids as the only credential, `chmod`-sealed stores, build network access. Requirements for R2.08's remote listener; no external review until RX.02. Brief in `dev-tickets/phase-1/`. | M | Phase 1 release |
| RX.02 | Security review of evidence and identity | Forged evidence, key handling, executor sandbox escape. External review if possible. | L | Phase 3 |
| RX.03 | Opt-in telemetry | Anonymous usage and performance metrics with a clear opt-in. Needed to measure fallback rates and token savings in the wild. | M | Phase 2 |
| RX.04 | Documentation site | Hosted docs from the `docs/` directory, versioned with releases, with `llms.txt` and `llms-full.txt` at the root and the agent guide (R1.26) at a stable URL, so an agent with web access finds the answer in one fetch. | M | Phase 1 release |
| RX.05 | Windows support decision | Whether and how to support Windows workspaces. No copy-on-write primitive comparable to APFS or overlayfs; options are ReFS block cloning, a VFS, or Linux-only via WSL. ADR. | S | Phase 1 |
| RX.06 | Performance regression gate | Benchmarks from R0.06 and R1.12 run in CI with a threshold that fails the build. | S | Phase 1 |
| RX.07 | Agent onboarding benchmark | A real agent (Claude Code headless first, Codex second) in a fresh container, with no Reef configuration, after `reef init`, from nothing, and inside `ws run`: tool calls, failed `reef` invocations, tokens, and cost to a working workspace, three trials, on demand, published like the benchmarks. Phase 1's agent exit criterion. Brief in `dev-tickets/phase-1/`; results in [`onboarding-benchmark.md`](onboarding-benchmark.md). | M | Phase 1 release |

---

## Suggested milestone order

1. Phase 0 in full.
2. Phase 1 in full. This is the first public release and the first thing anyone can use.
3. Phase 2 API and live state workstreams, then the agent surface, then web UI v0. R4.17 release engineering runs alongside.
4. Phase 3 evidence and policy first, then landing, then conflicts, then the human interface. The desktop shell is last in the phase.
5. Phase 4 store and lazy fetch first, then landing on the store, then federation, then git interop hardening.
6. Phase 5 can start its merge engine interface and commutation tier during phase 3, since they only depend on conflict objects.
7. Phase 6 after phase 4 settles.
