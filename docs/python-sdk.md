# Python SDK

`reef-vcs` is the Python client of the Reef daemon ([ADR 0054](adr/0054-sdks.md)). Every RPC of the API is a method on one client, generated from the schema, with a workspace block that holds a workspace the way `reef ws run` holds one for a command.

```sh
pip install reef-vcs
```

Python 3.11 or later, on macOS or Linux. The package talks to a running daemon and never starts one: install `reef` as the [quickstart](quickstart.md) says, then run any `reef` command (or `reef daemon start`).

**Versions.** Use the reef-vcs release that matches the daemon's version. `reef.__version__` is the release, and `reef.API_REVISION` is the schema revision it was generated from. Daemons of other versions serve the calls both know. A call one cannot serve raises `reef.IncompatibleError`, which names the daemon's version and suggests `reef daemon stop`. Before sending a field or calling an RPC added after revision 1 (each one's comment says when), check the daemon: a daemon ignores fields it does not know.

```python
client.require_revision(12)   # raises IncompatibleError on an older daemon
```

## Connecting

```python
import reef

client = reef.connect()
```

`reef.connect()` reaches the daemon the `reef` CLI would use:

| Setting | Argument | Environment | Default |
| --- | --- | --- | --- |
| Remote daemon | `remote=` | `REEF_REMOTE` | none: the socket |
| Socket | `socket` (first argument) | `REEF_SOCKET` | `config.toml`'s `[daemon] socket`, then the platform's |
| State directory | `state_dir=` | `REEF_STATE_DIR` | `~/Library/Application Support/reef` (macOS), `$XDG_STATE_HOME/reef` or `~/.local/state/reef` (Linux) |
| Configuration file | `config=` | `REEF_CONFIG` | `<state>/config.toml` |
| Agent name | `agent=` | `REEF_AGENT` | none |

On Linux the default socket is `$XDG_RUNTIME_DIR/reef/daemon.sock` when the state directory is the default one. Empty variables count as unset, and relative paths are made absolute. A socket path longer than 103 bytes is refused (`ConfigError`).

The agent name is sent with every call, and the operations those calls record name it instead of the workspace's owner. Use a stable name per agent (`planner-3`), visible ASCII of at most 256 bytes.

Other options: `timeout=` is the default deadline for each unary call, in seconds (none by default). `connect_timeout=` (5 s) bounds the wait for the connection; when nothing answers in that time, `reef.DaemonNotRunningError` is raised with `next == ["reef", "daemon", "start"]`.

`Client` is thread-safe and a context manager. `client.with_options(timeout=..., agent=...)` returns a client that shares the connection with other options. `client.target` is the socket path or the remote's URL.

### A remote daemon

```python
client = reef.connect(remote="vm")   # a profile saved by `reef remote add`
client = reef.connect(
    remote="https://vm.example:7470",
    token=token,                     # `reef remote token create` on the daemon's machine
    fingerprint="sha256:...",        # `reef remote show` on the daemon's machine
)
```

The arguments override `REEF_REMOTE_TOKEN` and `REEF_REMOTE_FINGERPRINT`, which override a profile's. With a fingerprint, the SDK reads the daemon's certificate on a handshake of its own and checks the key against the pin before anything is sent. A mismatch raises `reef.RemoteTlsError` (`failure == "pin_mismatch"`), naming the key the daemon presented. When the URL's host is not one of the certificate's names (an IP address it does not list, say), the SDK gives grpc and the daemon one of the names it does list: the pin authenticates the daemon.

Without a fingerprint, the certificate must validate for the URL's host against **grpcio's bundled roots** (Mozilla's), not the operating system's, which the `reef` CLI uses. For a private CA, pass `root_certificates=pem_bytes` or set `GRPC_DEFAULT_SSL_ROOTS_FILE_PATH`. grpc also checks a certificate's validity dates, which a pin does not need. The daemon renews its generated certificate 30 days before it expires; a lapsed one raises `RemoteTlsError` (`handshake`).

After a renewal (a new certificate for the same key), the SDK pins the new certificate. A read is retried once on the new channel. A write's `UnavailableError` is raised instead, since the write may have landed; the next call goes through. A token that does not allow a call raises `PermissionDeniedError` with the daemon's reason (`REMOTE_TOKEN_INVALID`, `REMOTE_ACCESS_DENIED`, ...). ADMIN calls are refused over a remote; their docstrings say so.

## Calls

Every RPC is a method in snake case, with the request's fields as keywords:

```python
status = client.get_status(workspace=ws_id)
created = client.create_workspace(repo=repo_id, new_on="main", owner="planner-3@vm")
```

You can also pass a request message: `client.get_status(reef.api.v1.GetStatusRequest(workspace=ws_id))`. Passing a message and keywords together is a `TypeError`. Enum fields take a member or its name (`backend="BACKEND_APFS"`). `Duration` and `Timestamp` fields take a `timedelta` or a `datetime`. Messages and enums are importable from `reef.api.v1`. Each method's docstring is the schema's comment for the RPC and its fields.

**Pages.** A paged RPC also has `<method>_pages(...)`, which yields each response and follows `next_page_token`. Where its items are known, it has `<method>_all(...)`, which yields the items one by one:

```python
for op in client.list_operations_all(repo=repo_id, page_size=200):
    ...
```

**Streams.** `subscribe_operations`, `subscribe_presence`, `backup` and `build_store` return a stream. Iterate it; leaving its `with` block, or `stream.cancel()`, ends it, and a cancelled stream stops iterating rather than raising.

```python
with client.subscribe_operations(repos=[repo_id]) as stream:
    for frame in stream:
        for op in frame.operations:
            print(op.id, op.description)
```

Streams do not resume by themselves. To survive a daemon restart, keep the last `cursor` and start again from it:

```python
cursor = None
while True:
    try:
        with client.subscribe_operations(repos=[repo_id], cursor=cursor) as stream:
            for frame in stream:
                cursor = frame.cursor or cursor
                handle(frame.operations)
    except reef.UnavailableError:
        time.sleep(1)
```

## Errors

Every failure is a `reef.ReefError`, of one class per kind:

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

Each carries the daemon's words: `reason` (such as `WORKSPACE_NOT_FOUND`), `message`, `hint` (how to fix it), `next` (a `reef` command line that does, as an argv list), `metadata` (values that go with the reason), `code` (the gRPC status code) and `method`. `str(error)` is the message, then the hint on its own line. `next` is a command for a person or an agent to run, not something the SDK runs.

```python
try:
    client.get_workspace(workspace=ws_id)
except reef.NotFoundError as error:
    print(error.reason, error.hint, error.next)
```

The SDK's own failures are subclasses: `DaemonNotRunningError` and `RemoteConnectError` (unavailable), `RemoteTlsError` (permission denied; `failure`, `presented`, `expected`), `RemoteMisdirectedError` (not found: the daemon does not serve the URL's host name), `ConfigError` (invalid argument: the socket or the remote could not be settled) and `IncompatibleError` (unsupported; `daemon_version`, `daemon_api`, `active_workspaces`). A request over 4 MiB is refused before it is sent.

## A workspace for a block of code

```python
with client.workspace(repo_id, purpose="fix the parser", new_on="main") as ws:
    (ws.path / "parser.py").write_text(fixed)
    ws.activity("running the tests")
    run_tests(ws.path)
```

The block holds the workspace as `reef ws run` holds one for a command:

- **Entering** creates the workspace, owned by `<user>@<host>:<pid>` unless `owner=` says otherwise.
- **While it runs**, the block renews the lease on the daemon's cadence and reports one `ACTIVITY` presence entry, which `reef presence watch` and the web UI's fleet page show. The entry's text is `activity=`, else the purpose, else the command line. `ws.activity(text)` replaces it, at most once a second. Neither the heartbeats nor presence ever raise into the block: a lost link and an expired lease are logged at `warning` on the `reef` logger, and `ws.lease_lost` says whether the daemon ended the lease.
- **Leaving**, whether the block returned or raised, clears the entry, stops the heartbeats and releases the workspace with a final snapshot, so every edit is in its change. `keep=True` leaves the workspace active instead. If the release fails after a block that raised, the block's exception propagates and the release's is logged.

The handle has `id`, `path` (a `pathlib.Path` on the daemon's machine), `repo_id`, `change_id`, `workspace` (the last `Workspace` message), and the calls an agent makes most, with the workspace filled in: `snapshot()`, `status()`, `diff()`, `read_file(path)`, `edit_files(edits=...)` (on the workspace's change unless `revision` or `new_on` names another), `move(...)`, `activity(text)` and `release()`.

`client.attach(workspace_id)` holds an existing workspace the same way, by `ws run --workspace`'s rule: it must be active with a live lease and held by `owner=` or, without one, by this user on this machine. It is released at the end only with `release=True`.

Over a remote daemon, `ws.path` is on the daemon's machine; use `ws.read_file` and `ws.edit_files` there.

## Presence

The workspace block reports for you. To report finer activity, such as the files an agent is editing or a long step's progress, use a `Reporter`:

```python
from reef.presence import Reporter, State, Subject
from reef.api.v1 import PresenceKind

reporter = Reporter(
    client, Subject(agent="planner-3", kind=PresenceKind.PRESENCE_KIND_EDITING, workspace=ws.id)
)
reporter.set(State(paths=["src/parser.py"]))
...
reporter.clear()
```

It follows the reporting contract in `ReportPresence`'s comment (R2.11). It holds one entry per subject and replaces the entry at most once a second. With `REFRESH` (the default), it refreshes the entry every 10 s with a 30 s TTL; with `BURST`, it sends one report per `set`, never refreshed, with a 60 s TTL. If the daemon no longer knows the entry, it reports it again. Clearing waits at most 2 s. Failures never reach the caller.

## asyncio

`reef.aio` has the same surface, awaited:

```python
import reef.aio

async with await reef.aio.connect() as client:
    status = await client.get_status(workspace=ws_id)
    async for op in client.list_operations_all(repo=repo_id):
        ...
    async with client.subscribe_operations(repos=[repo_id]) as stream:
        async for frame in stream:
            ...
    async with client.workspace(repo_id, purpose="async work") as ws:
        await ws.snapshot()
```

An `AsyncClient` belongs to the event loop it was created on; using it from another loop raises `RuntimeError`. `reef.aio.presence.Reporter` is the asyncio reporter.

## Threads and fork

The sync client is safe to share between threads; its heartbeats and presence run on daemon threads. As with any grpc program, create clients after `os.fork()`, not before. On import, the SDK sets `GRPC_ENABLE_FORK_SUPPORT=false` unless the environment already sets it, because grpc's fork handlers log two lines on every `subprocess` call made while a channel is open. grpc reads it as it starts, so it stays set, and the program's child processes inherit it. Set it yourself before importing `reef` (or grpc) to choose otherwise.

## Developing the SDK

The SDK lives in `sdk/python/`, and its code is generated from `crates/reef-api/proto` at every build and never committed. It needs [uv](https://docs.astral.sh/uv/):

```sh
just python-check    # generate, ruff, mypy --strict, the unit tests
cargo build -p reef-vcs -p reef-daemon && just python-test   # against scratch daemons
just python-build    # sdist and wheel into sdk/python/dist, checked by twine
```
