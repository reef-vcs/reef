# reef-vcs

The TypeScript client of the [Reef](https://github.com/reef-vcs/reef) daemon. Reef gives each agent
its own workspace: a copy-on-write copy of a git or jj repository, leased to one process and
snapshotted after every write. Work lands as a jj change; nothing is lost when a workspace is
released.

```sh
npm install reef-vcs
```

The package talks to a running daemon and never starts one. Install `reef` as the
[quickstart](https://github.com/reef-vcs/reef/blob/main/docs/quickstart.md) says; any `reef`
command starts the daemon, or run `reef daemon start`. Use the reef-vcs release that matches the
daemon's version: older and newer daemons serve the calls both know, and a mismatch is reported as
`IncompatibleError`. Node 22.15 or later, on macOS or Linux; ESM, and `require()` works from
Node 22.12.

## Connect

```ts
import { connect } from "reef-vcs";

const client = await connect(); // the daemon the reef CLI uses
const named = await connect({ agent: "planner-3" }); // name the agent on every operation it records
const remote = await connect({ remote: "vm" }); // a profile saved by `reef remote add`
```

`connect()` finds the socket as `reef` does (`REEF_SOCKET`, `REEF_STATE_DIR`, `REEF_CONFIG`, then
the platform's default), or the remote daemon `REEF_REMOTE` names. Close the client with
`await client.close()`, or hold it with `await using`; a script that forgets still exits.

## Status and history

Every RPC is a method on the client, taking an object of the request's fields:

```ts
const status = await client.getStatus({ workspace: wsId });
for await (const op of client.listOperationsAll({ repo: repoId })) {
  console.log(op.id, op.description); // every page
}
```

Messages, enums and protobuf-es's helpers are importable from `reef-vcs/api/v1`. 64-bit integers
are `bigint`: log a message with `toJson(Schema, message)`.

## A workspace for a block of code

```ts
await client.withWorkspace({ repo: repoId, purpose: "fix the parser" }, async (ws) => {
  await writeFile(join(ws.path, "parser.ts"), fixed);
  ws.activity("running the tests");
});
// Released with a final snapshot: the edit is in the workspace's change.
```

On Node 24 and later, or with a transpiler, the same block is `await using`:

```ts
await using ws = await client.workspace({ repo: repoId, purpose: "fix the parser" });
```

While it is held, the lease is renewed and a presence entry shows it in `reef presence watch` and
the web UI, as `reef ws run` does for a command.

Each commit the daemon records for the workspace gets a signed provenance record. Say what the
work is for, at the start and as it changes; an instruction is hashed here, and only its SHA-256 is
sent (`REEF_MODEL` and `REEF_TASK` fill a model and task you do not give):

```ts
await using ws = await client.workspace({
  repo: repoId,
  provenance: { model: "example/model-1", task: "T-123", instruction: prompt },
});
await ws.declare({ instruction: nextPrompt }); // snapshots first; "" clears a field
```

## Subscriptions

```ts
const stream = client.subscribeOperations({ repos: [repoId] });
for await (const frame of stream) {
  for (const op of frame.operations) console.log(op.id);
}
```

Breaking out of the loop, `stream.cancel()`, or an `AbortSignal` passed as `{ signal }` ends it.

## A remote daemon

```ts
const client = await connect({
  remote: "https://vm.example:7470",
  token, // from `reef remote token create` on that machine
  fingerprint: "sha256:...", // from `reef remote show` on that machine
});
```

The daemon's key is checked against the fingerprint in every TLS handshake, before the token is
sent.

## Errors

Every failure is a `ReefError` subclass by kind (`NotFoundError`, `ConflictError`, ...), with the
daemon's `reason`, `hint` (how to fix it) and `next` (a `reef` command line that does):

```ts
import { NotFoundError } from "reef-vcs";

try {
  await client.getWorkspace({ workspace: wsId });
} catch (error) {
  if (error instanceof NotFoundError) console.log(error.reason, error.hint, error.next);
}
```

Read more in the
[TypeScript SDK guide](https://github.com/reef-vcs/reef/blob/main/docs/typescript-sdk.md).

## Maintainers

Releases publish to npm from the repository's `release.yml` by trusted publishing, with
provenance; see the R2.07 brief's one-time setup.
