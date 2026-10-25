# The Reef daemon API

This is the guide for writing a client of the Reef daemon: an SDK, a tool, an agent harness. The API is gRPC, package `reef.daemon.v1`, defined by the `.proto` files in [`crates/reef-api/proto/reef/daemon/v1/`](../crates/reef-api/proto/reef/daemon/v1/). Every message, field, and RPC there carries a comment; this page explains how the pieces fit, and the files are the reference. Rust clients use `reef_api::client::Client`, which has one method per RPC. The design decisions behind the API are [ADR 0029](adr/0029-daemon-api-v1-and-the-reef-api-crate.md).

Several services are written down before their implementation: the daemon answers those RPCs with `UNIMPLEMENTED` and a hint naming the ticket that implements them (see [Not implemented yet](#not-implemented-yet)). Their shapes are part of v1 and will not change when they land.

## Transport

The daemon listens on a Unix socket: `$XDG_RUNTIME_DIR/reef/daemon.sock` on Linux when that is set, else `<state>/daemon.sock`, unless configured otherwise (`reef config show` prints it, and the CLI takes `--socket` or `REEF_SOCKET`; on macOS the state directory is `~/Library/Application Support/reef`). gRPC runs over HTTP/2 without TLS on that socket. A client connects with any URI (the authority is ignored) and a connector that opens the socket.

The socket serves gRPC server reflection (`grpc.reflection.v1`), so generic tools work against a running daemon:

```sh
socket="$(reef daemon status -o json | jq -r .socket)"
grpcurl -unix -plaintext "$socket" list
grpcurl -unix -plaintext "$socket" reef.daemon.v1.Daemon/Status
```

`Daemon.Status` reports `api_version` (`"v1"`) and `schema_hash`, the BLAKE3 of the descriptor set the daemon was built with: a client built from the same schema can compare it with its own (`reef_api::SCHEMA_HASH` in Rust).

Remote and browser access over TCP, with TLS and bearer tokens, is R2.08's (`Remote` below); those listeners do not serve reflection.

## Authorization

Whoever can open the socket can call every RPC: the socket is created `0600` in a `0700` state directory, so that is the daemon's user ([threat model](threat-model.md)). Tokens and scopes apply only to the TCP listeners (R2.08).

## Compatibility

Within v1 every change is additive. Fields and RPCs are added; none is renumbered, renamed, or removed. A field that stops being written keeps its number and a comment saying since when. A client must therefore ignore unknown fields (every protobuf library does) and treat an unset field as its default. Every enum in a response has a zero value `*_UNSPECIFIED` that the daemon never sends, so a value a client does not know (from a newer daemon) is not mistaken for a real one; enums in requests keep a meaningful zero (`DiffRequest.Level.STAT`, `CreateRequest.MissingStores.FAIL`). A breaking change would be a new package, `reef.daemon.v2`.

## Names, ids, and revisions

- **Requests name things by role**: `repo` a repository, `workspace` a workspace, `rev` a revision, `change` a change (a field that also takes any revision says so), `op` an operation.
- **`repo`** accepts a repository id (16 hex characters), a unique id prefix of 4 or more characters, or an absolute path inside a registered root. Where a request also names a workspace, `repo` may be empty.
- **`workspace`** accepts a workspace id, a unique prefix, or an absolute path inside an active workspace's directory.
- **Ids** are lowercase hex: 16 characters for Reef's own (repositories, workspaces, leases, tokens), and jj's full hex for change, commit, and operation ids. Humans see change ids in jj's "reverse hex" (`z` to `k`); `rev` accepts both.
- **`rev`** resolves as a change id prefix, a commit id prefix, or a local bookmark, in that order, plus `@` (the user's own checkout), `<workspace-id>@` (a Reef workspace's working-copy commit), and a trailing `-` for the first parent. Only `Changes.Log` takes jj's revset language.
- **Times** are `google.protobuf.Timestamp`, durations `google.protobuf.Duration`. A few Phase 1 fields count milliseconds and say so in their name (`lease_ms`, `uptime_ms`).
- **Paths** in requests are absolute when they are on the daemon's machine (`RegisterRequest.path`, `CreateRequest.path`), and repository-root relative, `/`-separated, when they are inside a tree.

## Pagination

A request whose list can grow takes `Page page { page_size, page_token }`; its response carries `PageInfo page { next_page_token, page_size, restarted }`.

- `page_size` 0 is the RPC's default (100; 1,000 for `Trees.List`), and more than the cap (1,000; 10,000 for `Trees.List`) is the cap. `PageInfo.page_size` says what was used.
- The first request sends an empty `page_token`. Each later request sends the previous response's `next_page_token` unchanged, with the same filters. An empty `next_page_token` means the last page.
- Tokens are opaque: base64url of the request's filter hash and a cursor. A token sent with other filters is `INVALID_ARGUMENT`. Tokens do not expire, and a cursor survives new writes.
- `restarted` says the cursor's item is gone (a commit `Changes.Log` paged past was hidden, say) and the page starts over from the first: redraw instead of appending.

Paged RPCs: `Workspaces.List`, `Changes.Log`, `Operations.Log`, `Trees.List`, `Symbols.Lookup`, `Queries.Run`. `Workspaces.List` is newest first. The Rust client's `list_workspaces` follows the tokens for you; the other methods return one page.

## Errors

A failed RPC returns a gRPC status whose code comes from the error's kind:

| `ErrorDetail.kind` | gRPC code | CLI exit code |
| --- | --- | --- |
| `NotFound` | `NOT_FOUND` | 3 |
| `AlreadyExists` | `ALREADY_EXISTS` | 4 |
| `InvalidArgument` | `INVALID_ARGUMENT` | 2 |
| `Conflict` | `FAILED_PRECONDITION` | 5 |
| `Unavailable` | `UNAVAILABLE` | 6 |
| `PermissionDenied` | `PERMISSION_DENIED` | 7 |
| `Io` | `INTERNAL` | 8 |
| `Internal` | `INTERNAL` | 1 |

The status message is the error and its causes joined with `: `. The details (the `grpc-status-details-bin` trailer) are a `google.rpc.Status` whose `details` hold one `google.protobuf.Any` with type URL `type.googleapis.com/reef.daemon.v1.ErrorDetail`, the standard rich-error model, so `grpcio-status`, connect-es, and tonic's `tonic-types` decode it without Reef-specific code. [`ErrorDetail`](../crates/reef-api/proto/reef/daemon/v1/error.proto) is:

- `kind`: the row above. Key retries and exit codes on it, not on the gRPC code: `Io` and `Internal` share `INTERNAL`.
- `chain`: the message of the error and each cause, outermost first.
- `hint`: one sentence on how to fix it, when the daemon knows.
- `next`: a command line that fixes it or shows what to do next, as an argv an agent can run verbatim (`["reef", "store", "build", "deps", "--repo", "a3dda866c7e8f343"]`). Never a placeholder.

A status without the detail (a dropped connection, a deadline, a limit such as `RESOURCE_EXHAUSTED` or the TCP listeners' `UNAUTHENTICATED`) maps back through the code column, every unmapped code to `Internal`. `UNIMPLEMENTED` is used only for RPCs that are not implemented yet; their detail's kind is `Internal`.

## Mutations

Every response to a request that writes carries `op_id` (`final_op_id` for a release), the jj operation it recorded, empty when it recorded none (an unchanged snapshot); where the response also carries the `Operation`, the two agree. From R2.09 on it also carries `seq`, the operation's event sequence, so a client can wait for the event of its own write on a subscription. The R2.03 and R2.04 writes (`Changes.Describe`, `Abandon`, `Squash`, `Operations.Undo`, `Trees.Apply`) also snapshot every workspace they would move first and check the new commits out in them afterwards. Their responses list those snapshots (`snapshots`), the workspaces checked out (`workspaces_updated`), and those whose check-out failed (`workspaces_failed`, which then get `attention = checkout-failed`; the write stands).

Three RPCs are client-streaming: `Trees.Apply`, `Presence.Report`, and `Symbols.Ingest`. gRPC-web carries unary and server-streaming calls only, so a browser client cannot make them.

## Services

Twelve services. Each paragraph names the ticket that implements an RPC still stubbed today.

### Daemon

[`daemon.proto`](../crates/reef-api/proto/reef/daemon/v1/daemon.proto). **`Status`**: the daemon's version, process, state directory, socket, database schema version, backends and whether each can serve workspaces, counts of repositories and active workspaces, and the API version and schema hash. Fields filled by later tickets are empty until then: per-repository event sequences and the daemon-wide newest `seq` (R2.09), listener addresses (R2.08), presence counts (R2.10, R2.11), whether the UI is built in (R2.18).

### Repositories

[`repositories.proto`](../crates/reef-api/proto/reef/daemon/v1/repositories.proto). **`Register`** registers the jj repository containing an absolute path, by its root; with `if_not_jj = INIT_ALONGSIDE_GIT` a git checkout gets a colocated `.jj` first. Registering twice succeeds with `already_registered`. **`List`** returns every registered repository, oldest first. **`Forget`** unregisters one, refused while any of its workspaces is active.

### Workspaces

[`workspaces.proto`](../crates/reef-api/proto/reef/daemon/v1/workspaces.proto). A workspace is a leased directory on a jj change, snapshotted after every write. **`Create`** materializes one on a change (`change`) or a new change on a revision (`new_on`), or on the default start (`main`, then the remote's default branch, and so on; `start` reports which). It issues a lease, which the holder renews with **`Heartbeat`** every `lease.heartbeat_every`; a lease past its grace is reclaimed. **`List`** pages workspace rows, newest first, filtered by repository, state, and owner. **`Inspect`** returns one workspace with its disk usage, its indexed snapshots, and the other workspaces on its change (optionally snapshotting first). **`Snapshot`** snapshots now. **`Move`** snapshots and checks another change out in the same directory, keeping id, path, and lease. **`Release`** takes a final snapshot and removes the directory; the change stays. **`Status`** (R2.02; served from the status index without a scan from R2.23) says what changed in the workspace: its change's files against its parents, the paths written since the last snapshot, conflicts, and divergence with the `squash` that resolves it.

### Changes

[`changes.proto`](../crates/reef-api/proto/reef/daemon/v1/changes.proto). **`Log`** (R2.02) evaluates a revset, `reef_log()` by default, a page of `Change`s at a time; `with_graph` adds jj's graph edges and orders rows as `jj log` draws them. **`Get`** (R2.02) returns one change and its diff stat. **`Diff`** (R2.02) diffs a change against its parents, or two revisions, at a level: `STAT` (counts), `FILES` (paths and kinds), `LINES` (structured hunks, bytes per line), or `SYMBOLS` (R2.15: definitions added, removed, changed, and moved, with their call sites). It is bounded by bytes and says so in `truncated`. **`Describe`**, **`Abandon`**, and **`Squash`** (R2.03) rewrite history as `jj describe`, `abandon`, and `squash` do, and move the workspaces on the changes they rewrite (see [Mutations](#mutations)).

### Operations

[`operations.proto`](../crates/reef-api/proto/reef/daemon/v1/operations.proto), with the event messages in [`events.proto`](../crates/reef-api/proto/reef/daemon/v1/events.proto). **`Log`** and **`Get`** (R2.02) read the jj operation log, filtered by kind, workspace, owner, time, change, or event sequence. **`Undo`** (R2.03) restores the view to before an operation, recording a new one. **`Subscribe`** (R2.09) streams `Frame`s of `Event`s, live, after replaying any the client missed: operations, workspace changes, lease changes, and presence. Event sequences are daemon-wide and gapless, so resuming after the last `seq` seen loses nothing, and a frame's `coalesced` counts the events the filter left out. A lagging subscriber is caught up from the index and marked `lagged`. **`Replay`** (R2.12) returns the repository as it was at a past sequence, operation, or time: its changes as `Log` with the graph, its bookmarks, and its workspaces reconstructed.

### Trees

[`trees.proto`](../crates/reef-api/proto/reef/daemon/v1/trees.proto), all R2.04: the virtual workspace, for agents that read and write without a directory. **`List`** pages a revision's entries under a path. **`Read`** streams a file's content in 64 KiB chunks, metadata on the first; a conflicted file is refused unless the client asks for the materialized conflict. **`Apply`** is client-streaming: a header naming the change (and optionally the commit the client read it at, refusing if it moved), then whole-file edits (`write` in chunks, `delete`, `symlink`, `copy_from`). It rewrites the change's commit in one operation and checks it out in any workspace on the change. Conflicted paths are refused, not resolved.

### Presence

[`presence.proto`](../crates/reef-api/proto/reef/daemon/v1/presence.proto), R2.10 and R2.11. What each agent says it is doing, in memory only, never in history. **`Report`** is client-streaming: one `PresenceUpdate` per change of activity, and a keepalive at least every half TTL; the entries clear when the stream ends (unless `oneshot`) or expire. **`Subscribe`** streams `Frame`s of presence events: the whole table first, then changes. `Workspace.presence` carries a workspace's live entries in every workspace response.

### Symbols

[`symbols.proto`](../crates/reef-api/proto/reef/daemon/v1/symbols.proto), R2.14. An index of definitions and references keyed by blob, so it is shared across revisions. **`Index`** parses a revision's changed files, or its whole tree, with the daemon's tree-sitter tags. **`Ingest`** takes a SCIP index a client built, streamed in chunks; SCIP rows win over tags for the same blob. **`Lookup`** pages the definitions at a revision by name (exact or prefix) and kind.

### Queries

[`queries.proto`](../crates/reef-api/proto/reef/daemon/v1/queries.proto), R2.16. Named, parameterized queries over the daemon's index; no SQL crosses the API. **`List`** describes each query's parameters and columns; **`Run`** pages its rows as `google.protobuf.ListValue`s.

### Remote

[`remote.proto`](../crates/reef-api/proto/reef/daemon/v1/remote.proto), R2.08, served on the socket only. Bearer tokens for the TCP listeners: **`IssueToken`** (the secret is in the response once and stored only as a hash), **`ListTokens`**, **`RevokeToken`** (by name, id, or id prefix).

### Maintenance

[`maintenance.proto`](../crates/reef-api/proto/reef/daemon/v1/maintenance.proto). **`Gc`** collects jj operations and git objects past the undo window, unused base cache entries, ended workspace rows, and unused stores, per repository, with a dry run. **`Backup`** streams progress while it copies the daemon's state into an empty directory with agents still writing; its last message has `done` and the manifest's content as `summary`. **`Quiesce`** holds the daemon still for a filesystem snapshot until **`Resume`** with its token, or a timeout. Restore is offline (`reef restore`) and has no RPC.

### Stores

[`stores.proto`](../crates/reef-api/proto/reef/daemon/v1/stores.proto). Shared dependency stores (`node_modules`, `target`), built once per lockfile hash and mounted read-only into every workspace with that lockfile. **`Build`** runs the store's command in the daemon and streams its output, then the result. **`List`** returns built stores. **`Prepare`**, **`Seal`**, and **`Abort`** (R2.27) split a build so the client runs the command itself: `Prepare` leases a scratch workspace and returns where and what to run, the client runs it and heartbeats the lease, and `Seal` (with the exit code) or `Abort` ends it.

## Not implemented yet

The daemon answers these with `UNIMPLEMENTED`, detail kind `Internal`, and the hint `not implemented until <ticket>`. The table is `reef_daemon::service::stubs::STUBS`; each ticket removes its rows when it lands.

| RPC | Ticket |
| --- | --- |
| `Workspaces.Status` | R2.02 |
| `Changes.Log`, `Changes.Get`, `Changes.Diff` | R2.02 (`Diff` level `SYMBOLS`: R2.15) |
| `Changes.Describe`, `Changes.Abandon`, `Changes.Squash` | R2.03 |
| `Operations.Log`, `Operations.Get` | R2.02 |
| `Operations.Undo` | R2.03 |
| `Operations.Subscribe` | R2.09 |
| `Operations.Replay` | R2.12 |
| `Trees.List`, `Trees.Read`, `Trees.Apply` | R2.04 |
| `Presence.Subscribe`, `Presence.Report` | R2.10 |
| `Symbols.Index`, `Symbols.Ingest`, `Symbols.Lookup` | R2.14 |
| `Queries.List`, `Queries.Run` | R2.16 |
| `Remote.IssueToken`, `Remote.ListTokens`, `Remote.RevokeToken` | R2.08 |
| `Stores.Prepare`, `Stores.Seal`, `Stores.Abort` | R2.27 |

Each of these RPCs also carries its ticket as a trailing `// R2.xx` comment in its `.proto` file.
