# The workspace provider contract

A program that isolates each session in its own directory, usually by making a git worktree, can ask Reef for a workspace instead. It runs three commands, with JSON in and out: `reef ws provider create`, `attach` and `release`. This page is the contract. [ADR 0053](adr/0053-workspace-provider-contract.md) explains its decisions.

## What a workspace is, and is not

A Reef workspace is a copy-on-write directory on one jj change, snapshotted as it is written ([quickstart](quickstart.md)). It is not a git checkout:

- It has no `.git` and no `.jj`. `git status` in it fails, and a check for uncommitted changes that runs git finds none.
- Work lands as a jj change in the repository's log (`reef log`, `jj log`), not as a branch.
- It lives at the daemon's path, under its state directory, never inside the repository's checkout.
- Untracked files of the checkout, such as a `.env`, are not copied into it. Reef's shared stores give a workspace its dependencies.

Nothing is lost when a workspace goes: each release, and each reclaim, takes a final snapshot first.

## The commands

Each command takes its fields as flags, or as one JSON object on stdin keyed by the same names (`--stdin`). `--at FIELD=POINTER` takes a field from anywhere in that object by an [RFC 6901](https://www.rfc-editor.org/rfc/rfc6901) JSON pointer, so a program's own event maps onto the contract without code. The order is: a flag, then a pointer, then the object's own key, then the default. Keys a command does not know are ignored. `reef schema cli ws provider create` prints each command's input and output schema.

### `create`

| Field | Meaning |
| --- | --- |
| `repo` | The repository: an id, a prefix, or a path inside it. Default: the current directory's. A path inside an active workspace starts the new one on that workspace's change, after a snapshot. |
| `name` | Any non-empty text. A name of 1 to 64 ASCII letters, digits, `.`, `_` and `-`, not all digits and not `mcp`, is used as given; any other is made one, the same way every time (other characters become `-`, and a short hash of the name is added). With a name, `create` returns the active workspace of that name if there is one (`attached: true`), so a retry or a repeated name gets the same directory. If that name's last workspace has ended and its change was kept, a new one starts on that change, so a session resumed after a reclaim finds its files. |
| `purpose` | What it is for. Default: the name. |
| `owner` | Who holds it. Default: `<user>@<host>:<name>`, else `<user>@<host>`. Not with `name`: a name is held through its owner, so the two together are refused. |
| `change`, `new_on` | Edit this revision, or start a new change on it. Default: as `reef ws create`. |
| `lease` | The lease length, such as `30m`. Default: the daemon's `lease.default`. |
| `hold_while` | The holder: a pid, or `parent` (below). |

It prints:

```json
{"contract": 1, "workspace_id": "3f2a…", "repo_id": "a3dd…", "path": "/…/ws/3f2a…",
 "change_id": "…", "owner": "me@host:fix-parser", "purpose": "fix-parser",
 "lease": {…}, "holder": {"pid": 4242, "follower_pid": 4250}, "attached": false,
 "start": "main"}
```

With `-o text` it prints the path alone, one line.

### `attach`

Takes `workspace` (an id, a prefix, or a path inside it; required), `owner` (the owner it must have; default: this user on this machine) and `hold_while`. It holds an existing active workspace for a new holder, as a restarted or resumed session does. It renews the lease once, which gives a lease in its grace period its full length back. It adds a holder and prints the same object as `create`, with `attached: true`; earlier holders that still run keep holding it. A released or reclaimed workspace is refused (`conflict`, exit 5), and its `next` starts a new workspace on its change.

### `release`

Takes `workspace` (required) and `hold_while`. It snapshots the workspace, removes its directory, ends its lease, and stops its followers. With `hold_while`, it releases on that holder's behalf only: while another holder still runs, it stops that holder's follower and keeps the workspace, printing `released: false`, `state: "active"` and `held_by`, the number of other holders. A program whose sessions can share a workspace passes its own session here.

```json
{"contract": 1, "workspace_id": "3f2a…", "released": true, "state": "released",
 "snapshot_taken": true, "final_op_id": "…", "held_by": 0}
```

By id, releasing a workspace that already ended, or is being released or reclaimed, is not an error: it prints `released: false` and the state it is in. By path, a directory that is gone is `not_found`, exit 3.

### Errors

A failure prints Reef's error object, `{"error": {kind, reason, message, hint, next, metadata}}`, and exits with its kind's code (2 for `invalid_argument`, 3 for `not_found`, 5 for `conflict`, 8 for `io`). Two reasons are the provider's own:

- `PROVIDER_INPUT_INVALID`: a field of the wrong type, a bad name, stdin that is not one JSON object or is over 1 MiB, or an `--at` naming no field. `metadata.field` names the field.
- `HOLDER_NOT_RUNNING`: `hold_while` names no running process. Nothing is created.

A program that got an error has no workspace left behind. The commands run only against a local daemon: `--remote` is refused, since a holder is a local process.

`contract` is 1. Fields are added without changing it; an incompatible change to the input or output changes it.

## The holder

`hold_while` names the process whose life the workspace's lease follows, usually the program's own session:

- A pid holds the workspace while that process runs. A reused pid is told apart by its start time.
- `parent` is the nearest ancestor of `reef` that is not a shell (`sh`, `bash`, `zsh`, `dash`, `ksh`, `mksh`, `fish`, `csh`, `tcsh`, `nu`, `pwsh`), up to eight levels. A program that runs its hooks through a shell gets itself. A wrapper that is not a shell (`timeout`, `make`, `npm run`) becomes the holder and ends with the hook, so run `reef` directly from the hook, or pass the program's pid.

`create` and `attach` start a follower, a detached `reef ws renew --follow --while-pid <holder>`, that renews the lease on the daemon's cadence while the holder runs. When the holder exits, the follower exits and the lease runs out. The daemon then reclaims the workspace with a final snapshot, at most the lease plus its grace after the holder's last renewal: 35 minutes by default. Nothing is released at the holder's death, so a session restarted or resumed within that time `attach`es and finds its directory. A workspace may have several holders, each with its own follower, and is held while any of them runs: no holder takes a workspace from another.

While a follower holds a workspace, `reef here` inside it reports `held: true` and does not suggest releasing it, since the program that created it will. Each follower keeps a record, `<state>/holders/<workspace id>.<follower pid>.json`, while it runs; a record whose follower has gone is ignored and removed by the next reader.

Without `hold_while` there is no follower, and the lease is the caller's to renew (`reef ws renew`, or `attach`).

## Writing an adapter

An adapter is configuration, not code:

1. Run `create` when the program makes a session's directory. Map the program's event onto the fields with `--at`, for example `--at repo=/cwd --at name=/name`. Pass `--hold-while parent`, and `-o text` if the program reads a path from the command's output.
2. Run `release` when the program removes the directory, mapping `workspace` to the path or id it gives.
3. If the program resumes sessions in their old directories, run `attach --at workspace=/cwd --hold-while parent` from the first event that names the session's directory after it resumes. An event that fires before the session is back in its directory names the wrong one. If that event fires often, such as on every prompt, add `--no-autostart` and ignore the exit code: outside a workspace it is a lookup that finds nothing, and in a workspace that is already held by the same process it keeps the follower.

Reef's own adapters are rows of its agent table, rendered into each program's package format.
