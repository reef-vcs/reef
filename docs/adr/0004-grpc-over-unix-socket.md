# 0004. gRPC with tonic over a Unix socket for the daemon API

- Status: accepted
- Date: 2026-10-15
- Ticket: R0.07

Backfilled from design.md; decision predates this record.

## Context

Agents talk only to the local daemon, and the CLI and MCP server are thin clients of the same API (design principle 8, [API](../design.md#api)). Agents will be written in Python and TypeScript far more than Rust, so the API needs generated clients rather than hand-maintained bindings. Subscriptions to the operation log and the presence channel, and long operations such as fetch and bisect, need server streaming. The web UI must reach the same streams from a browser ([Technology choices](../design.md#technology-choices)).

## Decision

We will expose the daemon API as gRPC with protobuf schemas, implemented with `tonic` and `prost`.

- Schemas live in `crates/reef-daemon/proto/` from R1.05 onward and are versioned and reviewed as an API, not generated from code (R2.01).
- The daemon listens on a Unix domain socket. Filesystem permissions on the socket are the first access control *(addition)*.
- A daemon can optionally also listen over TLS with token authentication so remote clients, such as a phone, reach the same API.
- Python and TypeScript SDKs are generated from the same schemas.
- The web UI reaches the same services over gRPC-web or a WebSocket bridge.

## Consequences

- Codegen keeps the SDKs in sync with the daemon, and no client parses text.
- Every schema change is an API change and goes through review, which is a real, intended cost.
- The Windows transport, named pipes or Windows' own Unix domain sockets, is open and tied to the Windows decision in RX.05 *(addition)*.
- The remote store speaks the same protocol, so clients do not care which they are talking to.

## Alternatives considered

**JSON-RPC over HTTP.** Simple and debuggable. Rejected: no native streaming and hand-written clients *(addition)*.

**Cap'n Proto.** Fast, with schemas. Rejected: a smaller ecosystem for Python and TypeScript *(addition)*.

**A bespoke binary protocol.** Rejected: no tooling, and every SDK is written by hand.
