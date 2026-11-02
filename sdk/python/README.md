# reef-vcs

The Python client of the [Reef](https://github.com/reef-vcs/reef) daemon. Reef gives each agent its
own workspace: a copy-on-write copy of a git or jj repository, leased to one process and
snapshotted after every write. Work lands as a jj change; nothing is lost when a workspace is
released.

```sh
pip install reef-vcs
```

The package talks to a running daemon and never starts one. Install `reef` as the
[quickstart](https://github.com/reef-vcs/reef/blob/main/docs/quickstart.md) says; any `reef`
command starts the daemon, or run `reef daemon start`. Use the reef-vcs release that matches the
daemon's version: older and newer daemons serve the calls both know, and a mismatch is reported as
`reef.IncompatibleError`. Python 3.11 or later, on macOS or Linux.

## Connect

```python
import reef

client = reef.connect()                    # the daemon the reef CLI uses
client = reef.connect(agent="planner-3")   # name the agent on every operation it records
client = reef.connect(remote="vm")         # a profile saved by `reef remote add`
```

`reef.connect()` finds the socket as `reef` does (`REEF_SOCKET`, `REEF_STATE_DIR`, `REEF_CONFIG`,
then the platform's default), or the remote daemon `REEF_REMOTE` names.

## Status and history

Every RPC is a method on the client, with the request's fields as keywords:

```python
status = client.get_status(workspace=ws_id)
for op in client.list_operations_all(repo=repo_id):   # every page
    print(op.id, op.description)
```

Messages and enums are importable from `reef.api.v1`.

## A workspace for a block of code

```python
with reef.connect() as client:
    repo = client.register_repository(path=".").repository
    with client.workspace(repo.id, purpose="fix the parser") as ws:
        (ws.path / "parser.py").write_text(fixed)
        ws.activity("running the tests")
    # Released with a final snapshot: the edit is in the workspace's change.
```

While the block runs, the lease is renewed and a presence entry shows it in `reef presence watch`
and the web UI, as `reef ws run` does for a command.

## Subscriptions, with asyncio

```python
import reef.aio

async with await reef.aio.connect() as client:
    async with client.subscribe_operations(repos=[repo_id]) as stream:
        async for frame in stream:
            for op in frame.operations:
                print(op.id)
```

## A remote daemon

```python
client = reef.connect(
    remote="https://vm.example:7470",
    token=token,                            # from `reef remote token create` on that machine
    fingerprint="sha256:...",               # from `reef remote show` on that machine
)
```

The daemon's key is checked against the fingerprint before the token is sent.

## Errors

Every failure is a `reef.ReefError` subclass by kind (`NotFoundError`, `ConflictError`, ...), with
the daemon's `reason`, `hint` (how to fix it) and `next` (a `reef` command line that does):

```python
try:
    client.get_workspace(workspace=ws_id)
except reef.NotFoundError as error:
    print(error.reason, error.hint, error.next)
```

Read more in the [Python SDK guide](https://github.com/reef-vcs/reef/blob/main/docs/python-sdk.md).
