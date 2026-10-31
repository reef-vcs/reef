---
name: reef
description: Isolate work in this repository in a Reef workspace (reef ws run), find out where you are (reef here), and read errors' next field for the fix.
generated-by: reef init
---

Written by `reef init` (reef 0.2.0); `reef guide` prints the current guide.

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
  ends it. `reef ws list` counts each one's changed files;
  `reef ws inspect <id>` has the rest. Ids take 4+ characters.
- `cd "$(reef ws move -o text <id> --new-on main)"` snapshots and starts the next task
  in the same directory and lease (`cd` again: overlays remount).
- `--workspace <id>` or `--repo <path|id>` picks what to read (default: here).

## Inside ws run

It runs in the workspace with `REEF_WORKSPACE`, `REEF_WORKSPACE_PATH`,
`REEF_REPO` and `REEF_SOCKET` set, its lease renewed until it exits.

## Output

Piped output is brief JSON; `-o json` or `REEF_OUTPUT=json` has every field.
Errors: `{"error": {kind, reason, message, hint, next, metadata}}`. Exit 0 ok,
2 usage, 3 not found, 4 exists, 5 conflict, 6 unavailable, 7 denied,
8 I/O, 9 unsupported, 1 other; `ws run` exits with its command's code.

## Changes

`reef status`, `log`, `show`, `diff`, `describe -m <text>`, `undo <op>` (`op log`).
Revisions are jj revsets.
