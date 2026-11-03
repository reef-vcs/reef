# Issue #126 Brief views keep `at_operation`

Phase 2 · Size S (no row; see "Size") · Fixes: issue #126 · Depends on: #114 (the brief views, ADR 0045), R2.12 (`--at-op` and `--at` on `status` and `ws list`, ADR 0046) · Unblocks: none listed

## Context

`reef status --at-op <op>` and `--at 1h` answer as of that operation. On a pipe the answer is the brief view (ADR 0045), and that view drops `at_operation`. In repository scope nothing else in it marks the read as historical. An agent reading it cannot tell which operation answered, or that the answer is not about now. Today the full id is only in `-o json`. With `--at`, stderr also gets `reef: at operation <12 hex> (<time>)`; `--at-op` prints no note.

This ticket keeps `at_operation` in the `status` and `diff` brief views when the request named an operation, and leaves it out of reads at head, as now. For the same reads, `status` and `diff` text gain a first line, `At operation <12 hex>`, with the operation's time when the response has it. It changes the CLI's views in `reef_client::brief`, the two text renderers, and how three call sites pick a view. There is no API, schema or full-JSON change, and every measured output is unchanged.

### What was verified while writing this ticket

All of this was checked at 179f5ca, with the installed `reef` 0.3.0 run in this repository. Paths are under `crates/` unless they start with `docs/`.

- **Which commands read at an operation.** `scope::AtArgs` (`reef-vcs/src/cli/scope.rs:32-77`) adds `--at-op`/`--at` to `status`, `ws list`, `ws inspect` and `op log` (R2.12). `scope::ReadArgs` (`:79-94`) adds them to `log`, `show`, `diff`, `file read`, `file list`, `symbols list` and `symbols index` (R2.27). `AtArgs::given()` (`:45`) says whether either flag was given. Only three commands have a brief view (ADR 0045 decision 4, `reef-vcs/src/cli/schema/outputs.rs:184`, `:229`, `:233`): `ws list`, `status` and `diff`. The rest print their full JSON compactly on a pipe.
- **Where `at_operation` shows today**, read at an operation:

  | Command | Brief view | Full JSON (`-o json`) | Piped default | Text |
  | --- | --- | --- | --- | --- |
  | `status` | yes | yes, top level, full id, at head too (`json::RepoStatus.at_operation`, `reef-client/src/json.rs:1894`) | **no** (dropped, `brief.rs:265`) | no |
  | `diff` | yes | yes, top level, at head too (`json::Diff.at_operation`, `json.rs:1834`) | **no** (dropped, `brief.rs:241`) | no |
  | `ws list` | yes (an array) | **no**: a top-level array of `Workspace` | no | no |
  | `ws inspect` | no | no: `GetWorkspaceResponse.at_operation` is not in `Inspected` | no | no |
  | `op log` | no | no: an array; `ListOperationsResponse.at_operation` is not printed | no | no |
  | `log`, `file list`, `symbols list` | no | no: arrays | no | no |
  | `show` | no | yes (`ChangeSummary.at_operation`, `json.rs:1864`) | yes | no |
  | `file read` | no | yes with `-o json`/`-o brief` (`FileRead.at_operation`, `json.rs:2160`) | text by default | no |

  The issue is right about `ws list`: `ListWorkspacesResponse.at_operation` (`reef-api/proto/reef/api/v1/workspaces.proto:446-447`) is dropped by `reef_client::Client::list_workspaces` (`reef-client/src/lib.rs:825-843`), which returns only `Vec<Workspace>`, so neither form can print it.
- **The issue is half wrong about `source`.** `brief::WorkspaceStatus` keeps `source` (`brief.rs:338`). In workspace scope the brief already ends `…"changed_file_count":0,"source":"operation"}}`. In repository scope there is no `workspace_status`, so nothing marks the read. The issue's point holds there, and the full id is missing in both scopes.
- **The response cannot tell a named read from a head read.** `GetStatusResponse.at_operation` is "the operation it was read at" (`status.proto:122-123`) and is set on every read. `RepositoryStatus.last_operation` is "the head operation, or `at_operation`" (`:150`). `GetDiffResponse` is the same. So the view needs the request's flag: in the CLI, `AtArgs::given()`; in MCP, the `at_operation` argument.
- **The call sites.** `status`: `ctx.out.object_with(&RepoStatus::from(&response), brief::RepoStatus::from)` (`reef-vcs/src/cli/status.rs:50-51`). `diff`: `object_with(&DiffText {…}, |text| brief::Diff::from(&text.diff))` (`diff.rs:112-119`). `ws list`: `ctx.out.list_with(&listed, brief::workspaces)` (`ws.rs:448`). MCP: `get_status` passes `at_operation: String::new()` and returns `brief::RepoStatus::from` (`reef-vcs/src/mcp/tools.rs:705-720`). `get_diff` passes the caller's `at_operation` and returns `DiffPage { diff: brief::Diff::from(&diff), .. }` with `diff` flattened (`tools.rs:765-800`, `mcp/results.rs:76-84`). `list_workspaces` and `get_workspace` take no `at_operation`. `list_changes`, `get_change` and `list_files` always return it.
- **The stderr note.** `AtArgs::at_operation` (`scope.rs:52-76`) prints `reef: at operation <12 hex> (<ended_at>)` for `--at` only, after a `GetOperation` by time. For `--at-op` it returns the argument as given, a prefix or a full id, and prints nothing.
- **Operation ids are 128 hex characters.** `,"at_operation":"<id>"` is 146 bytes.
- **The benchmark reads only at head.** No candidate in `reef-testing/tokens/scenario.toml` passes `--at-op` or `--at`. The guards in `reef-testing/tests/tokens.rs` parse `ws list` and `diff --summary` at head.
- **Existing tests that pin the head view.** `brief_status_drops_the_repository_listing` (`brief.rs:746`) asserts `at_operation` is absent. `brief_diff_is_lossless` (`:733`) expects the full diff without `at_operation`. `status_brief_and_full` (`reef-vcs/tests/vcs.rs:2433`) asserts piped `status` has no `"at_operation":`. `diff_brief_matches_full` (`:2482`) removes `at_operation` from the full diff before comparing. `replay_cli` (`:2511`) checks `--at-op`/`--at` through `-o json` only.

## Decisions

### Size

S, with no `dev-phases.md` row. The work is one optional field and one constructor on each of two views, one first line in each of two text renderers, three call sites, seven unit tests, one integration test, two extended tests, and four documentation edits.

### Which views gain the key

- **`status` (brief):** gains it, in both scopes.
- **`diff` (brief):** gains it. The issue names only the views R2.12 touched. `diff` took `--at-op` in R2.27 and has the same gap, and the rule belongs to the view, not to the ticket that added the flag.
- **MCP `get_diff`:** gains it when the caller passes a non-empty `at_operation`, because it returns the same view. Its description and `tools/list` do not change.
- **MCP `get_status`:** no change. It takes no `at_operation` (ADR 0046 decision 10), so `From` stays its view.
- **`ws list`, both forms:** no change. See "`ws list`" below and decision 1 at the end.
- **`ws inspect`, `op log`, `log`, `file list`, `symbols`:** no change. They have no brief view, and their full JSON has no `at_operation` to keep. Adding an echo to their full JSON is out of scope.
- **`show`, `file read`:** no change. They already print it.

### The key

- **Name and place:** `at_operation`, at the top level, where the full document has it (ADR 0045 rule 1). In `status` it is the first key, since `repository` before it is dropped. In `diff` it comes after `to_commit_id`. Serde writes fields in declaration order, so declare them in that order.
- **Value:** the response's full id, never shortened (rule 1: values are never shortened). With `--at-op 9e22f9e2762b` the key holds the full 128-character id, so the brief also resolves the prefix.
- **When:** only when the request named an operation: `--at-op`, `--at`, or MCP `get_diff`'s `at_operation`. A named read at the head operation, such as `--at 0s`, keeps it too. Reads at head leave it out, as now.
- **No other marker.** `workspace_status.source` stays as it is: `"operation"` in workspace scope, already kept. A brief read at head never has `at_operation`, so the key's presence marks a historical read in both scopes.

Before and after, `reef status --repo a3dda866c7e8f343 --at-op 9e22f9e2762b | cat` (description cut):

```
{"repository_status":{"trunk":{…},"trunk_bookmark":"main","active_workspace_count":1,"head_count":27,…}}
{"at_operation":"9e22f9e2762b841a…0c7b0572","repository_status":{"trunk":{…},"trunk_bookmark":"main","active_workspace_count":1,"head_count":27,…}}
```

**Cost:** 146 bytes, only on reads at an operation.

### How the view knows

`From<&json::RepoStatus>` and `From<&json::Diff>` stay the head views. They leave out `at_operation`, and MCP `get_status`, the tests and every head read keep using them. Each view gains a constructor for a read at a named operation:

```rust
impl<'a> RepoStatus<'a> {
    /// The view of a read at an operation the request named (`--at-op`,
    /// `--at`): [`From`]'s, with `at_operation`.
    pub fn read_at(status: &'a json::RepoStatus) -> Self
}
impl<'a> Diff<'a> {
    /// The same, for a diff: lossless, `at_operation` included.
    pub fn read_at(diff: &'a json::Diff) -> Self
}
```

The field is `#[serde(skip_serializing_if = "Option::is_none")] at_operation: Option<&'a str>`, `None` in `From`. `read_at` sets it to `Some` only when the id is non-empty (rule 2). The call sites pick the view with the flag:

- `status.rs`: `let named = args.at.given();` before the request, then `|s| if named { brief::RepoStatus::read_at(s) } else { brief::RepoStatus::from(s) }`.
- `diff.rs`: the same with `args.read.at.given()` and `brief::Diff::read_at(&text.diff)`.
- `mcp/tools.rs` `get_diff`: `let named = args.at_operation.as_deref().is_some_and(|op| !op.is_empty());` before `args` is moved, then `read_at` when `named`.

A `bool` argument to one constructor would also work. Two named constructors keep `From` the head view, which every existing caller already uses.

### Text

`reef status -o text` and `reef diff -o text` read at a named operation print one first line before what they print now (user decision 2026-11-03):

```
At operation 9e22f9e2762b (2026-11-03T17:37:54.807Z)
```

- **`status`:** the time is `repository_status.last_operation.ended_at`, which is the operation read (`status.proto:150`). With no `repository_status` or no `ended_at`, the line is `At operation <12 hex>` alone.
- **`diff`:** `GetDiffResponse` carries no operation time, so the line is `At operation <12 hex>`. No extra `GetOperation` call is made for it. `git apply` skips text before the first `diff --git` header, so a hunk diff stays appliable.
- **Reads at head print nothing new**, byte for byte.
- **How the renderer knows.** Both renderers need the request's flag, as the views do. `DiffText` gains `#[serde(skip)] named: bool`. `status` renders through a new `StatusText { #[serde(flatten)] status: RepoStatus, #[serde(skip)] named: bool }` in `render_vcs.rs` (or `#[serde(transparent)]` over `status`, like `DiffText`), so its full JSON is unchanged; `TextRender for RepoStatus` moves to it.
- **The stderr note** stays as it is. With `--at` in text form on a terminal, the operation is then named twice, on stderr and on stdout's first line. That is accepted: the note goes to every form, and only text gains the line.
- **Cost:** about 52 bytes, only on text reads at an operation. No benchmark candidate reads text at an operation.

### `ws list`

`ws list` stays as it is in both forms. Its brief and full forms are a top-level array (ADR 0045 decision 2), and a JSON array has no place for a key. There are three ways to make room, and each is worse than the gap:

- an object only at an operation, which changes the shape with a flag and breaks a reader that indexes the array;
- `at_operation` on every element, which is not in the full `Workspace`, so rule 1 breaks unless the full JSON grows a field that repeats the same value on every element;
- an object always, which breaks every script and the `state-repo` guard.

The caller named the operation on its own command line. With `--at`, stderr already names the resolved operation and its time. `reef status --at-op <op>` now gives the full id and the active-workspace count at that point.

### The stderr note

The note does not change. `--at` still prints `reef: at operation <12 hex> (<time>)`. `--at-op` still prints nothing, since the operation was given. The brief now carries the full id. The note still gives the time for `--at`, which is the one case where the caller did not name the operation.

### No API change

The data is already in `GetStatusResponse.at_operation` and `GetDiffResponse.at_operation`, and in `json::RepoStatus` and `json::Diff`. `API_REVISION` stays 14, the proto is untouched, and the database schema stays 6. `reef schema` derives `brief.RepoStatus` and `brief.Diff` from the structs, so both gain an optional `at_operation` string property. No schema document is committed. The MCP output schemas are published by `reef schema mcp`, not declared in `tools/list` (ADR 0050), so `tools/list` is byte-identical.

### ADR 0045

ADR 0045 is amended in place, as ADR 0054 was by R2.07. Decision 5 already lets the keys a view keeps change in a minor release, so no new ADR is needed. But decision 4 says outright that both views drop `at_operation`, and that statement becomes false:

- Status line: `- Status: accepted; amended by #126 (<merge date>)`.
- Decision 4, *Status*: "`repository` is dropped, and `at_operation` is kept only when the request named an operation (`--at-op`, `--at`; #126), since a read at head is the default and the key would only repeat the head; so are the repository's …".
- Decision 4, *Diff*: "lossless at a named operation; at head only `at_operation` is dropped; files leave out …".
- Decision 4, *Workspace*: append "A list is an array and carries no read point; with `--at`, stderr names it."
- Decision 7: append "`get_diff` keeps `at_operation` when the call passes one."
- Consequences: one bullet: "A brief `status` or `diff` read at an operation names it, in 146 bytes, and its text names it in a first line; reads at head do not change (#126)."

### The benchmark

Every candidate reads at head, so no measured output changes. The scenario's digest does not change, so there is no new baseline and no `tokens.yml` dispatch. `state-own` (`reef status`, 858 bytes) and `state-repo` (`reef ws list`, 215 bytes) stay byte-identical. Integration test 6's byte comparison and the unchanged head-view tests guard that. `tokens.rs` gains no test: it never reads at an operation, so a guard there would assert nothing this ticket could break.

## Deliverables

- **`crates/reef-client/src/brief.rs`**: the `at_operation` fields and `read_at` constructors above. Update the doc comments on `Diff` ("Dropped at head: `at_operation`; kept by [`Diff::read_at`]") and `RepoStatus` (the same, with `repository` still dropped), and add one sentence to the module comment: "A view of a read at an operation the request named keeps `at_operation` (`read_at`)."
- **`crates/reef-vcs/src/cli/render_vcs.rs`**: `StatusText` and `DiffText.named`, and the first line under "Text".
- **`crates/reef-vcs/src/cli/status.rs`**, **`diff.rs`**: pick the view and the text by `given()`. Update `status.rs`'s module comment: "…without a snapshot; piped, the brief names the operation."
- **`crates/reef-vcs/src/mcp/tools.rs`** (`get_diff`): pick the view by `named`.
- **Tests**: 1 to 8 below.
- **`docs/adr/0045-brief-json-by-default.md`**: the amendment above.
- **`CHANGELOG.md`**, Unreleased, **Fixed**: "Piped `reef status` and `reef diff` read at an operation (`--at-op`, `--at`) keep `at_operation`, the full id of the operation read, so brief JSON says when its answer is not about now; so does the MCP `get_diff` tool when called with `at_operation`. Their text read at an operation starts with `At operation <id>`. Reads at head print what they did ([#126](https://github.com/reef-vcs/reef/issues/126))."
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #126 | [issue-126-brief-keeps-at-operation.md](issue-126-brief-keeps-at-operation.md) | S | #114, R2.12 | none listed |`.
- **Unchanged**: the proto, `API_REVISION`, ADR 0046, `reef status --help` and `reef diff --help`, `reef guide`, `docs/agent-guide.md`, `docs/quickstart.md`, `docs/token-benchmark.md`, `scenario.toml`, `tokens.rs`, and the MCP tool descriptions.

## Tests

### `crates/reef-client/src/brief.rs`, in the existing `mod tests`

`full_status` and `full_diff` already set `at_operation` to `"77".repeat(64)`.

1. **`brief_status_read_at_keeps_the_operation`**: for `full_status(true)` and `full_status(false)`, `serde_json::to_string(&RepoStatus::read_at(&s))` starts with `{"at_operation":"7777` (the first key, the full 128 characters). With `at_operation` removed, its value equals `RepoStatus::from(&s)`'s.
2. **`brief_diff_read_at_is_lossless`**: `expand_diff(&value(&Diff::read_at(&full_diff())))` equals `value(&full_diff())` with nothing removed.
3. **`brief_read_at_leaves_out_an_empty_operation`**: with `at_operation: String::new()`, `read_at` serializes the same as `From`, for both views.
4. **`brief_is_a_subset_of_full`** (extended): add `RepoStatus::read_at` for both scopes and `Diff::read_at`.
5. **`brief_status_drops_the_repository_listing`** and **`brief_diff_is_lossless`**: unchanged. They are the head guard; `From` still leaves `at_operation` out.

Run with `cargo nextest run -p reef-client --lib brief`. These are `reef-client` library tests. They are not in the `reef` bin, unlike `render_vcs`'s.

### `crates/reef-vcs/src/cli/render_vcs.rs`, in `mod tests`

Run with `cargo nextest run -p reef-vcs --bins render_vcs` (the `reef` bin, not `--lib`).

5a. **`status_text_names_a_named_operation`**: a `StatusText` with `named: true` and a `last_operation` with `ended_at` renders `At operation <12 hex> (<ended_at>)` as its first line, then exactly the `named: false` rendering. Without `ended_at`, the line has no parenthesis.
5b. **`diff_text_names_a_named_operation`**: `DiffText` with `named: true` starts with `At operation <12 hex>\n` in each of the summary, stat, hunks and symbols formats; with `named: false` the output is unchanged.

### `crates/reef-vcs/tests/vcs.rs`

6. **`brief_names_the_operation_read`** (`// #126 test 6`), a new test after `replay_cli`:
   - **Setup**, as `replay_cli` has it: `fixture()` (its one-hour debounce keeps the head still), `create`, write `one.txt`, `ws snapshot`. Then `let x = head_op(…)`, write `two.txt` and `ws snapshot` again, so the head is no longer `x`.
   - **Workspace scope.** Piped `status --at-op &x[..12]`: `at_operation` is `x`, the full id from a prefix, and `workspace_status.source` is `"operation"`.
   - **Repository scope.** Piped `status --repo <id> --at-op &x`: `at_operation` is `x`, and there is no `workspace_status`.
   - **`--at`.** Piped `status --at 0s`: `at_operation` is the current `head_op`, and stderr starts with `reef: at operation `. Piped `status --at-op &x`: stderr is empty, so the note is unchanged.
   - **Head reads are byte-identical.** `let h = head_op(…)`. In repository scope, `head = stdout(status --repo <id>)` and `at_head = stdout(status --repo <id> --at-op &h)`. `head` has no `"at_operation":`, and `at_head.replacen(&format!("\"at_operation\":\"{h}\","), "", 1) == head`, byte for byte. Use repository scope because a workspace read at head reports `source` `index`, not `operation`.
   - **Diff.** Piped `diff --at-op &x` has `at_operation == x`, and `assert_brief_of(&brief, &full, "diff")` against `-o json diff --at-op &x` passes with nothing removed. Piped `diff` has no `"at_operation":`.
   - **`ws list` unchanged.** Piped `ws list --at-op &x` is an array whose stdout has no `"at_operation":`.
   - **Text.** `-o text status --at-op &x` starts with `At operation {&x[..12]} (`. `-o text diff --at-op &x` starts with `At operation {&x[..12]}\n`. `-o text status` and `-o text diff` at head contain no `At operation`.
7. **`status_brief_and_full`** and **`diff_brief_matches_full`**: unchanged. Both pin the head view.

### `crates/reef-vcs/tests/mcp.rs`, in `reads`

8. Beside the `get_diff` calls (`:504`): take `op` from `mcp.ok("list_changes", json!({"page_size": 1}))["at_operation"]`. `mcp.ok("get_diff", json!({"at_operation": &op[..12]}))` has `at_operation == op`. The existing `get_diff` result with no arguments (`hunks`) has no `at_operation` key.

### `crates/reef-vcs/tests/schema.rs`, in `outputs_match_their_schemas`

9. After `describe` sets `op` (`:301`), add `script.both(&a_dir, &["status", "--at-op", &op]).await;` and `script.both(&a_dir, &["diff", "--at-op", &op]).await;`. These validate the brief and full outputs with the key against `reef schema cli`.

## Implementation notes

- **Order:** `brief.rs` with tests 1 to 5; then the three call sites; then tests 6, 8 and 9; then the ADR, CHANGELOG and README.
- **The closure.** `Out::object_with` takes `impl FnOnce(&'a T) -> B` (`reef-vcs/src/cli/out.rs:87-95`), and both branches return the same `B`, so an `if` inside one closure compiles. Compute `named` before `args.at` is moved into the request.
- **Text.** `StatusText` must keep `status`'s full JSON byte-identical: `status_brief_and_full` and `outputs_match_their_schemas` guard it.
- **Timing tests flake under load** on `main`. Test 6 snapshots explicitly and reads the head with `head_op` right before each comparison, so it never waits on the watcher.
- **No `cfg(target_os = "linux")` code** is touched.

## Acceptance criteria

- [ ] Piped `reef status` and `reef diff` read with `--at-op` or `--at` print `at_operation`, the full id, as the first key of `status` and after `to_commit_id` in `diff`. Reads at head print what they printed before, byte for byte (tests 1 to 6).
- [ ] `status` and `diff` text read at a named operation start with `At operation <12 hex>` (with the time for `status`); text at head is unchanged (tests 5a, 5b, 6).
- [ ] MCP `get_diff` with `at_operation` returns it, and without one does not. `get_status`, `list_workspaces` and `tools/list` are unchanged (test 8, and the existing `tools/list` budget test).
- [ ] `ws list`, `ws inspect` and `op log` output is unchanged in every form. The stderr note is unchanged.
- [ ] No API or schema change. `api_digest_matches_revision` passes at revision 14. `reef schema cli`'s `brief.RepoStatus` and `brief.Diff` list `at_operation` (test 9).
- [ ] `just tokens-check` passes unchanged. ADR 0045, CHANGELOG and the tickets README are updated. `just ci` passes.

## Verification

```sh
cargo nextest run -p reef-client --lib brief
cargo nextest run -p reef-vcs --bins render_vcs
cargo nextest run -p reef-vcs --test vcs brief replay_cli
cargo nextest run -p reef-vcs --test mcp reads
cargo nextest run -p reef-vcs --test schema outputs_match_their_schemas
REEF_TOKENS_SOURCE=/Users/arsood/Projects/ReefVCS/reef just tokens-check
just ci
```

`just tokens-check` skips silently inside a Reef workspace unless `REEF_TOKENS_SOURCE` names a checkout with git history (#115's dogfooding note).

## Out of scope

- **An `at_operation` echo in `ws list`, `ws inspect`, `op log`, `log`, `file list` and `symbols` JSON.** Their full JSON never carried one. For the array forms it is a shape change. For `ws inspect` (an object) it would be an additive key in the full JSON, but it has no brief view and is not this bug. The PR files one follow-up issue that lists them.
- **`at_operation` on the MCP `get_status`, `list_workspaces` and `get_workspace` tools.** ADR 0046 decision 10 left MCP out, and `tools/list` is at its budget.
- **File counts in `ws list --at-op`.** `ws.rs:421-431` does not ask for `changed_file_counts` at an operation, since ADR 0045 decision 6 counts at head, so the `FILES` column is `-` and the brief has no counts. That is a separate gap, not filed here.
- **A note on stderr for `--at-op`.**

## Decisions taken with the user (2026-11-03)

1. **`ws list` at an operation:** no change in either form, for the reasons under "`ws list`". No stderr note for `ws list --at-op`.
2. **Text forms:** named in this ticket: `status` and `diff` text read at a named operation start with `At operation <12 hex>` (see "Text").
3. **MCP `get_diff`:** follows the view, and returns `at_operation` when the caller passed one.
