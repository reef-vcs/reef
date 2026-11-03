# TypeScript SDK

`reef-vcs` is the TypeScript client of the Reef daemon ([ADR 0054](adr/0054-sdks.md)). Every RPC of the API is a method on one client, generated from the schema, with a workspace block that holds a workspace the way `reef ws run` holds one for a command.

```sh
npm install reef-vcs
```

Node 22.15 or later, on macOS or Linux. The package is ESM; `require("reef-vcs")` loads it from CommonJS on Node 22.12 and later. It talks to a running daemon and never starts one: install `reef` as the [quickstart](quickstart.md) says, then run any `reef` command (or `reef daemon start`). Bun, Deno and browsers are not supported; the web UI has its own client.

**Versions.** Use the reef-vcs release that matches the daemon's version. `VERSION` is the release, and `API_REVISION` is the schema revision it was generated from. Daemons of other versions serve the calls both know. A call one cannot serve throws `IncompatibleError`, which names the daemon's version and suggests `reef daemon stop`. Before sending a field or calling an RPC added after revision 1 (each one's comment says when), check the daemon: a daemon ignores fields it does not know.

```ts
await client.requireRevision(12); // throws IncompatibleError on an older daemon
```

## Connecting

```ts
import { connect } from "reef-vcs";

const client = await connect();
```

`connect()` reaches the daemon the `reef` CLI would use:

| Setting | Option | Environment | Default |
| --- | --- | --- | --- |
| Remote daemon | `remote` | `REEF_REMOTE` | none: the socket |
| Socket | `socket` | `REEF_SOCKET` | `config.toml`'s `[daemon] socket`, then the platform's |
| State directory | `stateDir` | `REEF_STATE_DIR` | `~/Library/Application Support/reef` (macOS), `$XDG_STATE_HOME/reef` or `~/.local/state/reef` (Linux) |
| Configuration file | `config` | `REEF_CONFIG` | `<state>/config.toml` |
| Agent name | `agent` | `REEF_AGENT` | none |

On Linux the default socket is `$XDG_RUNTIME_DIR/reef/daemon.sock` when the state directory is the default one. Empty variables count as unset, and relative paths are made absolute. A socket path longer than 103 bytes is refused (`ConfigError`).

The agent name is sent with every call, and the operations those calls record name it instead of the workspace's owner. Use a stable name per agent (`planner-3`), visible ASCII of at most 256 bytes.

`connect()` calls `GetDaemonStatus` before it returns, so a daemon that is not running is named there: `DaemonNotRunningError`, with `next` `["reef", "daemon", "start"]`, after `connectTimeoutMs` (5,000) at most. `timeoutMs` is the default deadline for each unary call (none by default).

`client.withOptions({ agent, timeoutMs })` returns a client that shares the connection with other options. `client.target` is the socket path or the remote's URL; `client.daemonStatus()` is the status `connect()` read.

**Closing.** `await client.close()` closes the connection, for every client sharing it; `await using client = await connect()` does the same at the end of the block. A script that never closes still exits when its work is done: the connection does not keep Node alive while no call or stream is open.

### A remote daemon

```ts
const vm = await connect({ remote: "vm" }); // a profile saved by `reef remote add`
const pinned = await connect({
  remote: "https://vm.example:7470",
  token, // `reef remote token create` on the daemon's machine
  fingerprint: "sha256:...", // `reef remote show` on the daemon's machine
});
```

The options override `REEF_REMOTE_TOKEN` and `REEF_REMOTE_FINGERPRINT`, which override a profile's. With a fingerprint, the SDK reads the daemon's certificate on a handshake of its own and checks the key against the pin before anything is sent. The session then trusts only that certificate and checks the key again in every TLS handshake, reconnects included. A mismatch throws `RemoteTlsError` (`failure: "pin_mismatch"`), naming the key the daemon presented. When the URL's host is not one of the certificate's names (an IP address it does not list, say), the SDK names one it does list to TLS and the daemon, and still connects to the URL's host: the pin authenticates the daemon.

Without a fingerprint, the certificate must validate for the URL's host against Node's bundled roots (Mozilla's), `NODE_EXTRA_CA_CERTS` and the operating system's store. For a private CA, pass `rootCertificates: [pem]`. A pinned certificate outside its validity dates throws `RemoteTlsError` (`handshake`); the daemon renews its generated certificate 30 days before it expires.

After a renewal (a new certificate for the same key), the next handshake refuses the old certificate before anything is sent, so the SDK pins the new one and retries the call once, read or write. A call that fails after its request was sent (the daemon restarted under it) throws `UnavailableError` and is not retried. A token that does not allow a call throws `PermissionDeniedError` with the daemon's reason (`REMOTE_TOKEN_INVALID`, `REMOTE_ACCESS_DENIED`, ...). ADMIN calls are refused over a remote; their JSDoc says so.

## Calls

Every RPC is a method in lower camel case, taking an object of the request's fields:

```ts
const status = await client.getStatus({ workspace: wsId });
const created = await client.createWorkspace({
  repo: repoId,
  start: { case: "newOn", value: "main" },
  owner: "planner-3@vm",
});
```

You can also pass a message: `client.getStatus(create(GetStatusRequestSchema, { workspace: wsId }))`. The request is typed: an unknown field or a wrong enum is a type error. A `oneof` is `{ case, value }`. Enums are TypeScript enums (`Backend.APFS`). `Timestamp` and `Duration` fields take protobuf-es's messages, made with `timestampFromDate(date)` and `durationFromMs(ms)`. 64-bit integers are `bigint`, which `JSON.stringify` refuses: log a message with `toJson(Schema, message)`. Messages, enums, schemas and these helpers are importable from `reef-vcs/api/v1`. Each method's JSDoc is the schema's comment for the RPC.

A second argument holds call options: `{ signal }` (an `AbortSignal`) and `{ timeoutMs }`, which overrides the client's default.

**Pages.** A paged RPC also has `<method>Pages(...)`, which yields each response and follows `nextPageToken`. Where its items are known, it has `<method>All(...)`, which yields the items one by one. `timeoutMs` applies to each page.

```ts
for await (const op of client.listOperationsAll({ repo: repoId, pageSize: 200 })) {
  // ...
}
```

**Streams.** `subscribeOperations`, `subscribePresence`, `backup` and `buildStore` return a `Stream`, an async iterable that starts on its first iteration. Breaking out of `for await`, `stream.cancel()`, the call's `signal`, or leaving `await using` ends it; an ended stream stops iterating rather than throwing.

```ts
const stream = client.subscribeOperations({ repos: [repoId] });
for await (const frame of stream) {
  for (const op of frame.operations) console.log(op.id, op.description);
}
```

Streams do not resume by themselves. To survive a daemon restart, keep the last `cursor` and start again from it:

```ts
let cursor = "";
for (;;) {
  try {
    const start = cursor ? { case: "cursor" as const, value: cursor } : { case: undefined };
    for await (const frame of client.subscribeOperations({ repos: [repoId], start })) {
      cursor = frame.cursor || cursor;
      handle(frame.operations);
    }
  } catch (error) {
    if (!(error instanceof UnavailableError)) throw error;
    await new Promise((done) => setTimeout(done, 1_000));
  }
}
```

**Whole files.** `readFile` returns at most 1 MiB. `client.readFileToEnd(...)` reads the rest from the commit the first read returned, so every chunk comes from one commit.

## Errors

Every failure is a `ReefError`, of one class per kind:

| Class | `kind` | Typical cause |
| --- | --- | --- |
| `NotFoundError` | `not_found` | an unknown workspace, repository or revision |
| `AlreadyExistsError` | `already_exists` | |
| `InvalidArgumentError` | `invalid_argument` | a malformed request, revset or path |
| `ConflictError` | `conflict` | a released workspace, an ended lease, a stale commit |
| `UnavailableError` | `unavailable` | the daemon, or something it needs, is away; retry |
| `PermissionDeniedError` | `permission_denied` | a remote token's scope |
| `IoError` | `io` | a filesystem failure on the daemon's machine |
| `InternalError` | `internal` | a bug |
| `UnsupportedError` | `unsupported` | not available on this daemon or platform |

Each carries the daemon's words: `reason` (such as `WORKSPACE_NOT_FOUND`), `message`, `chain`, `hint` (how to fix it), `next` (a `reef` command line that does, as an argv array), `metadata` (values that go with the reason), `code` (Connect's `Code`, exported as `Code`), `method`, and `cause` (the underlying error). `next` is a command for a person or an agent to run, not something the SDK runs.

```ts
try {
  await client.getWorkspace({ workspace: wsId });
} catch (error) {
  if (error instanceof NotFoundError) console.log(error.reason, error.hint, error.next);
}
```

`isReefError(value)` recognizes a ReefError from any installed copy of the package, where `instanceof` cannot.

The SDK's own failures are subclasses: `DaemonNotRunningError` and `RemoteConnectError` (unavailable), `RemoteTlsError` (permission denied; `failure`, `presented`, `expected`), `RemoteMisdirectedError` (not found: the daemon does not serve the URL's host name), `ConfigError` (invalid argument: the socket or the remote could not be settled) and `IncompatibleError` (unsupported; `daemonVersion`, `daemonApi`, `activeWorkspaces`). A request over 4 MiB is refused before it is sent.

## A workspace for a block of code

```ts
await client.withWorkspace({ repo: repoId, purpose: "fix the parser", newOn: "main" }, async (ws) => {
  await writeFile(join(ws.path, "parser.ts"), fixed);
  ws.activity("running the tests");
  await runTests(ws.path);
});
```

On Node 24 and later, or with a transpiler, the same block is `await using`:

```ts
{
  await using ws = await client.workspace({ repo: repoId, purpose: "fix the parser" });
  await writeFile(join(ws.path, "parser.ts"), fixed);
}
```

The block holds the workspace as `reef ws run` holds one for a command:

- **Entering** creates the workspace, owned by `<user>@<host>:<pid>` unless `owner` says otherwise. `change` or `newOn` says where.
- **While it runs**, the block renews the lease on the daemon's cadence and reports one `ACTIVITY` presence entry, which `reef presence watch` and the web UI's fleet page show. The entry's text is `activity`, else the purpose, else the command line. `ws.activity(text)` replaces it, at most once a second. Neither the heartbeats nor presence ever throw into the block. A lost link and an expired lease are emitted as warnings (`process.emitWarning`, type `ReefWarning`, codes `REEF_LEASE_LINK_LOST`, `REEF_LEASE_LAPSED`, ...), which `process.on("warning")` can route; `ws.leaseLost` says whether the daemon ended the lease. `NODE_DEBUG=reef` shows the rest. The SDK never writes to the console.
- **Leaving**, whether the block returned or threw, clears the entry, stops the heartbeats and releases the workspace with a final snapshot, so every edit is in its change. `keep: true` leaves the workspace active instead. With `withWorkspace`, if the release fails after a block that threw, the block's error propagates and the release's is a warning. With `await using`, the language reports both as a `SuppressedError`.

The handle has `id`, `path` (on the daemon's machine), `repoId`, `changeId`, `workspace` (the last `Workspace` message), and the calls an agent makes most, with the workspace filled in: `snapshot()`, `status()`, `diff()`, `readFile(path)` (the whole file, at the workspace's change unless `revision` names another), `editFiles({ edits })` (on the workspace's change unless `target` names another), `move(...)`, `activity(text)` and `release()`.

`client.attach(workspaceId)` and `client.withAttached(workspaceId, options, fn)` hold an existing workspace the same way, by `ws run --workspace`'s rule: it must be active with a live lease and held by `owner` or, without one, by this user on this machine. It is released at the end only with `release: true`.

Over a remote daemon, `ws.path` is on the daemon's machine; use `ws.readFile` and `ws.editFiles` there.

**Signals.** The SDK installs no signal handlers. A process killed inside a block leaves the lease to lapse and the presence entry to expire (30 s); every edit is already in the change, since the daemon snapshots what is written. For a prompt release on Ctrl-C, release in your own handler:

```ts
const ws = await client.workspace({ repo: repoId });
process.once("SIGINT", () => void ws.release().finally(() => process.exit(130)));
```

## Presence

The workspace block reports for you. To report finer activity, such as the files an agent is editing or a long step's progress, use a `Reporter`:

```ts
import { Reporter } from "reef-vcs";
import { PresenceKind } from "reef-vcs/api/v1";

const reporter = new Reporter(client, { agent: "planner-3", kind: PresenceKind.EDITING, workspace: ws.id });
reporter.set({ paths: ["src/parser.ts"] });
// ...
await reporter.clear();
```

It follows the reporting contract in `ReportPresence`'s comment (R2.11). It holds one entry per subject and replaces the entry at most once a second. With `REFRESH` (the default), it refreshes the entry every 10 s with a 30 s TTL; with `BURST`, it sends one report per `set`, never refreshed, with a 60 s TTL. If the daemon no longer knows the entry, it reports it again. Clearing waits at most 2 s. Failures never reach the caller.

## Developing the SDK

The SDK lives in `sdk/typescript/`, and its code is generated from `crates/reef-api/proto` at every build and never committed. It needs Node and npm (`cd sdk/typescript && npm ci` once):

```sh
just typescript-check    # generate, ESLint, Prettier, tsc, the build, the unit tests
cargo build -p reef-vcs -p reef-daemon && just typescript-test   # against scratch daemons
just typescript-build    # the tarball into sdk/typescript/dist-pack, checked by publint and attw
```
