# Reef development phases

*Companion to [design.md](design.md). Last updated 2026-10-15.*

This document breaks the design into phases and the phases into tickets. Each ticket row is meant to become one issue. Phases map to GitHub milestones, and a phase's exit criteria are the milestone's acceptance test.

**How to read the tables**

- **ID** is stable and should go in the issue title, for example `R1.04 overlayfs backend`.
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
| R0.01 | Workspace layout and crate roles | Settle the Cargo workspace: `reef-lib` (data model, workspaces, merge), `reef-daemon` (service, API), `reef-vcs` (the `reef` binary and umbrella). Document crate boundaries in a short ADR. | S | |
| R0.02 | Toolchain pinning and dev tooling | `rust-toolchain.toml`, a `justfile` or equivalent for build, test, lint, and bench, rustfmt and clippy config, pre-commit hook. | S | R0.01 |
| R0.03 | CI on macOS and Linux | GitHub Actions matrix: build, test, clippy, fmt. Cache cargo. Fail on warnings. | S | R0.02 |
| R0.04 | jj-lib embedding spike | Pin a jj-lib version. Open an existing jj repo, snapshot the working copy, list changes, read the op log, undo one op. Record what jj-lib exposes and what it does not in an ADR. | M | R0.01 |
| R0.05 | Synthetic repository generator | A tool that generates a repo with N files, configurable depth and file sizes, plus a few large binaries, for benchmarks and tests. | S | R0.01 |
| R0.06 | Benchmark harness | Criterion setup with one benchmark that snapshots the synthetic repo. Results published as CI artifacts. | S | R0.04, R0.05 |
| R0.07 | ADR directory and template | `docs/adr/` with a template. Backfill ADRs for the choices already made in design.md: jj-lib, SQLite, gRPC, Tauri, CEL. | S | |
| R0.08 | Contributor docs | `CONTRIBUTING.md`, issue and PR templates, code of conduct, DCO or CLA decision. | S | |
| R0.09 | Error and logging conventions | Choose error types (`thiserror` in lib, `anyhow` at edges), `tracing` setup, structured log format. Document in an ADR. | S | R0.01 |

---

## Phase 1: Leased copy-on-write workspaces

**Goal.** A daemon and CLI that replace git worktrees for agents on existing jj repositories: workspaces that cost only what the agent changed, are snapshotted continuously, and reclaim themselves when the agent goes away.

**Exit criteria.**

- Twenty workspaces on the 100k-file synthetic repo, each created in under one second on APFS and overlayfs, with disk usage per workspace proportional to modified files.
- Killing an agent process mid-edit leaves a snapshot in the store and no directory on disk after the lease expires.
- Two workspaces on the same change work without interfering.
- A shared dependency store is mounted read-only into every workspace and verified against a lockfile hash.
- Works on macOS and Linux with the same CLI.

### Workstream: workspace model and daemon skeleton

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.01 | Workspace schema in SQLite | Tables for workspaces (id, owner, change ID, purpose, backend, path, lease expiry, created, last snapshot), leases, and reclaim log. Migrations. | S | R0.04 |
| R1.02 | Materialization backend trait | `Backend` trait: materialize a snapshot at a path, capture the writable layer as a tree, tear down. Capability detection per platform and filesystem. | S | R1.01 |
| R1.03 | Daemon skeleton over a Unix socket | Long-running `reef-daemon` with tonic gRPC over a Unix socket, health check, graceful shutdown, single-instance lock, structured logs. | M | R0.09 |
| R1.04 | Workspace service: create, list, release | gRPC methods and protobuf messages for creating a workspace on a change, listing with owner and lease state, releasing. Protobuf schema lives in `reef-daemon/proto`. | M | R1.02, R1.03 |

### Workstream: materialization backends

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.05 | Plain copy fallback backend | Full copy materialization. Exists so every platform works and as the baseline for benchmarks. | S | R1.02 |
| R1.06 | APFS clone backend (macOS) | Materialize a base snapshot once per machine, then `clonefile` per file into the workspace. Base cache keyed by snapshot hash with eviction. | M | R1.02 |
| R1.07 | overlayfs backend (Linux) | Read-only lower from a materialized base, writable upper per workspace. Handle unprivileged mounts via user namespaces; fall back to fuse-overlayfs where kernel overlayfs is unavailable. | L | R1.02 |
| R1.08 | Reflink backend (btrfs, XFS) | `FICLONE` per file. Shares the base cache with the APFS backend. | S | R1.06 |
| R1.09 | Backend conformance tests | One test suite every backend must pass: materialize, modify, capture, tear down, large files, symlinks, permissions, case sensitivity. | M | R1.05 |
| R1.10 | Workspace creation benchmarks | Time and disk for 1, 20, and 100 workspaces per backend on the synthetic repo. Published from CI on both platforms. | S | R1.09, R0.06 |

### Workstream: snapshotting and leases

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.11 | Watcher-driven snapshots | `notify` watcher per workspace with debounce. Capture the writable layer and snapshot through jj-lib. Ignore rules from the repo plus a Reef-specific ignore file. | M | R1.04, R0.04 |
| R1.12 | Watchman backend (optional) | Use Watchman when present for large repos. Same interface as the notify watcher. | M | R1.11 |
| R1.13 | Lease manager | Heartbeat RPC, expiry loop, configurable lease length and grace. On expiry: final snapshot, op log entry, reclaim. Daemon restart preserves leases and applies a grace period before reclaiming anything. | M | R1.04, R1.11 |
| R1.14 | Local-only snapshot marking | Snapshots taken by the watcher are marked local and never exported or pushed until a change is explicitly published. | S | R1.11 |
| R1.15 | Garbage collection | Reclaim snapshots unreferenced by any change and older than the undo window. Reclaim base cache entries no workspace uses. Report what was freed. | M | R1.13 |

### Workstream: shared stores and agent integration

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R1.16 | Workspace spec file | `reef.toml` in the repo declaring shared stores to mount, build output paths to keep out of snapshots, and lease defaults. | S | R1.01 |
| R1.17 | Shared dependency store mounting | Mount or link declared stores into workspaces read-only. Verify against a lockfile hash before mounting. Daemon-only writes. | M | R1.16, R1.06, R1.07 |
| R1.18 | `reef ws run` | Run a command inside a leased workspace with automatic heartbeat for the process lifetime and release on exit. The primary agent entry point for phase one. | S | R1.13 |
| R1.19 | CLI for workspaces | `reef ws create`, `list`, `release`, `gc`, `inspect`, with JSON output when stdout is not a terminal. Thin client of the daemon. | M | R1.04, R1.15 |
| R1.20 | Crash and reclaim integration tests | Kill agents mid-edit, kill the daemon, fill the disk, expire leases, and assert the invariants from the exit criteria. | M | R1.13, R1.17 |
| R1.21 | Quickstart documentation | Install, point at an existing jj repo, run an agent in a workspace, watch it reclaim. | S | R1.19 |

---

## Phase 2: Typed API, MCP server, and live state

**Goal.** Agents and humans use one typed API for everything, state streams in real time, and the first web UI shows what the fleet is doing.

**Exit criteria.**

- The CLI is a thin client with no direct jj-lib access.
- An MCP server exposes the same operations as the API.
- A client can subscribe to the operation log and presence channel and receive events within 100 ms.
- Symbol-level diffs work for Python, TypeScript, Go, and Rust.
- A local web UI shows the smartlog, workspaces, and fleet panel live.
- Token cost to answer "what is the state of this repo" is measured and at least five times lower than parsing git output.

### Workstream: API

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.01 | Core protobuf schema | Messages and services for status, snapshot, diff, log, changes, undo, and workspaces. Versioned. Reviewed as an API, not generated from code. | M | R1.04 |
| R2.02 | Status, log, and change services | Implement the read side of the schema over jj-lib. Structured, paginated responses. | M | R2.01 |
| R2.03 | Snapshot, describe, and undo services | The write side: snapshot now, set change description, undo an operation, with each response returning the op log entry created. | M | R2.01 |
| R2.04 | Virtual workspace API | Read a file at a snapshot, apply an edit, receive a new snapshot hash, without materializing a directory. | M | R2.03 |
| R2.05 | CLI as thin client | Rewrite all CLI commands over the gRPC API. Remove jj-lib from the binary crate. | M | R2.02, R2.03 |
| R2.06 | Python SDK | Generated from the schema, published to PyPI as `reef-vcs`, with a thin idiomatic wrapper. | S | R2.01 |
| R2.07 | TypeScript SDK | Generated from the schema, published to npm, with a thin wrapper. | S | R2.01 |

### Workstream: live state

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.08 | Operation log subscription | Server-streaming RPC filtered by repository, change, workspace, or agent. Resume from a given operation. Server-side batching into frames. | M | R2.02 |
| R2.09 | Presence channel | Ephemeral presence over the same connection: agent activity, workspace being edited, lease heartbeats, long-running operation progress. Never persisted. | M | R2.08 |
| R2.10 | Agent presence reporting | `reef ws run` and the SDKs report presence automatically. A presence API for agents that want to report finer activity. | S | R2.09 |
| R2.11 | Replay | Reconstruct any live view at a past point from the operation log. Drives the scrubber in the UI. | M | R2.08 |

### Workstream: agent surface

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.12 | MCP server | `rmcp` server exposing the API as tools, including workspace lifecycle, status, diff, snapshot, and subscriptions as a wait tool. Ships in the `reef` binary as `reef mcp`. | M | R2.05 |
| R2.13 | Symbol index ingestion | Accept SCIP indexes and tree-sitter tag output per snapshot. Store symbol locations keyed by snapshot. | L | R2.02 |
| R2.14 | Symbol-level diff | `diff --symbols`: which symbols changed, added, or removed, and which call sites are affected. Line-level on expand. | M | R2.13 |
| R2.15 | Query layer | SQL-backed queries over changes, operations, and symbols: changes touching a symbol in a window, by agent, by outcome. Exposed through the API. | M | R2.13 |
| R2.16 | Token-efficiency benchmark | Measure tokens an agent needs to answer standard questions (state, what changed, who touched this) via Reef versus git porcelain. Publish results. | S | R2.14 |

### Workstream: web UI v0

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R2.17 | Web UI scaffold | React and TypeScript app, served by the daemon, gRPC-web or WebSocket bridge for streams, state-color system from the design doc. | M | R2.08 |
| R2.18 | Smartlog view | Stacks and changes as a live graph. | M | R2.17 |
| R2.19 | Workspaces and fleet panel | Workspaces with owner, lease, disk, and presence. Agents and their current change. | M | R2.17, R2.09 |
| R2.20 | Activity feed with scrubber | Coalesced operation feed, counts always correct, replay via the scrubber. | M | R2.11, R2.17 |

---

## Phase 3: Evidence, proposals, and landing

**Goal.** Changes carry signed evidence, proposals move through a lifecycle, a landing gate batches and verifies, conflicts are resolved by agents under policy, and humans work from a triage inbox.

**Exit criteria.**

- A proposal with valid evidence from a trusted attestor lands without human intervention under a permissive policy.
- A proposal with forged or untrusted evidence is refused with a structured reason.
- A batch containing one failing change bisects to the culprit and lands the rest.
- A stack lands as a prefix when a later change fails.
- A conflict is resolved by an agent, enters the proposal flow, and lands under tier one policy.
- The triage inbox shows the needs-you-now bucket live, and the desktop app notifies on it.

### Workstream: evidence and identity

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.01 | Evidence object format | in-toto attestation in a DSSE envelope: snapshot hash, attestor, kind, result, artifact hash. Stored content-addressed, linked from the change. | M | R2.03 |
| R3.02 | Evidence artifact store | Separate store for logs and build artifacts with a retention policy. Local implementation first. | M | R3.01 |
| R3.03 | Session keys and operator identity | Daemon issues short-lived ed25519 session keys bound to an operator or CI identity. Revocation. Sigstore keyless as an identity option. | L | R3.01 |
| R3.04 | Provenance records | Agent identity, model, session, task reference, instruction hash, signed with the session key, attached to snapshots. API and CLI to read them. | M | R3.03 |
| R3.05 | Trusted executor reference implementation | Runs a declared command on a tree hash in a sandbox, signs the result as evidence. Usable locally and in CI. | L | R3.01, R3.03 |

### Workstream: policy and proposals

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.06 | Policy file and CEL evaluation | `reef-policy.cel` or similar in the repo: trusted attestors, required evidence kinds, size limits, forbidden paths, sign-off areas, conflict tiers, attempt caps. Evaluated with `cel-rust`. | M | R3.01 |
| R3.07 | Policy simulation | Evaluate a proposed policy against recent landings and report what it would have blocked. API and CLI. | S | R3.06 |
| R3.08 | Change lifecycle states | Draft, proposed, queued, landed, declined, withdrawn, superseded, recorded as operations. Transitions in the API. | M | R2.03 |
| R3.09 | Stacks | Dependency edges on changes. Propose, review, and land a stack as one unit with prefix landing. | M | R3.08 |
| R3.10 | Comments as instructions | Comment objects on proposals, marked as note or instruction. An instruction resumes the originating agent's session through a webhook or the MCP server and shows estimated cost before running. | M | R3.08, R2.12 |

### Workstream: landing

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.11 | Landing service, local | Lands onto a local main. Verifies evidence against policy, refuses conflicted changes, rebases, checks commutation. | L | R3.06, R3.08 |
| R3.12 | Batching and bisect | Batch rebased proposals, run the executor on the batch head, land on pass, bisect on failure. | L | R3.11, R3.05 |
| R3.13 | Evidence transfer rules | Policy-declared rules that accept evidence from the submitted tree for cheap checks when hunks commute. | S | R3.12 |
| R3.14 | Landed history projection | Squash and summarize by policy into the published history while preserving the operation log. | M | R3.11 |
| R3.28 | Minimal check set computation | Given the symbol-level diff, cached evidence for ancestors and siblings, and the check kinds policy requires, compute the smallest set of checks that satisfies policy and request only those from the executor. Fall back to the full set when the symbol index is missing. | L | R3.12, R2.14 |
| R3.29 | Per-target evidence | Evidence kinds scoped to a test or build target, so a passing target on a tree is one object and partial results compose across batches. | M | R3.01 |
| R3.30 | Compute budgets in policy | Executor time caps per proposal, per agent, and per repository per day. Exhausted budgets surface in the triage inbox. | M | R3.06, R3.12 |
| R3.31 | Landing queue priority | Proposals a human is waiting on are scheduled before proposals an agent is waiting on. Policy can add priority rules. | S | R3.12 |

### Workstream: conflicts

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.15 | Conflict objects in the API | Expose jj-lib's stored conflicts as structured records with base, sides, and each side's provenance. | M | R3.04 |
| R3.16 | Resolver assignment and queue | Assign to the originating agent of the later change by default. Resolver queue with priority for conflicts whose owner's session has ended. | M | R3.15 |
| R3.17 | Resolution tiers and attempt caps | Policy tiers: resolve and land, resolve and require approval, ask before attempting. Attempt cap per conflict. Resolutions enter the proposal flow marked as resolutions. | M | R3.16, R3.06 |
| R3.18 | Intent conflict escalation | Heuristics and an agent-facing prompt for detecting that two sides disagree on what to build, escalating to ask-first regardless of policy. | M | R3.17 |
| R3.19 | Backpressure | Refuse a second unresolved conflict ancestor without policy approval. | S | R3.17 |

### Workstream: human interface

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R3.20 | Proposal view | Intent, then evidence, then symbol-level diff with expand to lines. Accept, decline, comment. | L | R3.08, R2.14, R2.17 |
| R3.21 | Triage inbox | Needs you now, watching, landed since you last looked. Live, with position held under the cursor and a visible new-items count. | M | R3.20, R2.08 |
| R3.22 | Landing pipeline view | Queued, batched, testing, landed or bisecting, with batches as units. | M | R3.12, R2.17 |
| R3.23 | Conflict view | Base and both sides with provenance, resolver status, tier, attempts, and manual takeover. | M | R3.15, R2.17 |
| R3.24 | Map view with conflict warning | Repository tree as a surface with presence-driven activity, warning when two agents work in the same area. | L | R2.09, R2.17 |
| R3.25 | Audit and explain | For a line or symbol: task, agent, instruction, evidence, landing. Two history views. | M | R3.04, R2.15 |
| R3.26 | Desktop shell | Tauri 2 app wrapping the web UI: bundled daemon, OS notifications for needs-you-now, menu bar fleet state, deep links, auto-update. macOS first. | L | R3.21 |
| R3.27 | Desktop shell on Linux and Windows | Platform ports, signing, and packaging. Windows depends on a workspace backend decision. | L | R3.26 |

---

## Phase 4: Shared lazy object store

**Goal.** A store server anyone can host, lazy-fetching clients, federation, hosted review, and hardened git interop so a team can share work without git in the middle.

**Exit criteria.**

- A fresh machine creates a workspace against a remote store without a full clone, fetching only touched files.
- Two stores federate: changes and evidence flow from a fork store to its upstream.
- Landing runs on the store with the same guarantees as phase three.
- A repository round-trips to git and back with landed history intact and change IDs stable across two importers.
- The web UI runs hosted on the store with authentication.

| ID | Title | Scope | Size | Depends on |
| --- | --- | --- | --- | --- |
| R4.01 | Store protocol | Protobuf services for objects, changes, evidence, subscriptions, and landing between a daemon and a store. Versioned and documented. | M | R2.01 |
| R4.02 | Store server | Rust service, Postgres metadata, S3-compatible blob storage, the store protocol over gRPC. Docker image. | L | R4.01 |
| R4.03 | Lazy fetch client cache | Daemon fetches trees on demand and blobs on first read, with a local cache and eviction. | L | R4.01 |
| R4.04 | Sparse profiles and prefetch hints | Repository-declared profiles that prefetch the subtrees a task will touch, so language servers and build tools do not defeat lazy fetch. | M | R4.03 |
| R4.05 | Publish and fetch proposals with evidence | Push a proposed change, its stack, evidence, and provenance to a store. Fetch others' proposals. | M | R4.02, R3.08 |
| R4.06 | Store authentication and authorization | Identity via the same operator identities as phase three. Per-repository roles. | M | R4.02, R3.03 |
| R4.07 | Landing service on the store | Move the landing service from local to the store. Executor integration over the network. | L | R4.05, R3.12 |
| R4.08 | Federation | A store tracks another store. Proposals and evidence flow between them with signatures intact. | L | R4.07 |
| R4.09 | Hosted web UI | Serve the web UI from the store with authentication, pointing at store subscriptions. | M | R4.06, R3.21 |
| R4.10 | Git wire protocol via gitoxide | Fetch and push against git remotes natively. No shelling out. | L | R0.04 |
| R4.11 | Deterministic change IDs on import | Derive change IDs from git commit hashes so two importers agree. | S | R4.10 |
| R4.12 | Clean export of landed history | Export the landed projection, not the operation log, to git. Round-trip test suite on real repositories. | L | R4.10, R3.14 |
| R4.13 | Homebrew tap and release engineering | Signed binaries for macOS and Linux, a Homebrew tap, release automation, changelog. Can start during phase two. | M | R2.05 |

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
| R5.01 | Merge engine interface | Tiered merge pipeline with a result type that is either a merged tree or a conflict object. Pluggable tiers. | M | R3.15 |
| R5.02 | Patch commutation tier | Detect disjoint hunks and merge without inspection. Property-based tests for commutation. | M | R5.01 |
| R5.03 | Mergiraf integration | Structural merge as a tier. Vendor or depend on Mergiraf, map its output into Reef's conflict objects. | L | R5.01 |
| R5.04 | Generated file drivers | Lockfile driver that merges manifests and regenerates for npm, pnpm, Cargo, Go modules, uv, and others declared in the workspace spec. | M | R5.01 |
| R5.05 | Formatting before hashing | Opt-in per repository: run the declared formatter before snapshotting so whitespace churn never reaches merge. | S | R1.11 |
| R5.06 | Fallback telemetry | Count tier outcomes per language and surface them in the query layer and the fleet panel. | S | R5.03, R2.15 |
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
| R6.01 | Daemon API specification | Normative description of every service, message, and error, derived from the protobuf schema plus semantics the schema cannot express. | L | R4.13 |
| R6.02 | Store protocol specification | Same for the store protocol and federation. | L | R4.08 |
| R6.03 | Evidence and policy specification | Attestation format, trust model, policy language surface. | M | R3.06 |
| R6.04 | Conformance suite | Black-box tests against any daemon or store endpoint. Reference implementations pass in CI. | L | R6.01, R6.02 |
| R6.05 | Governance documents | Decision process, spec change process, maintainer roles, security disclosure policy. | S | |
| R6.06 | Second-implementer guide | What a minimal conformant client and store need. Written by attempting one. | M | R6.04 |

---

## Cross-cutting tickets

These do not belong to a phase and should be scheduled alongside the phase where they first matter.

| ID | Title | Scope | Size | First needed |
| --- | --- | --- | --- | --- |
| RX.01 | Security review of workspace isolation | Shared stores as a lateral movement path, daemon socket permissions, lease hijacking. | M | Phase 1 |
| RX.02 | Security review of evidence and identity | Forged evidence, key handling, executor sandbox escape. External review if possible. | L | Phase 3 |
| RX.03 | Opt-in telemetry | Anonymous usage and performance metrics with a clear opt-in. Needed to measure fallback rates and token savings in the wild. | M | Phase 2 |
| RX.04 | Documentation site | Hosted docs from the `docs/` directory, versioned with releases. | M | Phase 1 release |
| RX.05 | Windows support decision | Whether and how to support Windows workspaces. No copy-on-write primitive comparable to APFS or overlayfs; options are ReFS block cloning, a VFS, or Linux-only via WSL. ADR. | S | Phase 1 |
| RX.06 | Performance regression gate | Benchmarks from R0.06 and R1.10 run in CI with a threshold that fails the build. | S | Phase 1 |

---

## Suggested milestone order

1. Phase 0 in full.
2. Phase 1 in full. This is the first public release and the first thing anyone can use.
3. Phase 2 API and live state workstreams, then the agent surface, then web UI v0. R4.13 release engineering runs alongside.
4. Phase 3 evidence and policy first, then landing, then conflicts, then the human interface. The desktop shell is last in the phase.
5. Phase 4 store and lazy fetch first, then landing on the store, then federation, then git interop hardening.
6. Phase 5 can start its merge engine interface and commutation tier during phase 3, since they only depend on conflict objects.
7. Phase 6 after phase 4 settles.
