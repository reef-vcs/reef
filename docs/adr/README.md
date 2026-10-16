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
| [0007](0007-crate-layout.md) | Crate layout | accepted |
| [0008](0008-jj-lib-embedding-findings.md) | jj-lib embedding findings | accepted |
| [0009](0009-error-and-logging-conventions.md) | Error and logging conventions | accepted |
| [0010](0010-dco-instead-of-cla.md) | DCO instead of CLA | accepted |
| [0011](0011-hash-and-object-backend-by-phase.md) | Hash function and object backend by phase | accepted |
| [0012](0012-daemon-process-model-and-threading.md) | Daemon process model and threading | accepted |
| [0014](0014-workspace-model-on-jj.md) | Workspace model on jj | accepted |
| [0015](0015-materialization-backends-and-base-cache.md) | Materialization backends and the base cache | accepted |
