# 0029. Daemon API v1

- Status: accepted
- Date: 2026-10-25
- Ticket: R2.01
- Supersedes: [0017](0017-daemon-api-v0-and-error-mapping.md)

## Context

Phase 1 shipped its API as `reef.daemon.v0` ([0017](0017-daemon-api-v0-and-error-mapping.md)): five services, explicitly pre-stable, with stringly typed states, `op_id` strings on writes, unpaginated lists, and a raw `ErrorDetail` in the status details that stock gRPC tooling cannot decode. 0.1.0 shipped it. Phase 2 puts every client on one typed API: the CLI as a thin client (R2.05), an MCP server (R2.13), Python and TypeScript SDKs (R2.06, R2.07), a web UI (R2.18), and a remote listener (R2.08). Once those exist, v1's field numbers, names and files are frozen, and later tickets may only add.

An abandoned first attempt at R2.01 sketched the read and write services loosely and had to redesign them while implementing; it also broke `reef daemon stop` and `status` across the v0 to v1 change, because a v1 client calling a 0.1 daemon got tonic's bare `UNIMPLEMENTED`, which the client read as an empty `internal` error.

## Decision

1. **Package `reef.api.v1`, in a new crate `reef-api`** (`crates/reef-api/proto/reef/api/v1/`, 13 files: one per service plus `options.proto` and `error.proto`). "api", not "daemon": R4.01's store serves the same services ([0004](0004-grpc-over-unix-socket.md): clients do not care which they talk to). The crate holds the protos, the generated messages, clients and servers, the descriptor set (`reef_api::FILE_DESCRIPTOR_SET`), the access table, and the error envelope, and depends on no other Reef crate, so a client needs nothing else to speak the API. This is the separate protocol crate [0007](0007-crate-layout.md) deferred to this ticket.
2. **The whole Phase 2 surface is declared now.** Every RPC that R2.02 to R2.04 and R2.09 to R2.11 implement is in the schema with its field numbers; the daemon answers each with `UNIMPLEMENTED`, reason `NOT_IMPLEMENTED`, until its ticket lands. A declared RPC that proves wrong is deprecated and replaced, never edited.
3. **v0 is retired except a frozen beacon.** `reef.daemon.v0`'s `daemon.proto` and `error.proto` stay, unchanged on the wire, in `reef-api`; the daemon serves only `reef.daemon.v0.Daemon/Status`, so a 0.1 CLI can still run `reef daemon status` and `stop`.
4. **Every version mismatch says what it is.** The router's `CompatLayer` rewrites a bare `UNIMPLEMENTED`: any other v0 method gets `FAILED_PRECONDITION` with a v0 `ErrorDetail { kind: "Conflict" }` telling a 0.1 client to upgrade (0.1 knows no `Unsupported`); any other unknown method gets `UNIMPLEMENTED`, reason `UNKNOWN_METHOD`. The client turns a bare `UNIMPLEMENTED`, `UNKNOWN_METHOD`, or a `NOT_IMPLEMENTED` from a daemon of another version into `ClientError::Incompatible` after asking the daemon what it is (v1 `GetDaemonStatus`, then v0 `Daemon/Status`). The CLI then restarts the daemon if it is idle (no active workspaces, autostart allowed, its pid agreeing with the pid file as `daemon stop` checks) and runs the command once more; otherwise it fails with exit code 9 and `next` `reef daemon stop`.
5. **Naming follows buf's `STANDARD` and `COMMENTS` lint rules.** Services end in `Service`; RPCs are verb-noun; every RPC has its own `<Rpc>Request` and `<Rpc>Response`; enum values carry the enum's prefix and zero is `<ENUM>_UNSPECIFIED`, whose meaning the enum's comment states; every element has a leading comment giving its meaning, unit, and what empty or zero means. Under buf's `FILE` breaking rules a message cannot move between files later, so placement is final.
6. **References.** `repo` is a repo id, a unique prefix of four or more hex characters, or an absolute path inside a registered root. `workspace` is a workspace id, a unique prefix, or an absolute path inside an active workspace; where a request names one, `repo` may be empty. `revision` is a jj revset that must resolve to exactly one commit (`@` is the request's workspace's working-copy commit; `trunk()` and `immutable_heads()` are Reef aliases). `at_operation` reads at that operation's view; empty is the head.
7. **Ids are full lowercase hex in responses**, change ids included; requests also accept change id prefixes in jj's reverse hex, whose alphabet is disjoint. Reef's own ids are 16 hex characters.
8. **Time.** Instants are `google.protobuf.Timestamp`, UTC, named `<event>_at`. Intervals a client sets or that are policy are `google.protobuf.Duration`. Measured elapsed times are `uint64 <name>_ms`.
9. **Enums for closed sets the daemon defines** (workspace state, backend, watcher, attention, lease state and end reason, snapshot kind, mount mode, backup step); free text and open sets stay strings. A scalar is `optional` only where zero is meaningful and distinct from the default.
10. **Every List RPC is paginated from the first revision**: `int32 page_size` (0 means 100, above 1,000 is 1,000, negative is `INVALID_ARGUMENT`), `string page_token`, `string next_page_token` (empty on the last page). Tokens are opaque base64url, bound to the request's other fields (`INVALID_ARGUMENT`, reason `INVALID_PAGE_TOKEN`, when sent with others), and walk items in a fixed key order so pages hold when items are added or removed between them.
11. **Typed filters, no field masks.** Filters are typed fields, ORed within a field and ANDed across fields, except `ListChanges.revset`, which is jj's language. Heavy parts of a response are opt-in flags; updates are explicit RPCs.
12. **Writes return their operation.** A response reports an operation it created as a full `Operation` message (`operation`, or a role such as `final_snapshot`), unset when nothing was recorded, and refers to one it did not create by id. Every `ACCESS_WRITE` RPC's response has `Operation operation`, except `RenewLease`, `ReportPresence` and `ClearPresence`, which record none.
13. **Streaming is server-side only**: subscriptions and long operations. The web UI reaches the API over gRPC-web, which carries no client streaming.
14. **The error model is `google.rpc.Status` with one `reef.api.v1.ErrorDetail`** (`kind`, `reason`, `chain`, `hint`, `next`, `metadata`) packed as an `Any` in `grpc-status-details-bin`, which Python's `grpcio-status`, Connect-ES and `tonic-types` decode with stock tooling. `reason` is an UPPER_SNAKE constant documented on the RPCs that return it. The gRPC code comes from the kind as in 0017, plus a ninth kind.
15. **A ninth error kind, `Unsupported`**: the daemon cannot serve the request as asked (a declared RPC it does not implement, a method it does not know, a version mismatch). It maps to `UNIMPLEMENTED`, is a caller error, has exit code 9 and kind text `unsupported`. This adds to [0009](0009-error-and-logging-conventions.md)'s kinds and [0025](0025-agent-facing-cli-contract.md)'s exit codes; the CLI's JSON error object gains `reason` when it is not empty.
16. **Every RPC states its access in the schema.** `options.proto` defines `enum Access { READ, WRITE, ADMIN }` and the method option `(reef.api.v1.access)`; `reef-api`'s build fails if a method lacks it. Reads that never snapshot also set `idempotency_level = NO_SIDE_EFFECTS`, and retry-safe calls `IDEMPOTENT`. R2.08's remote scopes and R2.13's tool hints build on it; `reef_api::access(path)` serves the table.
17. **Versioning: v1 only grows.** Nothing is removed, renumbered, renamed or moved; a field that should go is marked `[deprecated = true]`. `buf breaking` with the `FILE` rules runs against the base branch in CI. `reef_api::API_REVISION` increases by one with every schema change and `GetDaemonStatus` reports it; a test fails until `API_DIGESTS`, the blake3 of the v1 descriptors without comments for each revision, has a new last entry that matches and as many entries as the revision number. A v2 would be a new package served beside v1.
18. **Tooling.** Codegen stays `protox` plus `tonic-prost-build`, so building needs neither `protoc` nor `buf`. `buf` (pinned in CI) lints, checks formatting, and checks breaking changes, in CI's `api` job and in `just api`.

### API review checklist

Every pull request that touches `crates/reef-api/proto` is reviewed against this list; it is how "reviewed as an API, not generated from code" is met from R2.01 on.

1. `buf breaking` passes against `main`; nothing is removed, renumbered, renamed or moved.
2. `API_REVISION` and `API_DIGEST` are bumped.
3. Every new element has a leading comment giving its meaning, its unit, and what empty or zero means.
4. A new enum's zero is `_UNSPECIFIED`, with its meaning stated.
5. A new RPC sets `access` (and `idempotency_level` where it holds), and its request and response are named after it.
6. A List RPC has `page_size`, `page_token` and `next_page_token`.
7. A `WRITE` RPC's response has `Operation operation`.
8. References use `repo`, `workspace`, `revision` and `at_operation` as defined, and ids are full hex.
9. Errors' kinds and reasons are documented on the RPC.
10. The RPC is implemented or answers `NOT_IMPLEMENTED`.
11. CHANGELOG lists the addition.

## Consequences

- Later Phase 2 tickets implement declared RPCs and add fields at the next free numbers; they do not reshape the schema. A wrong declaration costs an unused RPC, not a migration.
- An upgrade replaces the binaries but leaves the old daemon running. The CLI restarts an idle old daemon itself; a busy one keeps serving its `ws run` sessions, whose heartbeats a new daemon would refuse, until its user stops it.
- The CLI's JSON did not change: the client maps v1 enums back to Phase 1's strings, and `daemon status` only gains `api` and `api_revision`.
- `buf` becomes a contributor tool for `just ci`, but never a build dependency.
- The digest depends on protox's encoding; a protox upgrade that changes it updates `API_DIGEST` without a revision bump, with a note.

## Alternatives considered

- **Keep the protos in `reef-daemon`.** The CLI after R2.05 and the SDKs would have to depend on the daemon crate to reach the schema.
- **`reef.daemon.v1`.** The store will serve the same API; the package should not name the server.
- **Serve v0 beside v1 for a release.** Two handler sets for a pre-1.0 API with days-old users, and v0's raw error detail kept alive.
- **`google.rpc.ErrorInfo` instead of a Reef detail.** It has no place for `hint`, `next` or the chain, and two details would need keeping in step.
- **AIP-160 filter strings and field masks.** Every SDK would have to build and parse them for no measured gain.
- **Declare only the services the R2.01 row names.** The abandoned attempt; its sketch disagreed with the implementing tickets in about fifty places.
- **`protoc` with `buf generate`.** A toolchain dependency for every build, against R1.03's choice of `protox`.
- **No automatic restart.** An agent's first command after an upgrade would fail, the friction RX.07 measures.
