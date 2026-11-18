# Issue #263: tell an agent when another workspace is changing the same files

Phase 3 · Size M, low end · Fixes: issue #263 · Depends on: R2.02 (`GetStatus`), R2.23 (the status index, ADR 0048), #114 (brief JSON, ADR 0045), #259 (`Workspace.behind_trunk`, the read-at-`GetWorkspace` precedent) · Builds on: ADR 0029 (v1 only grows), ADR 0035 (MCP), ADR 0050 (`reef schema`) · Interacts with: R3.10 (Stacks, in flight: its own revision and `Workspace` fields), R3.36 (the map view's conflict warning can read the same computation), #262 (lifecycle `next` in `reef here`, separate), #145 (no overlap event; see decision 6) · Unblocks: nothing listed

## Context

Two agents editing the same file find out when a rebase or a landing conflicts. Reef knows earlier: each workspace's change is a jj commit, recorded at every snapshot, and presence (R2.10) lists the paths each snapshot edited. An agent has to go and ask (`reef presence watch`, which has no MCP tool), and nothing in the commands it already runs says another workspace touches its files.

The issue puts the signal in `reef status`, `reef here` and MCP `get_status`, and fixes three inputs: compute from persisted state (changed paths per open change), not presence; keep status answered from the R2.23 index with no tree walk; decide whether released workspaces' unlanded changes count. Acceptance: two workspaces change one file and each `reef status` names the other within one snapshot interval; `status/large/index_*` stay inside their gates; with no overlap, brief output gains zero bytes.

### What was verified while writing this ticket

Reef at `b68efa3f`: API revision 34, schema 20, ADR 0077 the latest, threat rows to S-206. R3.10 (Stacks) runs in parallel with revision 35, schema 21, ADR 0078 and S-207 to S-213. This brief takes **API revision 36**, **ADR 0079** and threat rows **S-214 and S-215**, renumbered on rebase. It needs **no schema** (decision 3), so slot 22 stays unused.

- **The proto.** `Workspace` ends at field 24 (`disk`, #272). `behind_trunk = 23` is the precedent: a figure read from the repository per request, set by `GetWorkspace` (and documented for `GetStatus`) for one workspace, unset in `ListWorkspaces` and at an operation (`service/workspaces.rs:633`, `rebase.rs:182`). `WorkspaceStatus` ends at 12 (`conflict_assignment_count`, R3.27, whose "at most 10, newest first, plus a count" this brief copies).
- **`GetStatus`** (`service/status.rs`) snapshots through the index, then runs one job on the repository's read thread at head: trunk, `heads(mutable())` described one by one, the conflict and divergence revsets, and the workspace's `@` against its parents through `diff::diff_page` (summary, `limit = max_files`). `diff_page` streams the whole tree diff whatever the limit (it counts `total`), and before that streams it once more to count differing files for the rename limit.
- **`reef here`** (`reef-vcs/src/cli/here.rs`) calls `GetWorkspace` with `skip_disk_usage` and no snapshot, flattens `json::Workspace` into its answer, and builds `hint` and `next` in `advise_with_disk`. Its text stays within 12 lines (`here::tests::text_fits_twelve_lines`).
- **Brief views** (`reef-client/src/brief.rs`): `Workspace` keeps `id`, `owner`, set flags and counts, and `disk` when not `ok`; `WorkspaceStatus` embeds it. Every key is a subset of the full JSON, zero values are left out, and lists are never shortened (ADR 0045, decision 3).
- **MCP**: `get_status` returns the brief status view and `get_workspace` the full `Inspected`. `tools/list` carries no output schemas (`mcp/tools.rs:109`), so new output fields do not touch its byte budget. This ticket adds no tool and changes no description.
- **What is open in a real repository.** In this repository on 2026-11-18, `mutable() & ~::trunk()` holds 46 commits and `heads(mutable())` 23. Released workspaces' changes stay visible and mutable (released `7bc8e285`'s `qtrnoruvrxwy` is a head, 36 minutes old). So do changes landed through a forge's squash merge (ADR 0044, decision 3): R2.16's four changes are open, two weeks old. Without an age bound, every agent here would be warned about work merged weeks ago.
- **Whether "landed" can be read from content.** Of the 43 non-empty open changes, a comparison of each changed path with `trunk()` found the content equal for 0 to 44 of a change's paths (`wkxsprzswkvm`, three weeks old, changes 86 paths and 42 still differ): trunk has edited most of those files again since the squash merge. `git merge-tree` of each change onto trunk conflicts for most old ones. So no content check tells an old squash-merged change from open work; the age bound does that, and the content check only covers the first days after a merge, before trunk edits the file again.
- **The history index** (ADR 0044) stores each change's paths (`change_paths`, indexed by path) and outcome, but computes them on a throttled worker: once a change is quiet for 5 s or its touches are 60 s behind (`history/touch.rs`). That misses "within one snapshot interval" (decision 3).
- **The status bench gates** (`docs/benchmarks.md`, `large`): `index_clean` < 15 ms (12.2 ms measured), `index_one_edit` < 100 ms (87.5 ms), `index_100_edits` < 250 ms (91.9 ms). The bench has one workspace and no other open change, so it measures what the feature adds to a status with nothing to find, not the cost of finding.
- **Remote tokens** are scoped `read` or `write` across every registered repository (ADR 0049, decision 5): a reader can already list every workspace and diff every change.

## Decisions

### Size (decision 1)

**M, low end**: one daemon module (the candidate set, the path cache, the matching), one proto message and two `Workspace` fields, the brief view, three CLI renderings, the `here` hint, one ADR, two threat rows, one bench row. Out: symbol-level matching and an overlap event (decisions 6 and 7), the web UI (R3.36).

### The data shape (decision 2)

```proto
// Another open change that changes a path this workspace's change
// changes (#263, ADR 0079).
message Overlap {
  // The other change.
  string change_id = 1;
  string change_id_reverse_hex = 2;
  // Its commit, as read.
  string commit_id = 3;
  // A Reef workspace on that change: an active one, else the newest
  // released one; empty when no Reef workspace has been on it.
  string workspace_id = 4;
  // That workspace's owner; with no workspace, the commit author's email.
  string owner = 5;
  // Whether an active workspace is on it now.
  bool active = 6;
  // The paths both changes change, in path order, at most 10.
  repeated string paths = 7;
  // Such paths, in all.
  uint32 path_count = 8;
  // When the other change was last written: its commit's committer time.
  google.protobuf.Timestamp written_at = 9;
}

message Workspace {
  ...
  // Other open changes that change paths this workspace's change changes,
  // at most 10, newest written first (#263). Set by GetStatus at head and
  // GetWorkspace; empty elsewhere, at an operation, and when none.
  repeated Overlap overlaps = 25;
  // Such changes, in all.
  uint32 overlap_count = 26;
}
```

On `Workspace`, as `behind_trunk` is, so status (`workspace_status.workspace`), `here` (which flattens the workspace), `ws inspect`, MCP `get_status` and `get_workspace` all carry it through one field and one brief rule. `ListWorkspaces` leaves it empty (decision 8).

**The brief view** of `Workspace` adds `overlaps` and `overlap_count`, both left out when empty or zero, so a workspace with no overlap gains zero bytes. A brief `Overlap` keeps `change_id_reverse_hex`, `workspace_id`, `owner`, `active`, `paths` and `path_count`, and drops `change_id`, `commit_id` and `written_at`. The bounds (10 changes, 10 paths each) are the server's, so the brief list is the full list and the subset rule holds. A typical overlap is about 120 bytes brief.

### Where the paths come from (decision 3)

**From the repository at head, through a cache keyed by commit id.** The overlap is computed on each read from the view the read already holds:

1. **Mine**: the paths the workspace's `@` changes against its parents, merged, as `GetStatus` counts them. In `GetStatus` they come from the diff the status already streams (the limit raised to 10,000 for the path list, the response still cut at `max_files`); in `GetWorkspace`, from the cache.
2. **Candidates**: `mutable() ~ (::@ | @::)` evaluated with the workspace's scope, newest first, at most 1,000 commits, then filtered (decision 4). Ancestors are the workspace's base and descendants are work stacked on it, so neither is another line of work. Commits on the workspace's own change id are dropped (a shared change, a divergent copy).
3. **Theirs**: each candidate's changed paths against its parents, merged, from a daemon-wide cache keyed by `(repository, commit id)`. A commit's diff never changes, so the cache is never invalidated, only bounded: up to 10,000 paths per commit (the history index's `MAX_PATHS`) and 250,000 paths in all, after which it is cleared and refilled on demand. A miss is one tree diff with no copy detection and no content read (`diff::changed_paths`, which R2.10's editing presence already runs after every snapshot), computed on the read thread inside the read's job.
4. **Matching**: for each candidate, the smaller path list is looked up in the larger by binary search (both are in path order). A candidate whose list was cut at 10,000 is checked for each of my paths past its last stored path by a tree lookup, so a repository-wide reformat still matches.
5. **Landed paths drop out**: a shared path where the candidate's content equals `trunk()`'s is not an overlap. That change adds nothing there over trunk (it landed through a squash merge, or the same edit landed), so rebasing either change cannot conflict on it. Two tree lookups per shared path; skipped without a `trunk()`.
6. **Attribution**: one query of the repository's workspace rows by the overlapping change ids (at most 10): an active row wins, else the newest released row. Its id and owner fill `workspace_id` and `owner`; with no row, `owner` is the commit's author email.

Why not the alternatives:

- **The history index's `change_paths`**: persisted and indexed by path, but its worker computes a busy change's paths about once a minute (5 s quiet, 60 s stale). The acceptance asks for one snapshot interval. Making its path half eager would change ADR 0044's worker for a second consumer.
- **Presence's editing entries**: ephemeral, lost at a restart, coalesced under load, and they list what each snapshot edited, not what the change changes. The issue rules them out.
- **A SQLite table of paths per open change**, kept by every snapshot and every rewrite (rebase, abandon, land, move): a second copy of what jj's commits already say, kept equal at every write path. ADR 0048 rejected the same for file states.
- **An inverted index (path to changes) in memory**: needs every rewrite to update it. The per-commit cache needs no invalidation, and matching is a few thousand lookups at 200 open changes.

"Persisted state" in the issue is met by jj's commits: the overlap is a pure function of the view at head, the same after a restart, with only the cache cold.

### Which other changes count (decision 4, the user's)

- **Active workspaces' changes always count**, however old.
- **Every other open change counts if it was written within `[overlaps] max_age`** (default 7 days; `0s` counts only active workspaces): released workspaces' unlanded changes, changes made in the root checkout, changes fetched from elsewhere. Age is the commit's committer time, which each snapshot updates.
- **Not**: the workspace's own change, its ancestors and descendants (decision 3.2), empty changes, hidden or landed changes (they are not `mutable()`), and paths landed in substance (decision 3.5).

Here, 7 days keeps the two changes of #316 (36 minutes) and the PR #209 change of a week ago, and drops the 40 older ones, R2.16's among them.

### What an agent sees (decision 5)

- **`reef status`**: the JSON gains it through `workspace_status.workspace`. The text gains one line per overlap, at most 3, after the changed files, then `and N more` when there are: `Also changing src/lib.rs (+2 more): workspace 1a2b3c4d (claude@host), change kxyzpqrs; reef diff -r kxyzpqrs`. A released workspace's line says `released workspace`.
- **`reef here`**: the JSON gains it through the flattened workspace. The `hint` gains one sentence after the one chosen today, naming the newest overlap: `Workspace 1a2b3c4d (claude@host) also changes src/lib.rs; `reef diff -r kxyzpqrs` shows its change. Conflicts do not block, so carry on or coordinate.` `next` is unchanged (#262 owns it). The text gains one line within the 12-line budget.
- **`reef ws inspect`**: JSON as `GetWorkspace` returns it; text one line, as status's first.
- **MCP**: `get_status` (brief view) and `get_workspace` (full) carry it. No new tool, no description change, `tools/list` unchanged.
- **Brief bytes**: zero added when there is no overlap. The token benchmark's fixtures have none, so its numbers do not move; a unit test pins a brief status and a brief `here` without overlaps byte for byte.

### Symbol-level matching (decision 6, the user's)

**Left to a follow-up.** Telling "same symbol" from "different symbols in one file" needs both changes' symbol diffs (R2.15) on the read path. A cold symbol diff of 100 files is 147 ms on `medium` (`docs/benchmarks.md`), past the status budget, and the history index's stored symbol touches lag up to a minute. The follow-up can match symbols only for the shared paths, from the history touches when both are current, and mark each overlap `symbols_disjoint`.

### An event when an overlap begins (decision 7, the user's)

**Left to a follow-up**, with #145. An event needs the overlap computed on the write path (after each recorded snapshot, for the snapshotting workspace and everyone it now overlaps), a new presence kind or operation attribute, and a `wait_for_operations` kind for MCP. Each is small, but together they double the ticket and move cost onto every snapshot. Agents see the overlap at their next `status` or `here`, which they already run between steps.

### Where it is computed, and what it costs (decision 8)

- **`GetStatus` at head** (with or without `skip_snapshot`): inside its existing read job, after the workspace's diff. At an operation: empty (an overlap is about now).
- **`GetWorkspace`** now: one more read job, as `behind_trunk` runs. Best effort in both: a failure is logged and leaves the list empty, never failing the read.
- **`ListWorkspaces`**: empty. A fleet listing would compute every workspace's overlaps on each call; R3.36's map view decides its own.
- **Cost with nothing to find** (the bench's case): one revset over the mutable commits and one cache hit for my own commit. The `index_*` gates have 2.8 ms of headroom on `index_clean`; the expected cost is well under 1 ms.
- **Cost with open work**: a new bench row, `status/<preset>/index_clean_open_changes`: 20 other workspaces, each with an open change sharing one path with the measured one, the cache warm. Recorded, with a target of under 15 ms on `large` (`index_clean`'s gate).
- **Cold cache** (after a daemon start): the first read diffs each candidate once, up to 1,000. On this repository that is about 20 commits.

### Configuration (decision 9)

`[overlaps] max_age` (duration, default `7d`, `0s` for active workspaces only). Nothing else: the bounds are constants, and there is no off switch, since a status with nothing to find costs one revset.

### ADR and threat rows (decision 10)

**ADR 0079, "Overlaps from open changes' paths"**: decisions 2 to 9. **S-214**, disclosure: an overlap names another workspace's paths, owner and change to any caller of `GetStatus` or `GetWorkspace`; the same caller can already list workspaces and diff changes, locally (one uid) and with a remote `read` token, so nothing new is exposed. **S-215**, cost: a read on a cold cache diffs up to 1,000 commits; bounded by the candidate cap, the cache budget, and the remote listener's per-token limits.

## Deliverables

### `crates/reef-lib/`

- `jj/diff.rs`: `changed_paths_vs_parents(repo, commit, limit) -> (Vec<String>, bool)` (merged parents, cut flag), and `same_at(repo, tree_a, tree_b, path) -> bool`.
- `jj/revset.rs` or a new `jj/overlap.rs`: the candidate walk (decision 3.2): commit id, change id, committer time, author email, newest first, capped.

### `crates/reef-daemon/`

- `src/overlap.rs`: `PathCache` (decision 3.3), `find(...)` returning `(Vec<v1::Overlap>, u32)`; attribution through one query in `db`.
- `service/status.rs`: raise the diff's limit for the path list, call `find` inside the job, set `workspace_status.workspace.overlaps` at head.
- `service/workspaces.rs`: `GetWorkspace` fills them (one job; beside `behind_trunk`).
- `config.rs`: `[overlaps] max_age`, its key in `KEYS`, `reef config` output.
- `benches/status.rs`: `index_clean_open_changes`.
- `tests/overlaps.rs`: the integration tests below.

### `crates/reef-api/`, `crates/reef-client/`, `crates/reef-vcs/`, `sdk/`

- `workspaces.proto`: `Overlap`, `Workspace.overlaps = 25`, `overlap_count = 26`. `API_REVISION` 36, `API_DIGESTS`, the SDKs regenerated.
- `reef-client`: `json::Overlap`, `json::Workspace` fields, the brief view.
- `reef-vcs`: status, `here` and `ws inspect` text; `here`'s hint sentence.
- `reef schema` derives the new keys; no `OUTPUTS` row changes.

### Documentation

- `docs/adr/0079-overlaps.md`; `docs/threat-model.md` S-214, S-215.
- `[overlaps]` documented where every section is: `config.rs`'s doc comments and `reef config`'s output.
- `docs/benchmarks.md`: the status section's before and after numbers and the new row.
- `docs/agent-guide.md` (and the `reef guide` text it mirrors): one sentence on overlaps beside `reef status`.
- README "Phase 3 so far" and CHANGELOG Unreleased.
- This README's table row.

## Tests

`crates/reef-daemon/tests/overlaps.rs`, through a test daemon with notify watchers:

1. **Both directions within one snapshot interval** (the acceptance): workspaces A and B on trunk each write `src/shared.rs`; A's status names B and B's names A, polled for at most `debounce + min_interval` plus a second's slack, with `workspace_id`, `owner`, `active`, `paths == ["src/shared.rs"]`.
2. **No overlap, no bytes**: A and B write different files; `overlaps` empty, `overlap_count` 0, and the brief JSON has no `overlap` key.
3. **The overlap ends** when B reverts its edit or abandons its change.
4. **Released**: B released after its write still counts (`active` false, its id and owner); with `max_age = 0s` it does not; an active workspace counts with `max_age = 0s`.
5. **Stacked**: B created on A's change (`--new-on`) writing the same file: no overlap either way.
6. **Landed in substance**: a change whose `src/shared.rs` equals trunk's (its edit landed through another commit) does not count.
7. **Shared change**: two workspaces on one change: no overlap between them.
8. **Bounds**: 12 overlapping changes give 10, newest first, `overlap_count` 12; 15 shared paths give 10 and `path_count` 15.
9. **No workspace**: a change made by jj in the root checkout counts, `owner` its author's email, `workspace_id` empty.
10. **At an operation**: a status at an earlier operation has none.
11. **`GetWorkspace`** carries the same list as `GetStatus` for the same head.
12. **Cache**: a unit test that a cut list (over the per-commit cap) still matches a path past its last stored entry, and that clearing at the budget gives the same answers.

`reef-client`: brief view unit tests (keys kept and dropped; zero bytes without overlaps). `reef-vcs`: `here` hint and text with an overlap; `outputs_match_their_schemas` covers the schema; MCP `get_status` includes `overlaps` in an integration test.

## Performance and laptop cost

- `status/large/index_*` before and after, on this machine, back to back (the before run on `b68efa3f`), each the median of criterion's 20 samples. Gates as today. Results in the PR and `docs/benchmarks.md`.
- A first before run, taken while the brief was written (`b68efa3f`, release, M2 Max, another agent's builds and a jj spike running): `walk` 266.6 ms, `walk_one_edit` 223.1 ms, `index_clean` 8.4 ms (6.4 to 10.8), `index_one_edit` 81.2 ms, `index_100_edits` 112.9 ms, `skip_snapshot` 8.4 ms. `skip_snapshot` no faster than `index_clean` and a `walk` 65% over the recorded 161 ms say the machine was loaded; the comparison that counts is the back-to-back pair after implementation.
- `index_clean_open_changes` recorded on `medium` and `large`.
- Memory: the cache holds at most 250,000 paths (about 20 MB at 80 bytes a path), shared across repositories.
- No disk, no new thread, no work on the snapshot path.

## Acceptance criteria

- [ ] Two workspaces change one file: each one's `reef status`, `reef here` and MCP `get_status` name the other within one snapshot interval (test 1).
- [ ] `status/large/index_*` within their gates, before and after measured on this machine.
- [ ] No overlap: brief `status` and `here` output byte-identical to before (test 2, the brief unit tests).
- [ ] Released workspaces' unlanded changes count within `max_age`; stacked and landed-in-substance work does not.
- [ ] `just preflight`, `just doc`, the full suite; the SDKs regenerated; `buf` lint and breaking checks.

## Verification

```sh
REEF_BENCH_PRESET=large cargo bench -p reef-daemon --features testing --bench status
cargo nextest run -p reef-daemon --test overlaps --no-fail-fast
# By hand, from two workspaces of this repository:
reef ws run -- sh -c 'echo x >> README.md; sleep 3; reef status'   # in two terminals
```

## Out of scope

- Symbol-level matching (decision 6) and an overlap event (decision 7): follow-up issues.
- `ws list` and the web UI (R3.36's map view).
- Overlaps of the workspace's own unlanded ancestors (a stack's lower changes) with others: the workspace's change only. R3.10 decides what a stack reports.
- `next` in `reef here` (#262).

## Risks

- **A busy repository with many mutable commits** (local branches never pruned): the candidate walk is capped at 1,000, newest first, and the age bound drops old ones after reading their commit. A repository past that cap misses its oldest candidates.
- **False positives from squash merges whose path changed again in trunk**: the landed check compares contents, so a squash-merged change whose file trunk later edited again still counts until `max_age`.
- **The status gate's headroom** (2.8 ms on `index_clean`): measured before and after; the work with nothing to find is one revset.

## Choices this brief made, and why

- The cache, not the history index or a table (decision 3): fresh at head, nothing to keep equal, no migration.
- Exclude ancestors and descendants: not another line of work. (The user's call, decision 4.)
- Content-equal-to-trunk drops a path: no conflict can come from it, and this repository's squash merges would otherwise warn for weeks.
- 10 changes and 10 paths each, as R3.27's assignments: the brief list is the full list, so the subset rule holds.
- `owner` falls back to the author's email so a change with no workspace still names someone, at no extra key.

## What later tickets inherit

- **R3.36** (map view): `overlap::find` per workspace, and the `Overlap` message.
- **The symbol follow-up**: `Overlap` grows a symbol verdict.
- **The event follow-up**: the same computation on the write path.

## Departures from dev-phases.md

None: #263 is not a `dev-phases.md` row, and R3.36's row is unchanged.

## Decisions for the user

**Answered 2026-11-18: all seven accepted as recommended.** Symbol-level matching (decision 3) and the overlap event (decision 4) go to follow-up issues, filed by the main session. The recommendations as they were put:

1. **Which other changes count.** *Recommended*: active workspaces' changes always; every other open change (released workspaces' unlanded changes, the root checkout's, fetched ones) if written within `[overlaps] max_age`, default 7 days; landed-in-substance paths never. Alternatives: a 24-hour default (fewer stale warnings, misses a PR waiting a day); active workspaces only (released work never warns).
2. **Stacked work.** *Recommended*: ancestors and descendants of the workspace's change never count. Alternative: descendants count (the agent whose change another is stacked on is told that the stacked change edits the same file; jj rebases the descendant when the base changes, which can conflict there).
3. **Symbol-level matching.** *Recommended*: a follow-up issue (decision 6). Alternative: in this ticket, from the history index's touches when both are current and path-level otherwise; adds about a third to the ticket and lags a busy change by up to a minute.
4. **An overlap event.** *Recommended*: a follow-up with #145 (decision 7). Alternative: in this ticket, a presence kind `OVERLAP` published from the snapshot hook, plus a `wait_for_operations` kind; about doubles the ticket.
5. **`reef here`.** *Recommended*: one `hint` sentence, `next` unchanged (#262 owns it). Alternative: append `reef diff -r <change>` to `next`.
6. **`ws list`.** *Recommended*: no overlaps there (decision 8). Alternative: list them per workspace (a fleet view in one call, at one overlap computation per listed workspace per call).
7. **Numbers.** API revision 36, ADR 0079, threat rows S-214 and S-215; schema slot 22 unused. *Recommended*: as stated.
