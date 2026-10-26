---
name: reef
description: Isolate work in this repository in a Reef workspace (reef ws run), find out where you are (reef here), and read errors' next field for the fix.
generated-by: reef init
---

Written by `reef init` (reef 0.1.0); `reef guide` prints the current guide.

# Reef

Reef gives each agent its own workspace: a copy-on-write copy of a git or
jj repository, leased to one process and snapshotted after every write.
Work lands as a jj change; nothing is lost when a workspace is released.

From inside the repository:

    reef ws run -- <command>   # new workspace, run there, release it
    reef here                  # where am I, and what next

The first command in a repository registers it (adding jj beside git).
Output is JSON when piped; errors carry `hint` and `next`, a command to run.

## Commands

- `reef ws create` prints a new workspace's path; `reef ws release <id>`
  ends it. `reef ws list`, `reef ws inspect <id>`. Ids take 4+ characters.
- `cd "$(reef ws move -o text <id> --new-on main)"` snapshots, then starts the next
  task in the same directory and lease; `cd` again, as an overlay is remounted.
- `--repo <path|id>` names the repository; default: the current directory's.

## Inside ws run

The command runs in the workspace directory with `REEF_WORKSPACE`,
`REEF_WORKSPACE_PATH`, `REEF_REPO`, and `REEF_SOCKET` set; its lease is
renewed until it exits.

## Output

`-o json` or `REEF_OUTPUT=json` forces JSON: one object or array, or
`{"error": {"kind", "reason", "message", "hint", "next"}}`. Exit 0 ok,
2 usage, 3 not found, 4 exists, 5 conflict, 6 unavailable, 7 denied,
8 I/O, 9 unsupported, 1 other; `ws run` exits with its command's code.

## Not yet

Undo, status, and diff come in Phase 2: run `jj` in the repository's own
root, not in a workspace, to see changes.
