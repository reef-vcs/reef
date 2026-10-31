# 0045. Brief JSON by default

- Status: accepted
- Date: 2026-10-31
- Ticket: issue #114

## Context

Phase 2's exit criterion asks that the tokens an agent needs to answer "what is the state of this repo" be at least five times fewer than parsing git's output. R2.17 measures it on its `state-repo` question ("which other agents have work not on `main` yet, and how many files has each changed?"), on the output an agent gets unasked. On a pipe that was pretty-printed JSON with every field ([0025](0025-agent-facing-cli-contract.md)), and the first baseline found it about eleven times dearer than git, not five times cheaper: 31,915 bytes and 9,895 `o200k` tokens against git's 2,893 and 989 on macOS (`docs/token-benchmark.md`).

Two things made it large. `ws list` had no changed-file count, so the answer took a second call per workspace. And every object carried every field: a workspace's path, lease, snapshot operation and three ids are about 1,000 bytes, which the question never reads. Leaving out only null and false values was measured at 2,968 bytes, no better than git; the paths alone are more than Linux's whole budget of 168 `o200k` tokens. A view has to drop fields.

`-o text` was already 1.8 times cheaper than git, but an agent does not ask for it, and text is not JSON. The user decided the gate is measured on the default.

## Decision

1. **Three output forms: `text`, `json` and `brief`.** `brief` is the default when stdout is not a terminal; `text` stays the default on one. `-o json` and `REEF_OUTPUT=json` print exactly the JSON of before, plus additive keys. `REEF_OUTPUT` and `-o` keep their precedence. No harness is detected: the rule applies to every pipe.
2. **The brief layout.** Compact JSON, ending in a newline. A top-level array prints as `[`, one compact element per line, `]`, and `[]` when empty, so it still parses as one array. Errors are the same object, compact. Commands without a view print their full JSON compactly. JSON Lines (`op watch`, `presence watch`) are unchanged. The text-first commands (`config`, `guide`, `ws run`, `init`, `file read`) keep their text default; `-o brief` gives their JSON, compact.
3. **Three rules for every view**, in `reef_client::brief`, each view a struct borrowing the full type:
   - *Subset.* Every key is where the full document has it, with the same value. Keys are only left out, never renamed; values are never shortened.
   - *Zero values are left out*: `null`, `false`, `0`, `""` and `[]`. A reader takes a missing key as its zero value. A view keeps some keys whatever their value, a count that answers a question among them.
   - *Dropped keys* are left out whatever their value, and each view lists them.
4. **Three views.**
   - *Workspace* (`ws list`, and inside `status`): `id`, `owner`, `changed_file_count` when counted (even 0), `state` when not `active`, `divergent` and `attention` when set, and `repo_id` only in a list that spans more than one repository. Everything else is dropped; `ws inspect <id>` has it.
   - *Status*: `repository` and `at_operation` are dropped, and so are the repository's `active_workspaces` (`ws list` has them), `heads` and `last_operation`, and the workspace's `snapshot`. Both repository counts and the workspace's `changed_file_count` always show. Changes keep `change_id_reverse_hex`, the description and set flags, and `commit_id` only when divergent. Changed files keep `path` and `kind`, `old_path` when set, and a side's type when it is not `file` (a missing type is `file` when that side exists).
   - *Diff*: lossless. Only `at_operation` is dropped; files leave out zero flags and counts, empty hunks, and `file` types under the rule above.
5. **The full JSON is the interface for programs; brief is for reading.** Full JSON keeps [0034](0034-cli-over-the-vcs-services.md)'s rules and only grows. The three rules above are the brief form's contract; which keys a view keeps may change in a minor release. A brief object may carry `change_id_reverse_hex` without `change_id`, amending 0034 decision 4.
6. **`ws list` counts changed files (API revision 10).** `ListWorkspacesRequest.changed_file_counts` asks for `Workspace.changed_file_count`: what each active or releasing workspace's `@` changes against its parents, merged, as `GetStatus` counts it, as of its last snapshot. It is `optional`, since 0 is an answer, and opt-in, since it costs one summary diff per workspace, run per repository on its read thread at its head. It is best effort: a repository whose job fails leaves its counts unset and the list stands. The flag is not part of the page token's binding. `reef ws list` and the MCP `list_workspaces` tool always ask; the text table gains a `FILES` column, `-` when unset.
7. **MCP returns the same views** from `list_workspaces`, `get_status` and `get_diff`, with `next_page_token` left out on a last page, amending [0035](0035-mcp-server.md) decision 5 for those three tools. `get_workspace`, `list_changes` and `list_operations` still return full objects, so every dropped field is one call away.
8. **The benchmark guards the gate.** R2.17's scenario gains one-call `state-repo` candidates (`reef ws list`, with `-o json` and `-o text`, and `list_workspaces`), and `just tokens-check` asserts that the cheapest default `reef` sequence is at most a fifth of git's in bytes and `o200k`, and that `ws list`'s counts match git's.
9. **One release.** The default changes in 0.3.0 with the form itself, recorded in the CHANGELOG as a change that breaks scripts reading piped `reef ws list`, `status` or `diff` without `-o json`.

## Consequences

- On R2.17's fixture (macOS), `state-repo` is one call: `reef ws list`, 215 bytes, 118 Anthropic and 87 `o200k` tokens, against git's 2,893, 1,691 and 989: git costs 13.5, 14.3 and 11.4 times as much. `state-own` (`reef status`) falls from 15,915 bytes to 861, still 4.7 times git's porcelain; that question is not gated.
- The live half of the criterion is not met by any output change. With no project instructions, a live agent first reads `reef --help`, about 3 KB and more than twice the live budget. That is discoverability, filed as its own issue.
- A script that reads piped `reef ws list`, `status` or `diff` and wants a dropped key breaks until it passes `-o json` or sets `REEF_OUTPUT=json`. Nothing in this repository does.
- `ws list` now does repository work: one summary diff per active workspace. Its counts lag edits the watcher has not yet snapshotted.
- A brief workspace has no path or change id; the workspace id reaches both (`ws inspect`, `--workspace`).
- Objects are one line, so grepping piped output by line finds less; `jq` or `-o json` is the tool.
- The `MCP + fixed cost` arm stays unmet: the tool list itself is about 20 KB (#116).
