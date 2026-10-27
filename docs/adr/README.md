# Architecture decision records

An architecture decision record (ADR) captures one decision: the context that forced it, what was decided, its consequences, and the alternatives that lost. Write one for any choice of dependency, protocol, storage format, on-disk layout, or public API shape that a later contributor could reasonably ask "why" about; not for implementation details inside one module.

Files are named `NNNN-kebab-title.md` with a four-digit number that is never reused or renumbered. Numbers are allocated in the ticket that calls for the ADR, so tickets running in parallel do not collide. Start from [0000-template.md](0000-template.md).

Statuses are `proposed`, `accepted`, `superseded by NNNN`, and `deprecated`. After acceptance an ADR is edited only to change its status or add a superseded note; new information goes in a new ADR.

Backfilled ADRs restate [design.md](../design.md). Anything they add that design.md does not state is marked *(addition)*.

This index is maintained by hand. Every PR that adds an ADR or changes a status updates the table. A title in the table matches the ADR's heading; a reserved row names its ticket until the ADR exists.

| ADR | Title | Status |
| --- | --- | --- |
| [0001](0001-record-architecture-decisions.md) | Record architecture decisions | accepted |
| [0002](0002-build-on-jj-lib.md) | Build on jj-lib rather than forking jj or starting clean | accepted |
| [0003](0003-sqlite-for-daemon-metadata.md) | SQLite for daemon metadata, leases, and indexes | accepted |
| [0004](0004-grpc-over-unix-socket.md) | gRPC with tonic over a Unix socket for the daemon API | accepted |
| [0005](0005-tauri-desktop-shell.md) | Tauri 2 for the desktop shell | accepted |
| [0006](0006-cel-for-policy.md) | CEL for repository policy | accepted |
| [0007](0007-crate-layout.md) | Crate layout | superseded by [0033](0033-thin-client.md) |
| [0008](0008-jj-lib-embedding-findings.md) | jj-lib embedding findings | accepted |
| [0009](0009-error-and-logging-conventions.md) | Error and logging conventions | accepted |
| [0010](0010-dco-instead-of-cla.md) | DCO instead of CLA | accepted |
| [0011](0011-hash-and-object-backend-by-phase.md) | Hash function and object backend by phase | accepted |
| [0012](0012-daemon-process-model-and-threading.md) | Daemon process model and threading | accepted |
| [0013](0013-daemon-state-directory-and-configuration.md) | Daemon state directory and configuration | accepted |
| [0014](0014-workspace-model-on-jj.md) | Workspace model on jj | accepted |
| [0015](0015-materialization-backends-and-base-cache.md) | Materialization backends and the base cache | accepted |
| [0016](0016-overlayfs-privilege-tiers.md) | overlayfs on Linux: privilege tiers | accepted |
| [0017](0017-daemon-api-v0-and-error-mapping.md) | Daemon API v0 and error mapping | superseded by [0029](0029-daemon-api-v1.md) |
| [0018](0018-snapshot-staleness-and-divergence.md) | Snapshot staleness and divergence | accepted |
| [0019](0019-leases-and-reclaim.md) | Leases and reclaim | accepted |
| [0020](0020-local-backup-format.md) | Local backup format | accepted |
| [0021](0021-workspace-spec-file.md) | Workspace spec file | accepted |
| [0022](0022-shared-dependency-stores.md) | Shared dependency stores | accepted |
| [0023](0023-watchman-as-the-large-repository-watcher.md) | Watchman as the large-repository watcher | accepted |
| [0024](0024-repository-onboarding.md) | Repository onboarding: implicit registration and jj alongside git | accepted |
| [0025](0025-agent-facing-cli-contract.md) | Agent-facing CLI contract | accepted |
| [0026](0026-narrowed-notify-snapshots.md) | Narrowed notify snapshots | accepted |
| [0027](0027-trust-boundaries-in-phase-1.md) | Trust boundaries in Phase 1 | accepted |
| [0028](0028-watch-attachment-off-the-create-path.md) | Watch attachment off the create path | accepted |
| [0029](0029-daemon-api-v1.md) | Daemon API v1 | accepted |
| [0030](0030-reads-over-jj-state.md) | Reads over jj state | accepted |
| [0031](0031-writes-over-jj-state.md) | Writes over jj state | accepted |
| [0032](0032-files-without-a-workspace.md) | Files without a workspace | accepted |
| [0033](0033-thin-client.md) | Thin client | accepted |
| [0034](0034-cli-over-the-vcs-services.md) | CLI over the VCS services | accepted |
| [0035](0035-mcp-server.md) | MCP server | accepted |
| [0036](0036-operation-log-subscriptions.md) | Operation log subscriptions | accepted |
| [0037](0037-presence-channel.md) | Presence channel | accepted |
| [0038](0038-subscription-broker.md) | Subscription broker | accepted |
