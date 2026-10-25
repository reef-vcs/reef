# 0029. Daemon API v1 and the `reef-api` crate

- Status: accepted
- Date: 2026-10-25
- Ticket: R2.01

## Context

Phase 1's API was `reef.daemon.v0` ([0017](0017-daemon-api-v0-and-error-mapping.md)): seven proto files written one ticket at a time, generated inside `reef-daemon`, reached through `reef_daemon::client::Client`. 0017 called it pre-stable and left the rename and the error envelope to R2.01.

Phase 2 adds the read side (status, log, diff), the write side (describe, abandon, squash, undo), the virtual workspace, event and presence streams, replay, symbols, named queries, remote tokens, and client-side store builds, and its first clients outside the daemon's own crates: the Python and TypeScript SDKs (R2.06, R2.07), the MCP server (R2.13), `reef schema` (R2.24), and the web UI's code generation (R2.18). Written one ticket at a time, as Phase 1 did, the API would be reviewed eleven times in pieces. And any of those clients reaching the generated code today has to depend on `reef-daemon`, which links jj-lib, SQLite, and every materialization backend.

The TypeScript SDK's research (R2.07) found that connect-es decodes the `grpc-status-details-bin` trailer as a `google.rpc.Status` with no fallback, so v0's raw `ErrorDetail` bytes there break every error on that client; `grpcio-status` in Python expects the same.

## Decision

We will publish the whole Phase 2 API at once as package `reef.daemon.v1`, in a new bottom crate `reef-api`, and delete v0.

1. **One schema, reviewed once.** `crates/reef-api/proto/reef/daemon/v1/`: `types.proto`, `error.proto`, and one file per service (`daemon`, `repositories`, `workspaces`, `changes`, `operations`, `events`, `trees`, `presence`, `symbols`, `queries`, `remote`, `maintenance`, `stores`). Every message and RPC a Phase 2 ticket implements is written now, with its field numbers and a comment on every message, field, enum value, service, and method (a test fails on an undocumented one). Where R2.01's sketch and the brief of the ticket that implements an RPC differed, the implementing brief's shape was taken (R2.01's PR lists each), since a later ticket may only add. The daemon answers each RPC whose ticket has not landed with `UNIMPLEMENTED`, kind `Internal`, and the hint `not implemented until R2.xx`, from one table (`reef_daemon::service::stubs::STUBS`) that each ticket shrinks.
2. **Additive within v1.** From this ticket on, fields and RPCs are added, never renumbered, renamed, or removed; a field that stops being written keeps its number and says since when in a comment. `crates/reef-api/tests/fixtures/v1-fields.txt` lists every `(message, field, number)`, and a test compares the schema against it, so a renumbering fails until the fixture is regenerated on purpose. A breaking change is `reef.daemon.v2`.
3. **Naming.** Nouns for services, verbs for RPCs, `<Verb><Noun>Request`/`Response` for new messages; v0's names that already serve (`CreateRequest`, `InspectRequest`, `GcRequest`, ...) and a few implementing briefs' (`SubscribeRequest`, `ReplayState`, `Seal` returning `BuildResult`) are kept. `repo`, `rev`, `change`, `op`, and `workspace` name a repository, a revision, a change id, an operation id, and a workspace on every request. Ids are lowercase hex, timestamps `google.protobuf.Timestamp`, durations `google.protobuf.Duration`. Every enum in a response has an `*_UNSPECIFIED = 0` the daemon never sends, so a value unknown to an older client is not read as a real state. v0's carry-overs keep their meaning; the renames are `Daemon.Status`'s messages (`DaemonStatusRequest`/`Response`), `HeartbeatRequest.workspace` (was `workspace_id`), and the new `Page` on `ListWorkspacesRequest`.
4. **Pagination.** `Page { page_size, page_token }` on every request whose list can grow, `PageInfo { next_page_token, page_size, restarted }` on its response. `page_size` 0 is the RPC's default and more than its cap is the cap: 100 and 1,000, except `Trees.List`'s 1,000 and 10,000 (R2.04). The token is opaque: base64url of the request's filter hash and a cursor. A token sent with other filters is `INVALID_ARGUMENT`. `reef_api::page` holds the one encoder and decoder. `Workspaces.List` pages newest first on `(created_at, id)`.
5. **Errors.** [0017](0017-daemon-api-v0-and-error-mapping.md)'s `ErrorKind` to code table and `ErrorDetail { kind, chain, hint, next }` are unchanged. The envelope changes: the status details are a `google.rpc.Status` (code, message, and `details`) whose one detail is `ErrorDetail` packed as a `google.protobuf.Any` with type URL `type.googleapis.com/reef.daemon.v1.ErrorDetail`, the standard rich-error model, built with `tonic-types` 0.14. `reef_api::error::detail_of` reads it, so `ClientError` and everything above it see no change.
6. **The `reef-api` crate.** The protos, the code generated from them (server and client, by `protox` and `tonic-prost-build` in `build.rs`, nothing checked in), `DESCRIPTOR_SET` (the `FileDescriptorSet` protox compiled, with comments and the imported well-known types, embedded from `OUT_DIR`), `SCHEMA_HASH` (its BLAKE3, hex), the client (`reef_api::client`, one method per v1 RPC, stubs included), the JSON output types (`reef_api::json`), pagination (`reef_api::page`), the error kinds (`reef_api::error`: `ErrorKind`, `Classify`, `classify_by_source`, `ErrorChain`, and the client's half of the code mapping), and `Advice`/`Advise` (`reef_api::advice`). It depends on tonic, prost, tokio, serde, and similar, and never on jj-lib, gix, rusqlite, or another Reef crate; `crates/reef-testing/tests/crate_graph.rs` walks its resolved normal dependencies to enforce it.
7. **Dependency direction** ([0007](0007-crate-layout.md), amended): `reef-vcs` → `reef-daemon` → `reef-lib` → `reef-api`, and `reef-vcs` → `reef-api` directly. `reef_lib::error` re-exports `reef_api::error`, so code classifying errors does not change. `reef_daemon::client` and `reef_daemon::proto` are removed, not re-exported, so no new code reaches the API through the daemon.
8. **Discovery.** The socket listener serves gRPC server reflection (`grpc.reflection.v1`, `tonic-reflection`) from `DESCRIPTOR_SET`, so `grpcurl -unix` works against a daemon. `Daemon.Status` reports `api_version = "v1"` and `schema_hash`. The remote and web listeners (R2.08) do not serve reflection.

## Consequences

- An SDK, the MCP server, or any Rust client depends on `reef-api` alone, which builds without a C compiler for SQLite and without jj-lib.
- v0 clients no longer connect: the services' paths changed with the package. Nothing outside this repository used v0.
- Every later Phase 2 ticket adds server code to a schema that exists; a ticket that finds a field missing adds one, and one that needs a different shape adds a field or an RPC beside the old one.
- Fields that nothing fills until their ticket lands (`Workspace.presence`, `Operation.seq`, `DaemonStatusResponse.seqs`, ...) are documented with that ticket, so a field still unfilled at the end of Phase 2 is visible.
- Generic gRPC tooling (grpcurl, Postman, `grpcio-status`, connect-es) understands both the schema, through reflection, and the errors, through the standard envelope.
- `Workspaces.List` returns at most a page; a client that wants every workspace follows `next_page_token` (the Rust client's `list_workspaces` does).

## Alternatives considered

**Keep generating inside `reef-daemon` and let clients depend on it**: every client would link jj-lib, SQLite, and the backends, and nothing would stop a client from reaching daemon internals.

**Raw `ErrorDetail` bytes in the status details, as v0**: Python decodes them, but connect-es decodes the trailer as `google.rpc.Status` and fails on anything else, so the TypeScript SDK would see no error kinds. **`google.rpc.ErrorInfo` instead of `ErrorDetail`**: `ErrorInfo { reason, domain, metadata }` has no place for the chain, the hint, or the argv `next` without encoding them into strings; one packed `ErrorDetail` keeps them typed.

**Serve v0 and v1 side by side for a release**: v0 promised nothing (0017), and two schemas served at once are two to keep in step.

**Write each service's schema in its own ticket, as Phase 1 did**: the API would be reviewed in eleven pieces, and later tickets would restructure what earlier ones shipped.
