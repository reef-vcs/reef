# 0017. Daemon API v0 and error mapping

- Status: superseded by [0029](0029-daemon-api-v1.md)
- Date: 2026-10-17
- Ticket: R1.05

## Context

[0004](0004-grpc-over-unix-socket.md) chose gRPC with tonic over a Unix socket for the daemon API, and R1.03 served the first service (`Daemon.Status`) on it. R1.05 adds the first services agents use: registering repositories and creating, listing, inspecting, snapshotting, and releasing workspaces. The CLI (R1.22), `reef ws run` (R1.21), and the benchmarks (R1.12) are its first clients. Later Phase 1 tickets add `Heartbeat` (R1.15), `Move` (R1.06), and the `Maintenance` and `Stores` services.

[0009](0009-error-and-logging-conventions.md) gave every error type a coarse `ErrorKind` through `Classify` so that the edges (gRPC codes here, CLI exit codes in R1.22) never match on messages. A client also needs the kind back. gRPC codes are coarser than `ErrorKind`: `Io` and `Internal` share `INTERNAL`.

The schema will change. Phase 2 (R2.01) reviews the whole API before anything outside this repository depends on it.

## Decision

We will publish the Phase 1 API as package `reef.daemon.v0`, explicitly pre-stable, and carry each error's kind in the status details.

- **Package and files.** `crates/reef-daemon/proto/reef/daemon/v0/`: `daemon.proto` (R1.03), `types.proto` (`Repository`, `Lease`, `Workspace`, `DiskUsage`), `error.proto` (`ErrorDetail`), one file per service. Generated with `protox` and `tonic-prost-build` in `build.rs`; nothing is checked in. Timestamps are `google.protobuf.Timestamp`. Field numbers are stable within v0 so later Phase 1 tickets only add fields.
- **Services.** `Repositories { Register, List, Forget }` and `Workspaces { Create, List, Inspect, Release, Snapshot }`. `Forget` is refused with `FAILED_PRECONDITION` while any of the repository's workspaces is active.
- **Request shape.** Every repository-scoped request carries `repo: string`, which accepts a repo id, a unique id prefix (four or more hex characters, as in jj), or an absolute path inside a registered root. Where a request also names a workspace (`Inspect`, `Release`, `Snapshot`), `repo` may be empty and the workspace id or prefix is looked up in every repository. *(addition)* A prefix that matches more than one record is `INVALID_ARGUMENT` with a message listing up to ten of the matching ids, so a client can print it as is (R1.22). *(addition)*
- **Ids** are lowercase hex strings: 16 characters for Reef's own (`RepoId`, `WorkspaceId`, `LeaseId`), and jj's full hex for `change_id`, `commit_id`, and `op_id`.
- **`op_id` on every mutating response**: the operation the request recorded, empty when it recorded none (an unchanged snapshot, a final snapshot that found nothing).
- **Errors.** A handler's error becomes a status whose code comes from its `ErrorKind`: `NotFound` → `NOT_FOUND`, `AlreadyExists` → `ALREADY_EXISTS`, `InvalidArgument` → `INVALID_ARGUMENT`, `Conflict` → `FAILED_PRECONDITION`, `Unavailable` → `UNAVAILABLE`, `PermissionDenied` → `PERMISSION_DENIED`, `Io` and `Internal` → `INTERNAL`. The message is the error's chain (`ErrorChain`). The details are an encoded `reef.daemon.v0.ErrorDetail { kind, chain }`: `kind` is the `ErrorKind` variant's name, and `chain` is the message of the error and each of its sources, outermost first. `reef_daemon::error::{status, status_from, kind_from_status}` implement this. `kind_from_status` reads the detail and falls back to the inverse of the code table (`UNKNOWN` and every unmapped code → `Internal`) for statuses tonic produces itself. The client surfaces errors as `ClientError::Rpc { kind, message, chain, status }`.
- **Clients use `reef_daemon::client::Client`**, one method per RPC, never the generated stubs.

## Consequences

- R2.01 can rename and renumber freely before `v1`. v0 makes no compatibility promise, and a v1 client will not talk to a v0 daemon.
- Clients get a machine-readable kind without parsing messages, and the CLI maps it straight to an exit code. `Io` and `Internal` stay distinguishable even though they share a code.
- Every new error type must implement `Classify`, and every new `ErrorKind` variant needs a code here. `ErrorKind` is `#[non_exhaustive]`, so an unmapped kind falls back to `INTERNAL` and `"Internal"` instead of failing to compile. A unit test round-trips one error per kind.
- Paths in requests are resolved by the daemon. A client sending a path relative to its own working directory is refused (`INVALID_ARGUMENT`) rather than resolved against the daemon's.

## Alternatives considered

**`google.rpc.Status` with `ErrorInfo`** (the richer standard error model, through `tonic-types`): it carries a reason, a domain, and metadata, and generic gRPC tooling understands it. It adds a dependency and a second error vocabulary to keep in step with `ErrorKind`, for no client we have today. Revisit in R2.01.

**Matching on messages**: rejected by 0009. Messages are for humans and change with wording.

**Codes only, no detail**: loses the `Io`/`Internal` distinction and the structured chain the CLI prints.
