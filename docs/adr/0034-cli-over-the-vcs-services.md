# 0034. CLI over the VCS services

- Status: accepted
- Date: 2026-10-26
- Ticket: R2.27

## Context

R2.02 to R2.04 implemented the read side, the write side and the file API of `reef.api.v1` ([0030](0030-reads-over-jj-state.md), [0031](0031-writes-over-jj-state.md), [0032](0032-files-without-a-workspace.md)), and each left its commands to the CLI. Until then an agent, or the person watching it, looked at and reverted what agents did with `jj` in the repository's own checkout, which the guide and the quickstart said. [0025](0025-agent-facing-cli-contract.md) fixed the CLI's contract: never interactive, JSON when piped, `hint` and `next` on every error with a known fix, names only ever added. [0033](0033-thin-client.md) made the CLI a thin client with no jj-lib.

Three things the services do not decide: which workspace a command means when it is run from a directory, what paths given on the command line are relative to, and what a CLI error says, since the daemon's advice is in the API's words and cannot name a command it never saw.

## Decision

1. **Twelve commands.** `reef status`, `log`, `show`, `diff`, `describe`, `abandon`, `undo`, `op log`, `op show`, `ws snapshot`, `file read` and `file list`, each one or two calls through `reef_client::Client`. No `file edit`: from a shell, the workspace is the editor, and agents without a directory reach `EditFiles` through MCP (R2.13).
2. **The scope, resolved once.** Every command but `op log` and `op show` resolves a scope before its own call: `--workspace <id, prefix, or path>`, else `--repo <id, prefix, or path>` (the repository and no workspace), else `REEF_WORKSPACE` (never falling back to the directory), else the active workspace containing the current directory, else the repository containing it, registering it as any command does (R1.25). A `--repo` path inside a workspace means that workspace's repository. Requests then carry full ids, so nothing resolves twice. `op log` and `op show` need only the repository. `ws snapshot`'s workspace is its argument, else steps three and four.
3. **Paths in are relative, paths out are not.** A path given inside the scope's directory is relative to the current directory, as `jj` takes paths, resolved lexically (it may exist only at another revision); with `--workspace` or `--repo`, or from outside the directory, it is repository-relative as given. A path that leaves the directory is refused before any call (`INVALID_PATH`). Every path printed is repository-relative, the API's form, so a printed path and a `next` mean the same from any directory.
4. **The JSON of v1 messages**, in `reef_client::json` beside Phase 1's types: the v1 field names; full lowercase hex ids, with `change_id_reverse_hex` beside every `change_id`; enums as lowercase names with `-` between words, `unknown` for a value this build does not know and `null` when unset; a string the API leaves empty to mean "none" as `null`, free text as a string. A list is one array. A write's answer carries the operation it recorded, or `null` when it recorded none.
5. **`metadata` in the error object.** `ErrorDetail.metadata` is printed as `error.metadata`, an object of strings, when it is not empty, for every command: an agent reads `AMBIGUOUS_REVISION`'s commits or `EXPECTED_COMMIT_MISMATCH`'s current commit as data. Additive ([0025](0025-agent-facing-cli-contract.md) decision 5).
6. **The CLI's advice for API reasons.** An error whose daemon advice has a `next` keeps it. Otherwise a table keyed by reason (and, for `OPERATION_NOT_FOUND`, by whether the command was given an operation) replaces the hint and builds the `next` from the scope, the command's own arguments and the error's metadata, never a placeholder: `reef log -r <the commits>` for an ambiguous revision, `reef show <commit>` for a stale `--expected-commit`, `reef op log --repo <id>` for operation errors, `reef file list` or `file read` for a path of the wrong type, the command again for a page error, and the command again with `--no-snapshot` for `SNAPSHOT_FAILED`. A reason not in the table keeps the daemon's advice. The daemon gains that one reason, on a failed snapshot before a read or a write, with metadata `workspace_id` and the snapshot error's own kind.
7. **`undo` names its operation, `abandon` its revision.** Watchers and other agents record operations every second, so "the last operation" is rarely the caller's (R3.26 scopes undo by agent), and `@` is the change the caller stands on. `undo` with no operation fails with `reef op log` as its `next`.
8. **Never interactive.** `describe` requires `-m`, joins several as `jj describe` does, and never opens an editor.
9. **`file read` prints the file's bytes, even on a pipe**, as `config show` prints TOML, one 1 MiB range at a time, each after the first at the first range's commit; `-o json` asks for the object, with `content` when the bytes are UTF-8 and `content_base64` when not.
10. **Writes return the workspace.** `describe`, `abandon`, `undo` and `ws snapshot` run in a workspace read it once more afterwards and return it as `workspace`; when it is divergent, a stderr note names the `reef log` that lists its commits and the `reef abandon` that resolves it, and the exit stays 0. `reef here` gives a divergent workspace the same `next`. This is how the CLI shows issue #80 (a write racing a busy workspace's edits) until it is fixed.

## Consequences

- An agent needs no `jj` to see or revert what agents did; the guide's long form names the commands, and the quickstart uses `reef` throughout.
- `reef status` is a command; it is no longer a usage error pointing at `reef here`, which stays the cheap "where am I" (no snapshot, no diff).
- Prefixes change, as [0025](0025-agent-facing-cli-contract.md) foresaw: `reef s` and `reef st` (formerly `store`), `reef d` (formerly `daemon`) and `reef a` (formerly `guide`, through `agent-guide`) are ambiguous, a usage error listing the candidates. Full names, and `sto`, `da`, `gu`, keep working.
- A path printed from a subdirectory (`src/a.rs`) and passed back from there means another path (`src/src/a.rs`). An absolute path, or `--workspace`, always works.
- The advice table can drift from the daemon's reasons; a reason without a row keeps the daemon's API-worded hint. A later reason a CLI command can hit adds its row in the same change.
- A read command in an unregistered git checkout registers it, as every command does (R1.25); `--no-register` opts out.

## Alternatives considered

- **A global `--workspace`.** It collides with `ws run --workspace`; the scope flags are flattened into the commands that take them.
- **Undo of the last operation.** Racy with watchers and other agents, as above.
- **The daemon building CLI argv from request fields.** It cannot tell `reef diff` from `reef show`, and MCP and SDK callers would get CLI commands for API calls; their hints stay in the API's words.
- **A text graph in `log`.** The JSON carries the edges; the web UI (R2.19) draws them.
- **Printing paths relative to the current directory.** Two forms of one path, and a `next` must run from anywhere.
- **Parsing the daemon's hint to detect a failed snapshot.** The CLI never parses messages ([0033](0033-thin-client.md) removed the last place it did); a reason costs one constant.
