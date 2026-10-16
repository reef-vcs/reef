# 0007. Crate layout

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.01

## Context

The workspace started as three placeholder crates, `reef-vcs`, `reef-lib`, and `reef-daemon`, published at 0.0.0 only to reserve the names on crates.io. None had a defined role.

The design's [Architecture](../design.md#architecture) section fixes the process boundaries: agents talk to a local daemon over a typed API; the daemon owns snapshots, workspaces, merge, and queries; the CLI and the MCP server are thin clients (design principle 8, "API first, CLI second"). The crate boundaries should follow the process boundaries, so that what the service owns and what a client may touch are visible in the dependency graph. The graph alone does not stop the CLI from using the data model: `reef-vcs` depends on and re-exports `reef-lib`. Keeping the CLI a thin client is enforced by review and by R2.05.

Benchmarks in `reef-lib` and integration tests in the daemon both need the same synthetic repository generator and fixtures (R0.05, R0.06). That code must be shared without being published and without creating a dependency cycle.

## Decision

We will have four crates:

- `reef-lib`: data model, jj-lib embedding, workspace model, merge engine, query layer. No server or CLI code; usable by any process.
- `reef-daemon`: the long-running service. API, workspace materialization backends, lease manager, watchers, logging initialization. A library plus the `reef-daemon` binary.
- `reef-vcs`: the `reef` binary (CLI, later `reef mcp`), plus a library that re-exports `reef_lib` and `reef_daemon` so downstream users depend on one crate.
- `reef-testing`: `publish = false`. Synthetic repository generator, fixture builders, test helpers. Depends on no other Reef crate.

The dependency direction is strictly `reef-vcs` → `reef-daemon` → `reef-lib`. Any crate may dev-depend on `reef-testing`. The direction is enforced by review and by `crates/reef-testing/tests/crate_graph.rs`, which reads `cargo metadata` and fails on a forbidden edge.

The workspace uses edition 2024, declares every third-party version once in `[workspace.dependencies]`, and every crate inherits `[workspace.lints]`. `reef-testing` is left out of `default-members`.

## Consequences

- A change to the data model is made in `reef-lib`. The daemon links it directly; clients see it only as far as the API exposes it.
- The daemon binary and the CLI binary cannot share code except through `reef-daemon` as a library. That is acceptable: the CLI is a thin client by design principle 8, and R2.05 removes jj-lib from the CLI's direct reach.
- Publishing requires path dependencies to carry versions, so internal entries in `[workspace.dependencies]` carry `version`. Until a real release, `cargo publish --dry-run` for `reef-daemon` and `reef-vcs` verifies against the empty 0.0.0 placeholders on crates.io; once they use their siblings the dry run needs `--no-verify`, and a release bumps all three versions together (R4.17).
- A bare `cargo build` skips `reef-testing`; CI and anything that needs it pass `--workspace`.
- We would revisit this if the protocol schema needs its own crate (see alternatives) or if compile times force splitting `reef-lib`.

## Alternatives considered

**Single crate.** One crate with library and binaries. Rejected: every binary and test would compile jj-lib and its dependencies, and there would be no seam at which R2.05 can remove jj-lib from the CLI's direct use.

**A separate `reef-proto` crate.** A crate holding the generated protocol types, shared by daemon and clients. Deferred to R2.01, when there is a schema to put in it.

**Do nothing.** Keep the placeholders and let structure emerge. Rejected: R0.02, R0.04, R0.05, and R0.09 all need to know where their code goes, and a fixture crate added later would have to untangle existing test code.
