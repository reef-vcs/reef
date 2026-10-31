# Issue #114 Brief JSON by default

Phase 2 · Size M (no row; see "Size") · Fixes: issue #114 (the scripted half; see "What this ticket will not achieve") · Depends on: R2.17 (the benchmark, its scenario and verdict rules), R2.27 (the commands and their JSON), R2.13 (the MCP server and its `tools/list` budget) · Unblocks: the scripted half of Phase 2's exit criterion on repository state

## Context

R2.17 evaluates Phase 2's exit criterion on `state-repo`: "which other agents have work not on `main` yet, and how many files has each changed?" The gated Reef answer is the output an agent gets unasked, which on a pipe is JSON. At the first baseline that JSON was about eleven times dearer than git, not five times cheaper:

| Arm, cheapest sufficient sequence | bytes | anthropic | o200k |
| --- | ---: | ---: | ---: |
| git: `worktree list`, then `status --short -uall` per worktree (macOS) | 2,893 | 1,673 | 989 |
| git, same, Linux CI (PR #121, at 505bfab) | 2,593 | 1,505 | 842 |
| reef default: `ws list`, then `diff -r <change> --summary` per workspace | 31,915 | 13,550 | 9,895 |
| reef `-o text`: `ws list`, then `status --max-files 1` per workspace | 1,590 | 899 | 659 |
| mcp: `list_workspaces`, then `get_diff` summary per workspace | 11,436 | 5,666 | 3,858 |

The target is at most a fifth of git under every counter: on macOS 578 bytes, 334 anthropic and 197 o200k tokens; on Linux, where CI publishes, 518, 301 and 168. Text does not count (decided by the user): agents get JSON unless they ask.

This ticket makes the default JSON small:

- **Brief JSON when piped.** A third output form, `brief`, becomes the default when stdout is not a terminal: compact JSON, top-level arrays one element per line, and for `ws list`, `status` and `diff` a view that leaves out fields an agent does not act on. A view only leaves keys out; it never renames a key or shortens a value, so a brief document is a subset of the full one. `-o json` and `REEF_OUTPUT=json` print the full JSON exactly as today.
- **`ws list` counts changed files.** `ListWorkspacesRequest` gains an opt-in `changed_file_counts`, and `Workspace` gains `changed_file_count` (API revision 10). `reef ws list` and the MCP `list_workspaces` tool ask for it, so one call answers `state-repo`.
- **MCP returns the same views** from `list_workspaces`, `get_status` and `get_diff`.
- **The benchmark gains the one-call candidates**, so the cheapest sequence can be measured.

### What this ticket will not achieve

Said plainly, with numbers, so nobody reads the scripted result as the whole verdict:

- **The live half of the exit criterion stays unmet.** The live budget is a fifth of git's median answer tokens: 448 on macOS (git 2,242), 383 on Linux (git 1,914). R2.17 gives no arm project instructions, so every live `reef` trial at the baseline started with `reef --help`. That one call is 3,160 bytes and 725 o200k tokens; across the scripted rows Anthropic's count is 1.36 to 1.69 times o200k, so it is roughly 1,000 to 1,200 Anthropic tokens. That is already more than twice the budget before the agent asks anything. `reef guide` alone is 1,582 bytes and 438 o200k (about 600 to 740 Anthropic). After this ticket the best live path is `reef --help` and then `reef ws list`, about 1,150 to 1,350 tokens. That is roughly 1.7× cheaper than git on macOS and 1.5× on Linux, not 5×. No change to output size closes the gap. R2.17 already names the follow-up for this case: discoverability. See decision 1.
- **`mcp + fixed` stays unmet.** `list_workspaces` alone becomes about 231 bytes and 83 o200k tokens, but the server's fixed cost is 19,938 bytes, 7,550 Anthropic and 4,462 o200k tokens. That is #116.

### What was verified while writing this ticket

- **Output modes.** `Out::detect` (`crates/reef-vcs/src/cli/out.rs:52-61`) picks text on a terminal and JSON otherwise. `json_into` (`:282-288`) pretty-prints every JSON result. `-o` is a global clap arg with `env = REEF_OUTPUT` (`cli/mod.rs:102-112`). `output_mode` and `output_flag` (`:614-650`) read it for usage errors before clap parses. `config`, `guide`, `ws run`, `init` and `file read` default to text even on a pipe (`:428-438`).
- **Where the bytes go.** In the saved baseline outputs (R2.17's `results/scripted/`), one `ws list` element is 1,000 to 1,100 bytes pretty-printed. Of that, `path` is 130 bytes and 61 o200k tokens as compact JSON, `lease` about 330 bytes, `last_snapshot_op` 128 hex characters, and `change_id`, `change_id_reverse_hex` and `commit_id` 32, 32 and 40. The three paths alone are 183 o200k tokens, more than Linux's whole budget of 168. Pretty-printing costs about 30% of the tokens on top of compact JSON: the same three objects are 199 o200k pretty against 151 compact.
- **The JSON types.** `reef_client::json::Workspace` (`crates/reef-client/src/json.rs:270-316`), `Change` (`:957-994`), `FileChange` (`:1226-1237`), `FileDiff` (`:1269-1296`), `Diff` (`:1532-1545`) and `RepoStatus` with its two parts (`:1594-1649`). Every key is always present, with `null` for unset (`:1-5`, test `workspace_json_has_stable_names_and_nulls` at `:1965`). `DiffText` serializes as its `Diff` (`cli/render_vcs.rs:226-237`).
- **The commands.** `ws list` builds `Vec<Workspace>` and calls `out.list` (`cli/ws.rs:202-232`). Its text row is `render.rs:104-127`. `status` calls `out.object(&RepoStatus)` (`cli/status.rs:44`). `diff` calls `out.object(&DiffText)` (`cli/diff.rs:117-121`).
- **No count exists today.** `Workspace` (`workspaces.proto:223-266`, last field 19) has no changed-file count. `DiskUsage.files_changed` counts files that differ from the base, which is a different thing. `ListWorkspacesRequest` (`:386-400`, last field 5) reads only the database (`service/workspaces.rs:157-203`, `listed` at `:49-80`). `GetStatus` counts a workspace's changed files with `diff::diff_page(repo, &DiffBase::Parents, &wc.commit_id, &DiffOptions { format: Summary, limit, .. })`, whose `total` counts every file whatever the limit (`service/status.rs:162-193`). `API_REVISION` is 9 (`reef-api/src/lib.rs:49`).
- **Page tokens bind the request.** `PageRequest::new` keys the token on the request with `page_size` and `page_token` zeroed (`service/workspaces.rs:163-173`). Any new field is part of that key unless it is zeroed too.
- **MCP.** `list_workspaces`, `get_status` and `get_diff` return `reef_client::json` types (`mcp/tools.rs:545-571`, `:587-600`, `:645-680`, wrappers in `mcp/results.rs`). `CallToolResult::structured` sends the value and one compact text block (`mcp/mod.rs:397`). `tools/list` is held to 21,250 bytes (`tests/mcp.rs:190`) and is 21,191 now (R2.17's fixed-cost row), so 59 bytes are free.
- **The guide.** `docs/agent-guide.md` is 1,596 bytes against a 1,600 budget (`cli/guide.rs:91-101`). Its short form says "Output is JSON when piped", which stays true. `just agent-files` checks that the committed `AGENTS.md`, `CLAUDE.md` and skill match what `reef init` writes (`justfile:18-21`).
- **The benchmark.** A step's counted text is its command line plus its output (`reef-testing/src/tokens/scripted.rs:36-38`). A candidate is sufficient when its output names every agent in the truth (`:188-195`), unless the scenario marks it insufficient. `reef (default)` is the cheapest sufficient candidate without `-o text`. The `state-repo` candidates are in `crates/reef-testing/tokens/scenario.toml`. `tests/tokens.rs:312-341` checks that every arm has a sufficient candidate, in bytes. The fixture's own `reef` helper passes `-o json` (`tokens/fixture.rs:519`), as does the onboarding harness (`onboarding/run.rs:461`). Nothing in `scripts/`, `install.sh`, `packaging/` or the workflows parses default piped JSON.
- **Tests and the default.** The CLI test helpers clear `REEF_OUTPUT` (`tests/cli.rs:36-53`) and parse piped stdout as JSON (`tests/cli.rs:80`, `tests/vcs.rs:134-141`). With brief JSON as the default, only assertions on keys a view leaves out need `-o json`.
- **Tokenizer check.** Counting o200k with `tiktoken`'s `o200k_base` on the saved baseline outputs reproduces the published counts exactly (git 989, reef text 659, reef default 9,895), so the o200k estimates below are counts of the reconstructed text, not guesses. Anthropic's counts are estimated at 1.45 to 1.70 times o200k, which is the range for JSON with hex ids in the baseline rows.
- **ADRs.** The index ends at 0044 and no open PR adds one, so the next is 0045. The open PRs are #121 (benchmark results) and dependabot bumps. None touches the proto, so this is API revision 10.

## Decisions

### Size

M, with no `dev-phases.md` row. The work:

- two proto fields and one read job in the daemon;
- one client module of brief types;
- an output mode in the CLI, used by three commands, and a text column;
- three MCP tools moved to the brief types;
- four scenario candidates and a gate check in the benchmark's test;
- about 20 tests, ADR 0045, and doc edits.

### The numbers, and why this design

The `state-repo` answer must fit about 500 bytes and 160 o200k tokens, including the command line. That rules out a second call per workspace, and it rules out paths. One call has to carry who and how many.

The chosen candidate is `reef ws list` with the brief view, on the fixture's real ids:

```
reef ws list                                                          13 bytes
[                                                                      2
{"id":"4c642036b01117a3","owner":"bot-0","changed_file_count":3},     66
{"id":"74ed732ac9d9f374","owner":"bot-5","changed_file_count":59},    67
{"id":"32b014a821fcd2d7","owner":"bot-4","changed_file_count":4}      65
]                                                                      2
                                                                     ---
                                                                     215
```

Per element: `"id":"…"` is 23 bytes and 12 o200k tokens, `"owner":"bot-0"` 15 and 7, `"changed_file_count":3` 22 and 6, plus braces and commas.

| Counter | git, macOS | reef `ws list` brief | git ÷ reef | git, Linux | git ÷ reef |
| --- | ---: | ---: | ---: | ---: | ---: |
| bytes | 2,893 | 215 | 13.5× | 2,593 | 12.1× |
| o200k (counted) | 989 | 82 | 12.1× | 842 | 10.3× |
| anthropic (estimated, 1.45 to 1.70 × o200k) | 1,673 | 119 to 139 | 12.0× to 14.1× | 1,505 | 10.8× to 12.6× |

Ids are random per fixture build, so 300 draws of random workspace ids were counted: o200k median 82, maximum 90. The worst case is still 9.4× on Linux. This clears 5× under every counter on both platforms, with room for the views to keep more fields later.

What the other sequences cost once this lands, estimated the same way from the baseline outputs. They are not gated and are listed only so the new rows can be checked:

| Candidate | today bytes / o200k | brief bytes / o200k |
| --- | ---: | ---: |
| `state-repo` list, diff summary | 31,915 / 9,895 | 4,456 / 1,298 |
| `state-repo` list, status max 1 | 35,031 / 12,771 | 1,844 / 570 |
| `state-repo` list text, new FILES column (`-o text`) | – | 712 / 312 |
| `state-repo` mcp list (`list_workspaces` alone) | – | 231 / 83, plus 19,938 / 4,462 fixed |
| `state-own` status | 15,915 / 5,653 | 878 / 252 (git: 185 / 54) |

Note that the text form of `ws list` is larger than the brief JSON, because it prints paths.

The alternatives, with their numbers:

| Design | `state-repo` bytes / o200k (macOS) | git ÷ | Verdict |
| --- | ---: | ---: | --- |
| Count in `ws list`, full JSON as today (pretty) | 3,872 / 1,613 | 0.75× / 0.61× | no |
| Count, full JSON compact | 3,220 / 1,334 | 0.90× / 0.74× | no |
| Count, compact, null/false/0/empty fields left out | 2,968 / 1,265 | 0.97× / 0.78× | no |
| Text as agents' default (`ws list` with FILES) | 712 / 312 | 4.1× / 3.2× | no, and text is not JSON |
| Brief view keeping `change_id_reverse_hex` | 392 / 152 (median; max 166) | 7.4× / 6.5× | passes on macOS; 5.5× on Linux at the median, 5.07× at the worst draw, too thin |
| Brief view keeping `path` | 680 / 296 | 4.3× / 3.3× | no |
| **Brief view: id, owner, count** | **215 / 82** | **13.5× / 12.1×** | **chosen** |

Leaving defaults out is not enough. A view has to drop fields, and the path is the biggest of them.

### Output forms

- **Three forms: `text`, `json`, `brief`.** `Output` gains `Brief`, and clap's values are `text`, `json` and `brief` (`toml` stays an alias of `text`). `output_flag`'s raw parser accepts `brief` in every spelling.
- **The default on a pipe is `brief`.** On a terminal it is still `text`. `REEF_OUTPUT` still sets the default, and `-o` still beats it. `REEF_OUTPUT=json` restores today's behaviour for every command.
- **No harness detection.** The brief form applies to every pipe, whoever reads it, so nothing needs to know which agent is running. The env-signal table the north star allows is not needed.
- **`json` is unchanged**: pretty, every field, `null` for unset. It gains only the additive `changed_file_count` (below).
- **`brief` layout.** Compact JSON (`serde_json::to_writer`). A top-level array prints as `[`, then one compact element per line with `,` line ends, then `]`. An empty array is `[]`. Every result ends with a newline. An error object is the same object, compact.
- **Commands without a view** print their full JSON compactly under `brief`. That covers `here`, `ws create`, `inspect`, `log`, `show`, `history`, `op`, `presence` and the rest. Their views are a follow-up (see "Out of scope").
- **Text-first commands.** `config`, `guide`, `ws run`, `init` and `file read` treat `-o brief` like `-o json`, with the compact layout. Their default stays text.
- **JSON Lines** (`op watch`, `presence watch`) are already compact, one object per line, and do not change.

### The brief rules

A view is a typed serde struct built from the full `reef_client::json` type, so the two cannot drift. Three rules hold for every view:

1. **Subset.** Every key in a brief document is at the same place in the full document, with the same value. Keys are only left out, never renamed. Values are never shortened.
2. **Zero values are left out** inside a view: `null`, `false`, `0`, `""` and `[]`. A reader treats a missing key as its zero value. A view names the keys it always keeps, such as counts that answer a question, because there 0 must show.
3. **Dropped fields** are listed per view. They are left out whatever their value.

**Workspace** (`ws list`, and inside `status`):

- always: `id`, `owner`;
- when set, even 0: `changed_file_count`;
- when not zero: `divergent`, `attention`;
- `state` only when it is not `active`;
- `repo_id` only in a list that spans more than one repository (`ws list` without `--repo` lists every repository's; MCP's `all_repositories`), and never inside `status`;
- dropped: `purpose`, `change_id`, `change_id_reverse_hex`, `commit_id`, `backend`, `path`, `created_at`, `last_snapshot_at`, `last_snapshot_op`, `lease`, `watcher`, `watch_state`, `local_snapshot_count`, `stores`.

The workspace id is the handle for everything that follows: `reef diff --workspace <id>`, `reef status --workspace <id>`, `reef ws inspect <id>` for the path and lease, `reef ws release <id>`. `watch_state` is dropped because it is transient. A Linux workspace whose watch is still attaching would otherwise add about 6 tokens to the gate.

**Change** (`status`'s working copy, parents and trunk):

- always: `change_id_reverse_hex`;
- when not zero: `description` (verbatim), `empty`, `conflict`, `divergent`, `immutable`, `hidden`, `bookmarks`, `tags`, `workspace_ids`;
- `commit_id` only when `divergent` is true, since then the change id does not name one commit;
- dropped: `change_id`, `parent_commit_ids`, `parent_change_ids`, `author`, `committer`, `tree_id`, `jj_workspaces`.

**FileChange** (`status`'s changed files, and the base of `FileDiff`):

- always: `path`, `kind`;
- when set: `old_path`;
- `old_type` and `new_type` when they are neither `null` nor `file`. A missing type means `file` when that side exists, and no side when it does not (`added` has no old side, `deleted` no new side).

**FileDiff**: FileChange's brief, plus `binary`, `conflict`, `truncated`, `old_no_final_newline`, `new_no_final_newline`, `too_large` and `invalid_utf8` when true, `lines_added` and `lines_removed` when not 0, `hunks` when not empty (each hunk whole), and `symbols` when present (whole). Nothing is dropped. The diff view is lossless under rule 2 and the type rule.

**Diff** (`reef diff`, every format): `from_commit_ids`, `to_commit_id`, `total_files` (always), `files`, and `symbol_summary` when present. Dropped: `at_operation`.

**RepoStatus** (`reef status`):

- dropped at the top: `repository`, `at_operation`;
- `workspace_status` when set:
  - `workspace` (brief);
  - `working_copy` and `parents` (brief changes);
  - `changed_files` (brief);
  - `changed_file_count` (always);
  - `conflicted_paths` and `conflicted_path_count` when not zero;
  - `source` (always: it says whether the answer is the last snapshot's);
  - dropped: `snapshot`;
- `repository_status`:
  - `trunk` (brief change) and `trunk_bookmark` when set;
  - `active_workspace_count` and `head_count` (always);
  - `conflicted_change_ids` and `divergent_change_ids` when not empty;
  - dropped: `active_workspaces` (`reef ws list` has them, with counts), `heads` (`reef log -r 'heads(mutable())'`), `last_operation` (`reef op log --limit 1`).

### Changed-file counts: API revision 10, additive

```proto
// in ListWorkspacesRequest
  // Fill each active or releasing workspace's `changed_file_count`. Costs
  // one tree diff per such workspace on its repository's read thread; no
  // snapshot is taken. Not part of the page token's binding.
  bool changed_file_counts = 6;

// in Workspace
  // How many paths its working-copy commit changes against its parents,
  // merged, as GetStatus counts them, as of the last snapshot. Set only by
  // ListWorkspaces with `changed_file_counts`, for active and releasing
  // workspaces; unset otherwise, and when the count could not be read.
  optional uint32 changed_file_count = 20;
```

- **`optional`**, because 0 ("nothing changed") is meaningful and differs from "not counted" (ADR 0029 decision 9). **Opt-in**, because it is a heavy part of the response (decision 11).
- **The daemon.** `list_workspaces` groups the page's active and releasing rows by repository. For each repository it runs one job on the read thread, as `GetStatus` does (`service/status.rs:115-205`). The job resolves `@` in each workspace's scope at the head operation and takes `diff::diff_page(repo, &DiffBase::Parents, &commit, &DiffOptions { format: Summary, limit: 1, .. }).total`. That is the working-copy commit `reef status --no-snapshot` reports, not the record's `commit_id`.
- **Best effort.** A repository whose job fails leaves its counts unset, logs at `warn` with the repository id, and never fails the call. `ws list` is a recovery tool.
- **The page token** ignores the flag: zero it in the key passed to `PageRequest::new`, beside `page_size` and `page_token`.
- **Full JSON.** `reef_client::json::Workspace` gains `changed_file_count: Option<u32>` as its last key: a number, or `null`. Every full workspace object gains the key. Only `ws list` and `list_workspaces` fill it.
- **An older daemon** ignores the flag. The CLI then shows no count: the key is missing from brief and `null` in full JSON, and the text column shows `-`.

### `ws list`

- **It always asks for counts**, in every output form.
- **Text** gains a `FILES` column after `CHANGE`: the count, or `-` when unset.
- **`about`** becomes "List active workspaces, newest first, with how many files each changed."
- **JSON** stays one array, as R1.22 and ADR 0025 fixed.

### MCP

- **`list_workspaces`** sends `changed_file_counts: true` and returns `{workspaces: [brief], next_page_token}`, with the token left out on the last page.
- **`get_status`** returns the brief `RepoStatus`.
- **`get_diff`** returns the brief `Diff` flattened beside `next_page_token`, left out when unset.
- **Everything else is unchanged.** `get_workspace` still returns the full workspace (path, lease, recent snapshots), and so do writes that return a workspace. MCP has no `-o`, so `get_workspace`, `list_changes` and `list_operations` are where the dropped fields are.
- **Descriptions.** `list_workspaces` becomes "List a repository's workspaces, newest first: each is a directory leased to one agent, editing one change, with how many files it changed. Returns {workspaces, next_page_token}." (+32 bytes). `get_status`'s "Returns {repository, repository_status, workspace_status}." becomes "Returns {repository_status, workspace_status}." (−12). `tools/list` grows by 20 bytes, to about 21,211 of 21,250. The instructions do not change.
- **This amends ADR 0035 decision 5** ("results are the objects `reef -o json` prints") for these three tools.

### Compatibility and versioning

- **What breaks.** A program that reads piped `reef ws list`, `reef status` or `reef diff` without `-o json` and wants a key the brief leaves out. Also anything that greps piped JSON line by line: objects are now one line, though list elements are one per line. Nothing in this repository does either outside the tests named below.
- **The opt-out** is `-o json` per command, or `REEF_OUTPUT=json` for everything. Both give today's output byte for byte, plus the one additive key.
- **The contract.** The full JSON (`-o json`) stays the stable interface for programs, under R1.22's and ADR 0034's rules: keys are only ever added. The brief form is for reading. Its three rules are the contract, while which keys a view keeps may grow or shrink in a minor release. ADR 0045 says this, and the guide says `-o json` has every field.
- **Versioned and documented.** The CHANGELOG has a "Changed" entry that says this breaks scripts reading piped output without `-o json` and names the opt-out. The next release is 0.3.0, not 0.2.1. ADR 0045 records the decision and amends ADRs 0025 (the default form), 0034 decision 4 (a brief object can carry `change_id_reverse_hex` without `change_id`, and leaves nulls out) and 0035 decision 5.
- **The proto is additive** (revision 10, `buf breaking` clean). Old clients ignore the new field, and old daemons ignore the new flag.

### The benchmark

`crates/reef-testing/tokens/scenario.toml` gains these candidates under `state-repo`, after the existing ones, which stay as they are:

```toml
[[question.candidate]]
arm = "reef"
name = "list"
steps = ["reef ws list"]
[[question.candidate]]
arm = "reef"
name = "list full"
steps = ["reef ws list -o json"]
[[question.candidate]]
arm = "reef"
name = "list text"
steps = ["reef ws list -o text"]
[[question.candidate]]
arm = "mcp"
name = "list"
calls = [{ tool = "list_workspaces", args = {} }]
```

- **A new baseline.** Changing the scenario changes the fixture version (`421f33d6ff31` today). Rows after this ticket start a new baseline, which the Notes column says.
- **The gate guard.** `tests/tokens.rs`'s `fixture_and_truth` (run by `just tokens-check`) asserts that each workspace's `changed_file_count` from `reef ws list -o json` matches git's per-worktree count. With the `tokenizers` feature, which `tokens-check` turns on, it also asserts that `state-repo`'s cheapest `reef (default)` candidate is at most a fifth of git's in bytes and in o200k. A later change that bloats the view then fails a test, not just a dispatch.
- **No harness or method change.** Prompts, grading, arms and the live setup are R2.17's.

**Re-measuring.** Locally, from a full clone; a Reef workspace has no git history, so pass `--source` with a clone there:

```sh
just tokens scripted question=state-repo,state-own --no-sweep   # free; ANTHROPIC_API_KEY adds anthropic
cat crates/reef-testing/tokens/results/latest.md
just tokens-check                                               # truth, sufficiency, and the 5x guard
ANTHROPIC_API_KEY=... just tokens live question=state-repo arm=reef trials=3   # optional, about $0.10
```

After merge, dispatch a full run, since every JSON shape an agent reads has changed:

```sh
gh workflow run tokens.yml -f trials=3 \
  -f note="#114: brief JSON by default, ws list counts changed files (new baseline: scenario changed)"
```

That run's results PR, not this one, records the verdict. It updates `dev-phases.md`'s exit-criterion bullet and `token-benchmark.md`'s verdict paragraph: expected "scripted met (about 12×, 10×, 11×); live not met (about 1.5×), discoverability".

### Text

- **`-o`'s help**: "Output format: `brief` JSON when stdout is not a terminal, `text` when it is; `json` has every field". `reef --help` must stay within 60 lines.
- **`docs/agent-guide.md`, long form only.** The short form, and so `AGENTS.md`'s section, does not change. Within the 1,600-byte budget (this draft is 1,585):

  ```
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
  ```

  Regenerate the committed skill with `cargo run -p reef-vcs -- init --agents claude --no-register` so `just agent-files` passes.
- **`out.rs` module doc**: the three forms, the brief rules, and that views live in `reef_client::brief`.

## Deliverables

### `crates/reef-api/`

- The two fields above in `workspaces.proto`.
- `API_REVISION` 10, and a new last entry in `API_DIGESTS`.

### `crates/reef-daemon/`

- `service/workspaces.rs`: the counting job in `list_workspaces`, and the flag zeroed in the page key.

### `crates/reef-client/`

- `json.rs`: `Workspace.changed_file_count`.
- A new `brief.rs` (`reef_client::brief`): `Workspace`, `Change`, `FileChange`, `FileDiff`, `Diff`, `RepoStatus`, `RepositoryStatus` and `WorkspaceStatus` as serde structs borrowing from the full types, with `skip_serializing_if` helpers (`is_false`, `is_zero`, `is_file_type`). Also `workspaces(&[json::Workspace]) -> Vec<brief::Workspace>`, which applies the `repo_id` rule.

### `crates/reef-vcs/`

- **`cli/out.rs`**:
  - `Output::Brief`;
  - `Out::detect` defaults to it on a pipe;
  - `object`, `list`, `done`, `json_only` and errors write compact JSON under it;
  - `object_with(&full, |full| brief)` and `list_with(&full, |full| briefs)` print the view under `Brief`, and the full value otherwise.
- **`cli/mod.rs`**: the `-o` help text and `output_flag`'s `brief` spelling.
- **`cli/ws.rs`**: `changed_file_counts: true`; `list_with`; the `about`.
- **`cli/render.rs`**: the `FILES` column.
- **`cli/status.rs`, `cli/diff.rs`**: `object_with`.
- **`mcp/tools.rs`, `mcp/results.rs`**: the brief wrappers, the flag, and the two descriptions.

### `crates/reef-testing/`

- `tokens/scenario.toml`: the four candidates.
- `tests/tokens.rs`: the count and gate checks.

### Documentation

- **`docs/adr/0045-brief-json-by-default.md`** (accepted, ticket issue #114), indexed in `docs/adr/README.md`. It covers:
  - the three forms and the default;
  - the three brief rules and the three views;
  - the contract split: full JSON stable, brief for reading;
  - MCP's three tools;
  - the count (API revision 10);
  - no harness detection;
  - the numbers above.

  Add `- Amended by: [0045](0045-brief-json-by-default.md)` to ADRs 0025, 0034 and 0035.
- **`docs/agent-guide.md`** as above, and the regenerated skill.
- **`docs/token-benchmark.md`**:
  - "The arms" and "What is measured" say `reef (default)` is what a shell gets on a pipe, brief JSON (ADR 0045);
  - the `state-repo` row of the questions table notes the one-call candidate;
  - the verdict paragraph is left to the results PR.
- **`README.md:88`**: "Output is brief JSON when stdout is not a terminal (`-o json` for every field)".
- **`docs/quickstart.md:116`**: "Each command prints brief JSON when piped (`-o json` for every field)".
- **`docs/design.md:293`**: `Emits brief JSON by default when stdout is not a terminal ([ADR 0045](adr/0045-brief-json-by-default.md)).`
- **`CHANGELOG.md`**, under Unreleased:
  - **Added**: `ListWorkspaces` counts with `changed_file_counts` (API revision 10); `reef ws list`'s `changed_file_count` and `FILES` column; `-o brief`.
  - **Changed**: piped output is brief JSON (ADR 0045), "breaks scripts that read piped `reef ws list`, `status` or `diff` without `-o json`; `-o json` or `REEF_OUTPUT=json` restores the full JSON"; the MCP `list_workspaces`, `get_status` and `get_diff` results are brief.
- **`docs/dev-tickets/phase-2/README.md`**: this brief's row.
- **Unchanged**: `docs/dev-phases.md` (no row; the results PR updates the exit-criterion bullet), R2.17's brief, the short guide and `AGENTS.md`'s section.

## Tests

### `crates/reef-client/src/brief.rs`

1. **`brief_workspace_keeps_what_an_agent_acts_on`**:
   - an active workspace with a lease, a path, a purpose, `watch_state` `attaching` and a count of 3 serializes to exactly `{"id":…,"owner":…,"changed_file_count":3}`;
   - with `state` `releasing`, `divergent` true and `attention` `divergent`, those three keys appear;
   - with a count of 0 the key is there with 0;
   - unset, the key is missing.
2. **`brief_lists_name_the_repository_only_across_several`**: `workspaces()` over two workspaces of one repository has no `repo_id`. Over two repositories, every element has it.
3. **`brief_change_keeps_ids_description_and_set_flags`**:
   - a plain change is `{change_id_reverse_hex, description}`;
   - an empty description is left out;
   - `immutable` and `bookmarks` appear when set;
   - `commit_id` appears only when `divergent`.
4. **`brief_file_types_mean_file_when_missing`**:
   - modified file to file gives `{path, kind}`;
   - file to symlink keeps `new_type: "symlink"` only;
   - added gives no `old_type`, and deleted no `new_type`;
   - a rename keeps `old_path`.
5. **`brief_diff_is_lossless`**: for a `Diff` with one file whose every flag is false, one whose every flag is true or non-zero, hunks, a rename and a symlink, the test's `expand(brief)` (zero values plus the type rule) equals the full JSON minus `at_operation`.
6. **`brief_status_drops_the_repository_listing`**:
   - a `RepoStatus` with active workspaces, heads, a last operation, a snapshot and a repository has none of `repository`, `at_operation`, `active_workspaces`, `heads`, `last_operation` or `snapshot`;
   - it keeps both counts, `trunk`, `trunk_bookmark`, `source` and `changed_file_count`, the last even when 0;
   - in repository scope there is no `workspace_status` key.
7. **`brief_is_a_subset_of_full`**: for every sample in tests 1 to 6, every key path in the brief JSON exists in the full JSON with an equal value. A helper walks objects, and arrays by position.

### `crates/reef-client/src/json.rs`

8. **Updated `workspace_json_has_stable_names_and_nulls`**: `changed_file_count` is `null` when unset and a number when set, and is the last key.

### `crates/reef-vcs/src/cli/out.rs` and `cli/mod.rs`

9. **`brief_lists_one_compact_element_per_line`**: two elements give `"[\n{…},\n{…}\n]\n"`, none gives `"[]\n"`, and an object is one line ending in `\n`.
10. **Updated `output_flag_reads_every_spelling`**: `-o brief`, `--output brief`, `--output=brief` and `-obrief` all parse.

### `crates/reef-vcs/tests/cli.rs`

11. **`json_when_piped` becomes `brief_json_when_piped`**:
    - `reef ws list` with no `-o` parses as one array whose element has exactly the keys `id`, `owner` and `changed_file_count`, one element per line;
    - `-o json` has `path`, `lease` and `changed_file_count`, pretty;
    - `REEF_OUTPUT=json` matches `-o json`;
    - `-o brief` with `REEF_OUTPUT=json` is brief;
    - `repo list` stays as it was, compact.
12. **Updated `ws_list_and_inspect`**: its `state` assertion reads `-o json`. The text header is `ID STATE CHANGE FILES OWNER LEASE BACKEND ATTENTION PATH`.
13. **`ws_list_counts_changed_files`**:
    - a workspace with one tracked file modified, one added and one deleted, after `reef ws snapshot`, lists `changed_file_count` 3;
    - a fresh workspace lists 0, with the key present;
    - after a release, `ws list --all` has no count for it (brief: missing; `-o json`: `null`; text: `-`);
    - an unsnapshotted edit does not change the count until `ws snapshot`.
14. **`ws_list_brief_names_the_repository_when_several`**: two repositories with a workspace each give `repo_id` on both. `--repo` one of them gives no `repo_id`.
15. **Updated `reef_output_env_sets_the_default`**: `REEF_OUTPUT=brief` is accepted. `-o json` beats it.

### `crates/reef-vcs/tests/vcs.rs`

16. **`status_brief_and_full`**:
    - in a workspace with three changed files, `reef status` has `workspace_status.changed_file_count` 3, `changed_files` as `{path, kind}`, `parents[0].description`, and `repository_status.active_workspace_count`, and none of `heads`, `last_operation` or `at_operation`;
    - `reef status -o json` has all of them as before;
    - `reef status --repo <id>` has no `workspace_status` key.
17. **`diff_brief_matches_full`**: for `reef diff --summary` and `reef diff` on a change with a modified, an added and a renamed file, the brief expanded by test 5's rules equals `-o json` minus `at_operation`.

### `crates/reef-daemon/tests/`, beside the existing `ListWorkspaces` tests

18. **`list_workspaces_counts_changed_files`**:
    - with `changed_file_counts`, each active workspace's count equals `GetStatus { skip_snapshot: true }`'s `changed_file_count`;
    - without the flag, every count is unset;
    - a released workspace's count is unset with the flag;
    - a page token from a call with the flag works on a call without it.

### `crates/reef-vcs/tests/mcp.rs`

19. **`list_workspaces_is_brief_with_counts`**:
    - `list_workspaces` returns elements with exactly `id`, `owner` and `changed_file_count`, and no `next_page_token` key on a single page;
    - `get_workspace` on the same id still has `path` and `lease`;
    - `get_status` has no `heads`;
    - `get_diff` files have no `false` flags.

    The `tools/list` budget test (≤ 21,250 bytes) passes unchanged with the new descriptions.

### `crates/reef-api/`

20. **`api_digest_matches_revision`**, unchanged code, with revision 10 and its digest.

### `crates/reef-testing/`

21. **`scenario` unit tests** parse the four new candidates. `list text` is a text candidate, and `list full` is not.
22. **`fixture_and_truth`** (ignored, `just tokens-check`):
    - `changed_file_count` per agent from `reef ws list -o json` equals git's counts (bot-0 3, bot-4 4, bot-5 59);
    - `state-repo`'s cheapest sufficient `reef` candidate without `-o text`, times five, is at most git's cheapest, in bytes and, with `tokenizers`, in o200k.

Every other test passes, with `-o json` added only where an assertion reads a key a view leaves out. Grep the tests for `"ws", "list"`, `"status"` and `"diff"` with `json(`, `ok(` and `err(` to find them. Do not set `REEF_OUTPUT=json` in a shared helper: it would turn `file read`'s and `ws run`'s text defaults into JSON.

## Implementation notes

- **Order:**
  1. `reef_client::brief` with tests 1 to 7;
  2. `Output::Brief` and the `Out` methods with tests 9 and 10, then fix the CLI tests that read dropped keys;
  3. `status` and `diff` with tests 16 and 17;
  4. the proto fields, revision and daemon job with tests 18 and 20;
  5. `ws list` with tests 8 and 11 to 15;
  6. MCP with test 19;
  7. the scenario and test 22, measured with `just tokens scripted`;
  8. ADR 0045 and the documents.
- **Run `just tokens scripted question=state-repo` before writing the ADR's numbers**, and put measured numbers in it, not this brief's estimates. If the measured o200k for `list` on macOS is above 120, something the view should leave out is in it.
- **The counting job uses `limit: 1`.** `total` still counts every file. Do not read the paths.
- **Timing tests flake under load** on `main` (leases, snapshots, `ws_run`). Test 13 snapshots explicitly and never waits on the watcher.
- **`just tokens-check` and `just tokens` need a full clone.** In a Reef workspace, pass `--source` to `scripted`, or run them from a clone with the branch checked out.
- **`buf`** is not installed on the Mac. CI's `api` job checks the proto.
- **No `cfg(target_os = "linux")` code** is touched. The Linux CI job covers the rest.

## Acceptance criteria

- [ ] Piped output is brief JSON. `-o json` and `REEF_OUTPUT=json` print today's JSON plus `changed_file_count` (tests 11, 15).
- [ ] Every brief document is a subset of the full one, and the diff view is lossless (tests 5, 7, 17).
- [ ] `reef ws list` and `list_workspaces` report each active workspace's changed-file count as of its last snapshot. Text has a `FILES` column (tests 13, 18, 19).
- [ ] API revision 10 is additive and `buf breaking` clean (test 20).
- [ ] `tools/list` ≤ 21,250 bytes, and the guide ≤ 1,600 bytes with the long form above.
- [ ] `just tokens scripted` shows `state-repo`'s `reef (default)` at most a fifth of git's in bytes and o200k, and in Anthropic's count when a key is set. `just tokens-check` guards bytes and o200k (test 22).
- [ ] ADR 0045 is written and indexed, ADRs 0025, 0034 and 0035 are marked amended, and the CHANGELOG, README, quickstart, design, token-benchmark page, guide and skill are updated. `just ci` passes.
- [ ] After merge, `tokens.yml` is dispatched as above, and its results PR records the verdict.

## Verification

```sh
cargo nextest run -p reef-client brief json
cargo nextest run -p reef-vcs --lib out
cargo nextest run -p reef-vcs --test cli --test vcs --test mcp
cargo nextest run -p reef-daemon list_workspaces
cargo nextest run -p reef-api
just tokens scripted question=state-repo,state-own --no-sweep
just tokens-check
just agent-files
just ci
```

## Inherited scope

- **From R1.22 and ADR 0025:** one JSON value per command, one array for lists, errors with `hint` and `next`, `REEF_OUTPUT`. Brief keeps all of them; only the default form changes.
- **From R2.27 and ADR 0034:** the full JSON's rules, unchanged under `-o json`.
- **From R2.13 and ADR 0035:** tools return `reef_client` types, under the `tools/list` budget. Three tools now return the brief types.
- **From R2.17:** the scenario, counters, verdict rules and dispatch-only workflow. This ticket adds candidates and a guard, and changes no method.

## Out of scope

- **The live half of the exit criterion.** Discoverability, not output size: see "What this ticket will not achieve" and decision 1. The PR files an issue with those numbers.
- **Views for other commands** (`here`, `ws create`, `ws inspect`, `log`, `show`, `history`, `op log`, `presence`). They print compact full JSON under brief for now. The PR files one follow-up issue, measured on `state-own`, `who-file` and `who-symbol`.
- **A brief `--symbols` view.** R2.17 found the symbols JSON no smaller than the unified diff. Its `symbols` objects pass through whole here. Part of the follow-up above.
- **#116** (MCP fixed cost). This ticket adds 20 bytes to `tools/list`. It also moves #116's break-even: on `state-repo` the CLI default (215 bytes) becomes cheaper than MCP's `list_workspaces` (231 plus fixed), so MCP saves nothing there.
- **#115** (`history -o text` authors), **#117** (MCP trailing slash), **#118** (`mcp --help` names one harness), **#119** (a second harness). Nothing here touches them.

## Known limits

- **Counts lag unsnapshotted edits.** The watcher snapshots after writes, so the lag is usually seconds. `reef status --workspace <id>` snapshots first.
- **`ws list` now does repository work**: one summary diff per active workspace, behind that repository's read jobs. Record its time for 100 workspaces on R1.12's `medium` in the PR.
- **Brief objects are one line**, so grepping piped JSON by line finds less. `jq`, or `-o json`, is the tool.
- **A brief workspace has no path or change id.** The workspace id reaches both (`ws inspect`, `--workspace`).
- **The `state-own` brief (878 bytes) is still about 4.7× dearer than git's porcelain (185).** It is not gated, and it is 18× smaller than today.

## What later tickets inherit

- `reef_client::brief` and `Out::object_with` and `list_with`: a later view is one struct and one call site.
- `ListWorkspacesRequest.changed_file_counts` for the web UI (R2.19) and the SDKs.
- `tests/tokens.rs`'s 5× guard on `state-repo`.

## Departures and decisions

No departure from `dev-phases.md`; this issue has no row. The brief departs from three ADRs, and ADR 0045 records it: 0025 (the default form), 0034 decision 4 (brief objects leave nulls out and may carry `change_id_reverse_hex` without `change_id`), and 0035 decision 5 (three MCP tools return brief types).

1. **The live half cannot be met by this ticket.** With no project instructions, a live `reef` agent pays roughly 1,000 to 1,200 Anthropic tokens for `reef --help` against a 383 to 448 budget. Choose one:
   - (a) accept "scripted met, live not met", and file a discoverability issue;
   - (b) change R2.17's setup so each arm starts with what its tool installs. The `reef` arm would get `reef init`'s `AGENTS.md` section as session context, which is not counted as answer tokens. That reverses R2.17's "no arm gets `reef init`'s files" and starts a new baseline. The short guide would also need to name `reef ws list`;
   - (c) cut `reef --help` well below 1,000 bytes. Even then, help plus `ws list` is about 400 to 500 tokens, which is borderline.

   Decided (user, 2026-10-31): (a). This ticket does not touch the live setup.
2. **`purpose` in the brief workspace?** It is the creator's free text and says what each agent is doing. On the fixture it adds 72 bytes and about 27 o200k tokens: 287 bytes and about 109 o200k, which is 7.7× on Linux. Real purposes are longer and unbounded. Decided (user, 2026-10-31): no.
3. **`change_id_reverse_hex` in the brief workspace?** It makes `reef diff -r <change>` possible from the list alone. It costs 58 bytes and about 24 o200k per workspace: 392 bytes and a median of 152 o200k, which is 5.5× on Linux at the median and 5.07× at the worst of 300 id draws. Decided (user, 2026-10-31): no. `--workspace <id>` reaches the change.
4. **Flip the default in one release, or two?** Two would be 0.3.0 adding `-o brief` and 0.4.0 making it the default. Decided (user, 2026-10-31): one, in 0.3.0. 0.2.0 shipped two days before this brief, nothing in the repository parses default piped JSON, and the exit criterion is measured on the default.
5. **The name `brief`.** Alternatives are `compact`, which suggests whitespace only, and `agent`, which names a reader rather than a form. Decided (user, 2026-10-31): `brief`.
6. **Should `reef status` list the other workspaces with their counts, so it alone answers `state-repo`?** That costs up to 100 summary diffs on every status call. Decided: no. It carries `active_workspace_count`, and `ws list` has the rest.
