# Phase 2 tickets

Source of truth for scope: [`docs/dev-phases.md`](../../dev-phases.md) (Phase 2 tables) and [`docs/design.md`](../../design.md) (API, Subscriptions, Architecture, Scale). These files expand each Phase 2 row into an implementation brief. They are written for an implementing agent, not for humans: every decision that could be made in advance has been made here, and the brief says so. If a brief and `dev-phases.md` disagree on scope, the brief wins for the implementation and the disagreement is fixed in `dev-phases.md` in the same PR. Where a brief departs from `dev-phases.md`, the departure is listed under "Departures from dev-phases.md" below, and `dev-phases.md` was edited to match when the brief was written. Briefs are added as each ticket starts, so the table below grows with them.

The schema R2.01 writes is the authority for every later Phase 2 ticket that touches the API. A later brief implements the RPCs R2.01 declared and adds fields, values and RPCs. It never changes what R2.01 declared; a declaration that proves wrong is deprecated and replaced (ADR 0029).

Phase 1 is complete and released as 0.1.0 (2026-10-25). What it left behind, and what every Phase 2 brief builds on:

- **The v0 API** (`reef.daemon.v0`, ADR 0017): `Daemon`, `Repositories`, `Workspaces` (create, list, inspect, release, snapshot, heartbeat, move), `Maintenance` (GC, backup, quiesce, resume) and `Stores` (build, list), generated with `protox` and `tonic-prost-build` in `crates/reef-daemon/build.rs`. Errors carry `ErrorDetail { kind, chain, hint, next }`, with `ErrorKind` from `reef_lib::error` (ADR 0009) and advice from `Advise` (ADR 0025). R2.01 replaces this API with `reef.api.v1`.
- **The client and the CLI**: `reef_daemon::client::Client` (one method per RPC), `client::json` (the CLI's stable JSON), and the `reef` CLI under the agent-facing contract (ADR 0025: never interactive, `hint` and `next` on errors, `reef here`, additive names, exit codes by kind). The CLI still links jj-lib through `reef-daemon` (`reef daemon run`); R2.05 removes that.
- **The daemon**: one process per user over a Unix socket (ADR 0012, 0013); jj workspaces as Reef workspaces (ADR 0014); copy-on-write backends and the base cache (ADR 0015, 0016); watcher snapshots with narrowed scans, Watchman for large repositories, and watch attachment off the create path (ADR 0018, 0023, 0026, 0028); the snapshot index with local marking (R1.16); leases and reclaim (ADR 0019); backup (ADR 0020); spec files and shared stores (ADR 0021, 0022); onboarding (ADR 0024).
- **`reef_lib::jj`** over jj-lib 0.46.0: open, snapshot (split into scan and commit), operation log walk, `undo_last`, revision resolution (change id, commit id, bookmark; no revsets), materialization, GC.
- **The trust model** (`docs/threat-model.md`, ADR 0027): one uid is one trust domain, the socket is the authorization, and a remote listener (R2.08) must meet the listed requirements before it serves anything.
- **Measurement**: the criterion benches and `docs/benchmarks.md` (R1.12, R1.29, R1.32), the load test (R1.23, R1.30, `docs/load-tests.md`), and the onboarding benchmark's harness (RX.07, `docs/onboarding-benchmark.md`), which R2.17 reuses.
- **CI** on `ubuntu-24.04` and `macos-15`, a privileged Linux job for overlayfs, and tag-driven releases with an installer and a Homebrew tap (R1.28). macOS never compiles `cfg(target_os = "linux")` code; the privileged `ubuntu:24.04` container in `docs/benchmarks.md` does.

## Tickets

| ID | File | Size | Depends on | Unblocks |
| --- | --- | --- | --- | --- |
| R2.01 | [R2.01-core-protobuf-schema.md](R2.01-core-protobuf-schema.md) | L | R1.05 | R2.02, R2.03, R2.04, R2.05, R2.06, R2.07, R2.08, R2.09, R2.10, R2.11, R2.12, R2.24, R3.05 |
| R2.02 | [R2.02-status-log-and-change-services.md](R2.02-status-log-and-change-services.md) | L | R2.01 | R2.03, R2.05, R2.09, R2.12, R2.14, R2.23 |
| R2.03 | [R2.03-snapshot-describe-and-undo-services.md](R2.03-snapshot-describe-and-undo-services.md) | L | R2.01, R2.02 | R2.04, R2.05, R3.01, R3.09, R3.26 |

## Departures from dev-phases.md

These were decided while writing the briefs. `dev-phases.md` was edited to match in the same change.

- **R2.01 grows to L and carries the whole move to v1** (2026-10-25). Beyond the row's services, it declares in full every RPC that R2.02 to R2.04 and R2.09 to R2.11 implement (files, operation subscription, presence), so those tickets only implement and add. It ports Phase 1's services to v1 in a new `reef-api` crate, and retires v0 behind a compatibility path that names a version mismatch, restarting an idle old daemon instead of failing with an empty error. It adds an `Unsupported` error kind (exit code 9), `buf` lint and breaking-change checks in CI, and ADR 0029 with the API review checklist.
- **R2.02 names diff and the operation log** among the read side it implements, with revsets and reads at an operation. **R2.03 adds abandoning a change**, which is how a divergent change is resolved (R1.13 left that to R2.03).
- **R2.02 grows to L** (2026-10-26). Beyond the six read RPCs it implements operation kinds and effects for every reported operation, switches workspace creation, moves and store builds to revset resolution (a bookmark now beats an id prefix, and a divergent change is `invalid_argument`), adds eight fields and one enum value where the declared schema cannot carry what it describes (API revision 2), and adds ADR 0030. **R2.03 depends on R2.02**: it resolves revisions, builds `Change`s and writes the `reef.kind` vocabulary R2.02 defines.
- **R2.03 grows to L** (2026-10-26). Beyond the three write RPCs it coordinates every describe, abandon and undo with the live workspaces it rewrites (their scan locks, a snapshot first, adoption after), so a write never overwrites unsnapshotted edits or makes a change divergent through a race; abandoning one commit of a divergent change hands its descendants, bookmarks and working copies to the surviving commit; undo is a revert of one operation that refuses what it cannot revert exactly (workspace creation, release and moves, git ref imports, operations later ones changed again); it writes the `reef.*` attributes and the caller's `reef-agent` on every operation Reef records, through every write path; and it adds ADR 0031. No schema additions.
