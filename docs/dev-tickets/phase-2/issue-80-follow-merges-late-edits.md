# Issue #80 A write's follow-up carries a busy workspace's late edits

Phase 2 · Size M (no row; see "Size") · Fixes: issue #80 · Depends on: R2.03 (`Manager::rewrite`, its five steps, the divergent-mark reconciliation), R2.04 (`EditFiles` through `Manager::rewrite`, test 15) · Unblocks: nothing listed; R2.27's divergence note, `reef here`'s divergent advice and R2.13's tool text fire less and do not change

## Context

`Manager::rewrite` (R2.03, ADR 0031) holds the workspaces a write moves still: it snapshots them (step 3), writes (step 4), and has each take its new commit (step 5). A workspace whose agent keeps writing files gets edits on disk after step 3's scan. Step 5's follow-up snapshot then meets ADR 0018's fourth arm, a stale working copy with edits, and records those edits as a new commit of the commit the workspace had before the write. For a write that kept the workspace on its change (an `EditFiles` of `@`, an abandon of an ancestor, an undo of an edit), that is a second visible commit of the change: it diverges. Nothing is lost, but the agent has a divergent change to resolve for an ordinary edit. `files.rs::edit_while_the_workspace_writes` (R2.04's test 15) ends with about 15 visible commits from 20 edits.

This ticket makes the follow-up carry those late edits onto the commit the write put the workspace on, in the same change: the new tree is the three-way merge of the write's tree and the tree on disk over the tree the working copy last recorded, and any overlap is a jj conflict in that change. ADR 0018's fourth arm stays for everything Reef's own write did not cause, and for writes that moved the workspace to another change.

No schema change: `API_REVISION` stays 5. Three RPC comments are reworded (comments are outside the digest).

### What was verified while writing this ticket

Reef at `2c403d8` (R2.22 merged as #88); jj-lib 0.46.0 (`Cargo.lock`), read in the registry sources; the `jj` 0.46.0 binary for jj's own behavior. Paths are under `crates/` unless they start with `docs/`. The 15-commit count was not re-run; it is the issue's.

- **The arms.** `reef_lib::jj::Repo::begin_snapshot` (`reef-lib/src/jj/snapshot.rs:664`) reads `old_tree`, the tree the working copy last recorded, from the locked working copy (`:702`); scans the disk into `new_tree`; then matches (`:727`): no working-copy commit recovers; fresh rewrites the view's commit (`:733`); stale with `new_tree` equal to `old_tree` or to the view's commit's tree adopts with `check_out` (`:745`); otherwise rewrites `own_last` (the caller's `expected_commit`, else the working copy's commit at its own operation, `:721`, `last_own_commit` `:482`) and is divergent when that is not the view's commit (`:752`). `commit_snapshot` (`:405`) rewrites `plan.rewrite_of` and marks it divergent also when the view's commit at commit time is not that commit (`:431`); it sets the working-copy commit with `set_wc_commit` (`:440`), which in jj-lib only sets the pointer (`repo.rs:1654`; the old commit is not abandoned, unlike `edit`'s `maybe_abandon_wc_commit`, `:1732`). `SnapshotPlan` (`:209`) and `PendingSnapshot` (`:233`) carry the plan across threads; `finish` saves the state (`:347`).
- **Staleness compares trees.** `WorkingCopyFreshness::check_stale` is `Fresh` when the working copy's recorded tree equals the view's commit's tree, whatever the operations (jj-lib `working_copy.rs:388`). So `DescribeChange`, which changes no tree, never reaches the fourth arm; `EditFiles` with `new_on` moves no working copy (`reef-lib/src/jj/files.rs:1261`, "No working copy moves"). The affected writes are `EditFiles` rewriting a commit, `AbandonChange` and `UndoOperation`.
- **The daemon's snapshot.** `Manager::run_snapshot` (`reef-daemon/src/workspace/snapshot.rs:310`) passes the row's commit as `expected` (`:340`); `Pipeline::record` (`:559`) commits on the repository thread, then writes the row (`record_commit`, `:623`) and only then saves the working-copy state (`finish`, `:624`): rows before state, as R1.16 requires. `record_commit` (`:655`) sets `divergent` and attention `divergent` when the commit diverged (`:668`). Every snapshot passes `Scheduler::scan_hold_point` (`:330`; the test hook `hold_scans`, `snapshot/mod.rs:678`, `:691`).
- **The write.** `Manager::rewrite_detached` (`reef-daemon/src/workspace/rewrite.rs:185`): step 1 lists the moved jj workspaces descendants first (`planned.workspaces`, `order` at `:218`); step 2 locks them in id order; step 3 snapshots them in `order` (`snapshot_moved`, `:303`); step 4 (`commit`, `:713`) reads every working copy before (`:722`) and after (`:802`) the write (`rewrite::working_copies`, `reef-lib/src/jj/rewrite.rs:284`: commit, change, tree, divergence); step 5's `follow` (`:345`) logs moved workspaces it did not lock (`:360` to `:366`; a create or move landed between steps 1 and 4), then for each locked one in id order (`:369`) calls `follow_view` when the tree is unchanged (`:377`) or `snapshot_locked` (`:380`), and a failure is logged with "adopts it at its next snapshot" (`:390`). `reconcile_divergence` (`:401`) then reads the head and sets each moved row's mark to the truth (`:446` to `:455`).
- **What each write moves.** `edit_commit` rewrites one commit and rebases descendants (`files.rs:1222`), so a workspace on it or below stays on its change. `abandon_commit` (`reef-lib/src/jj/rewrite.rs:357`) either resolves a divergent change (working copies on the abandoned commit move to the successor, another commit of the same change) or abandons plainly (working copies on it move to one new empty commit, a new change; those below are rebased, same change). `revert_operation` moves working copies back to where the undone operation found them: usually another commit of the same change (undoing an edit or a snapshot), another change for an undo of an abandon.
- **The fourth arm for a move to another change is not a divergence.** After a plain abandon of the workspace's own `@` with late edits, arm 4 rewrites the abandoned (hidden) commit: the abandoned change comes back with one visible commit, the workspace sits on it, the abandon's new empty commit stays visible with no workspace, and `reconcile_divergence` clears the mark that `record_commit` set (the change is not divergent at head). Undoing an abandon with late edits is the mirror image. The issue's "the change diverges" holds for moves within a change.
- **jj does the same.** In a scratch repository with `jj` 0.46.0: workspace `ws2`'s `@` rewritten from the default workspace (`jj squash --into ws2@`), then a file edited in `ws2`, then `jj workspace update-stale` in `ws2`: "Concurrent modification detected, resolving automatically", and `ws2@` is `(divergent)` with the late edit on the other commit. ADR 0018's arm 4 is jj's behavior; carrying edits is Reef's own.
- **Two workspaces on one change diverge by design.** ADR 0018 rejects moving a workspace "onto another agent's tree" over its own edits; `jj_workspace.rs::stale_diverges_when_dirty` (`reef-lib/tests/jj_workspace.rs:625`, fixture `two_on_one_change` `:575`) and `writes.rs::abandon_resolves_divergence` (`:433`) build divergence that way. Nothing here may change them.
- **jj-lib's merge.** `MergedTree::merge(Merge<(MergedTree, String)>)` merges and resolves, with a conflict label per term (`merged_tree.rs:339`); `Merge::from_vec([a, base, b])` is two adds over one remove, as `restore_tree` uses it (`rewrite.rs:240`). File conflicts resolve content and the executable bit separately; an executable bit both sides changed differently leaves the file conflicted (`tree_merge.rs:411`, `:453`).
- **jj-lib's check-out overwrites unchecked.** `TreeState::check_out(new_tree)` updates every path that differs between its current tree and `new_tree` (`local_working_copy.rs:2290`) and does not check that the file on disk is still what it scanned ("TODO: Check that the file has not changed before overwriting/removing it.", `:2458`). A path present on disk but absent from the old tree is skipped (`skipped_files`). The adopt arm already has this window between its scan and its check-out.
- **Who reads the outcome.** Presence `EDITING` entries diff `SnapshotOutcome.rewrote` against the new commit (`presence/publish.rs:105`, `:127`), so a carried snapshot reports exactly the late edits. R2.27's CLI notes a divergent workspace after a write and `reef here` points it at `reef log` (ADR 0034 item 10, "until it is fixed"); R2.13's server instructions say what `workspace.divergent` means (`reef-vcs/src/mcp/instructions.md:5`). Both stay true: divergence still exists, for the cases below.
- **Accepted ADRs are not edited.** `docs/adr/README.md:7`: "After acceptance an ADR is edited only to change its status or add a superseded note; new information goes in a new ADR." Every accepted ADR has exactly one commit in its history. The issue's "update ADR 0018 and ADR 0031 (and ADR 0032's consequence)" becomes a new ADR, 0039 (the next free number; none is reserved).

## Decisions

### Size

M, with no `dev-phases.md` row: it fixes behavior R2.03 and R2.04 shipped, as #67 fixed #63. The work is a fifth arm in `begin_snapshot` with its own check-out ordering, a carry slot on each workspace that step 5 fills and every snapshot consumes, a follow order change, two test hooks, about twelve tests, ADR 0039, and comment and doc edits. No schema change, no new reason, no migration. The phase README's ticket table gains a `#80` row.

### When a snapshot carries instead of diverging

A **carry** is `reef_lib::jj::Carry { from: CommitIdHex, to: CommitIdHex, base: Option<MergedTreeId> }`: Reef's own write moved this workspace's working copy from commit `from` to commit `to`, both commits of one change, with different trees. `base` is `None` for a write's carry (the merge's base is the recorded tree, below) and set only for the re-merge after a failed check-out. Only `Manager::rewrite` and that re-merge create carries. A snapshot given a carry uses it when, at its scan:

1. the working copy is stale and has edits that neither adopt condition covers (it would otherwise take arm 4);
2. `to` is the view's working-copy commit for the workspace; and
3. `from` is the commit the workspace was checked out from: `own_last`, which in the daemon is the row's commit.

All three, or the snapshot ignores the carry and takes ADR 0018's arms unchanged. The rule is exact rather than inferred, and that is what makes it safe:

- **Another client's operation** that moved the workspace after the write (the `jj` CLI on the registered root, another Reef write) changes the view's commit, so (2) fails and arm 4 applies, as jj's own `update-stale` does. One that lands between this snapshot's scan and its commit is caught by `commit_snapshot`'s existing check (`snapshot.rs:431`): the commit is marked divergent, as any racing snapshot is today. jj's merge of concurrent operation heads may leave the view at `to`; then the carry applies, which only touches this workspace's tree.
- **Two workspaces on one change** (ADR 0018's core case) never get a carry: an ordinary snapshot creates none, so `stale_diverges_when_dirty` and test 11 of R2.03 hold.
- **A move to another change** gets no carry (decided under "Per write"), so its late edits keep arm 4.

Rejected conditions, recorded in ADR 0039: "the view's commit has the same change id as `own_last`" alone, and "`own_last` is a predecessor of the view's commit in the operation log" (jj-lib `evolution`). Both would also carry for another workspace's snapshot of a shared change, silently undoing ADR 0018's rule; the second also misses undo, which restores an older commit (a predecessor, not a successor) and resolving abandons, whose successor is a sibling commit.

### The merge

- **Base: `from`'s tree** (changed in the implementation from "the tree the working copy last recorded", `old_tree`; the two are equal except after a failed save, where the recorded tree is older and would bring spurious conflicts), not the tree of the commit the workspace was checked out from before step 3. The disk's late edits are exactly `new_tree − old_tree`. When step 3 recorded a snapshot, `old_tree` is that snapshot's tree, and the write was applied to that snapshot's commit (`resolve_target` follows step 3's rewrites), so `to` already contains step 3's edits; a base older than step 3 would apply them twice and conflict wherever the write changed lines step 3 had added. When step 3 recorded nothing, `old_tree` is the checked-out commit's tree. Where `old_tree` and `from`'s tree differ (a committed snapshot whose state was never saved), the recorded tree is still the right base: what both sides already share resolves as the same change.
- **The tree:** `MergedTree::merge(Merge::from_vec(vec![(to_tree, "<to's conflict_label()> (rewritten)"), (old_tree, "<from's conflict_label()> (snapshotted)"), (new_tree, "working copy")]))`. Late edits the write did not touch, and changes of the write's the agent did not touch, resolve; an overlap is a conflict in the change's tree, which `GetStatus` lists (`conflicted_paths`) and `Change.conflict` reports.
- **Modes and deletions** follow jj's tree merge: one side changing the executable bit and the other the content gives both; both changing the bit differently is a conflict; a deletion on one side and a modification on the other is a conflict; a deletion on disk of a path the write did not touch is a deletion; an added file on both sides with different content is a conflict.
- **`snapshot.max-new-file-size`**: a new file over the limit is left out of `new_tree` by the scan, as in every snapshot, and reported untracked; it is not in the merge and stays on disk. If the write added a file at that path, the check-out skips it (jj's `skipped_files`), as the adopt arm does today; the next snapshot then records the disk's file there, because the path is tracked.
- **When the merged tree equals `to`'s tree** (the late edits were already the write's), there is nothing to record: the snapshot checks `to` out, as the adopt arm does, and writes no operation.
- **Otherwise the plan** rewrites `to` with the merged tree: `rewrite_of = to`, `divergent = false`, `carried = true`. `commit_snapshot` is unchanged: the new commit is in the same change, its predecessor is `to`, descendants are rebased onto it, and the working copy points at it.

### Checking the merged tree out, and in what order

The disk holds `new_tree`; the commit holds the merged tree, which also has the write's changes and any conflict. So a carried snapshot checks its new commit out before the working-copy state is saved: `PendingSnapshot::check_out_carried(&mut self, repo: &ReadonlyRepo, commit: &CommitIdHex) -> Result<u32, SnapshotError>` loads the commit and calls the locked working copy's `check_out`, which updates only the paths that differ between `new_tree` and the merged tree (the write's own paths and the conflicted ones) and materializes conflicts with markers; it returns the files it updated. `finish` debug-asserts that a carried snapshot was checked out.

In the daemon, `Pipeline::record` orders a carried snapshot: commit, check out, row (`record_commit`), finish. Uncarried snapshots keep commit, row, finish. The row moves to the new commit only once the disk holds its tree, because a row naming the merged commit over a disk without the write's changes would make the next snapshot record the disk over that commit with no divergence, silently reverting the write for this workspace. With this order:

- **A crash after the check-out**: the state is unsaved, the disk is the merged tree, so the next snapshot adopts (the files already are the commit's, the adopt arm's second condition, `snapshot.rs:739`).
- **A crash between the commit and the check-out**: the row still names `from`, there is no carry after a restart, and the next snapshot takes arm 4: the change diverges, visibly, with nothing lost, as before this fix.
- **A failed check-out** (an I/O error): the snapshot fails and the daemon puts a carry `{ from: <the row's commit>, to: <the new commit>, base: Some(<the disk tree the failed snapshot merged>) }` on the workspace. Its next snapshot merges over that base: with the disk unchanged since, the merge is the carried commit's tree, and the snapshot checks it out without an operation; edits made since are carried on top. The base must be the merged disk tree, not the recorded tree: when the first merge conflicted, merging the conflicted tree again over the recorded tree stacks the late edit's side a second time (adds `two, three, three` over `one, one`), which jj does not simplify back. Test 5 pins both.

### Carries in the daemon

- **The slot.** `WorkspaceEntry` gains `carry: Mutex<Option<Carry>>` (a `std` mutex, a leaf lock: never held across an `.await` or while taking another lock), with `set_carry` (replaces), `take_carry`, and `restore_carry` (sets only if empty, so a newer write's carry wins).
- **Registration, in step 4's repository job** (changed in the implementation from "at the start of step 5", so no snapshot loads the write's head before its carry exists). A locked workspace's `from` is its row's commit when that is in the change (step 3's snapshot of an ancestor rebases a descendant without moving its row). From step 4's working copies before and after (`moved_copies`), every Reef workspace whose commit moved, whose change is the same before and after, and whose tree changed gets `set_carry(Carry { from: before.commit_id, to: after.commit_id })`, locked or not, when its entry exists and belongs to the repository. A pure function `carries(before, after) -> Vec<(WorkspaceId, Carry)>` does the selection.
- **Consumption.** `run_snapshot` takes the slot before its scan and passes the carry to `begin_snapshot`. A snapshot that fails before committing restores it; one that committed and failed its check-out sets the re-merge carry above; any other outcome drops it, whichever arm ran. So the carry reaches exactly the next snapshot of that workspace: step 5's follow for a locked workspace, and for one the write moved without locking it (created or moved between steps 1 and 4) or whose follow failed, its next snapshot of any kind (watcher, explicit, read, move, final, shutdown, or step 3 of a later write). This is the issue's "the next snapshot of a workspace the write moved without locking it".
- **Not persisted.** A daemon restart drops carries; the next snapshot then takes arm 4, as before this fix. It needs a write, late edits, and a restart before the workspace's next snapshot; persisting the slot would mean a schema migration for that window. Shutdown snapshots run with carries in place, so a clean stop is covered.
- **Follow order: descendants first.** `follow` iterates the plan's `order`, as step 3 does, instead of the locked entries' id order. A carried follow rewrites its commit and rebases descendants; done ancestor first, it would move a descendant's working copy off its carry's `to` before the descendant's follow, which would then take arm 4. Descendants first, each descendant has followed before its ancestor's follow rebases it, and adopts that rebase at its next snapshot when it has no newer edits.

### Operations, locks and rows

- **One snapshot operation per busy workspace**, as today's diverging follow records: kind `snapshot`, snapshot kind `explicit`, `reef.workspace` that workspace, `reef.agent` the request's agent else its owner, the default description. Its effects show the write's commit `REWRITTEN` into the carried commit. No new attribute and no new snapshot kind: the late edits cannot join the write's own operation (they reach the disk after its scan), and `explicit` is what step 5 records today.
- **Locks do not change**: step 5 still holds the repository's create lock shared and the scan locks; the carry slot is the only new lock and is a leaf.
- **Divergent marks** need no new code: a carried commit is not divergent, so `record_commit` sets nothing, and `reconcile_divergence` sets every moved row to the truth at head as it does now (clearing a mark the change no longer earns). A carried commit that races another writer at commit time is marked divergent by the existing path.
- **The write's response** still names the commit step 4 wrote (`EditFilesResponse.change`, `DescribeChangeResponse.change`), so it agrees with its `operation`. When a carried follow rewrote that commit, it is hidden by the time the response arrives; the CLI's and MCP server's re-read of the workspace (R2.27, R2.13) shows the current commit, and a caller that sends the response's commit as `expected_commit_id` gets `EXPECTED_COMMIT_MISMATCH` naming the current one.
- **Logs**: the carried snapshot logs `info` "edits made during a write carried onto its commit" with `workspace_id`, `commit_id`, `files_count` (files the check-out updated) and `conflict`; `warn` instead when the merged tree has conflicts. `SnapshotOutcome` (daemon-internal) gains `carried: bool`, which the in-process hooks and tests read.

### Per write

| Write | How the workspace moves | Late edits |
| --- | --- | --- |
| `EditFiles` rewriting `@` or an ancestor | to the rewritten or rebased commit, same change | carried |
| `EditFiles` with `new_on` | does not move | not affected |
| `DescribeChange` | same tree, never stale | not affected (fresh at its next snapshot) |
| `AbandonChange` of an ancestor | rebased, same change | carried |
| `AbandonChange` resolving a divergent change | from the abandoned commit to the successor, same change | carried: the losing workspace's late edits land on the winning version, as its descendants' content does |
| `AbandonChange` of the workspace's own commit, not divergent | to a new empty change | arm 4: the abandoned change comes back with the late edits and the workspace stays on it; the abandon's empty commit is left without a workspace (open question 1) |
| `UndoOperation` moving the workspace within its change (an edit, a snapshot, a resolving abandon undone) | back to an older commit of the change | carried: the undone state plus the late edits |
| `UndoOperation` moving it to another change (an abandon undone) | back to the other change's commit | arm 4, as for the abandon |

Undo within a change counts although the restored commit is a predecessor, not a rewrite: arm 4 there rewrites the hidden commit into a second visible commit of the change beside the restored one, which is exactly the divergence this issue removes, and Reef knows the move is its own. A move to another change does not count: the late edits were made in a change the write replaced, and carrying them would put them in a change they were not made in, possibly as conflicts against content the write just dropped. Arm 4 keeps them, losslessly, in the change they were made in, and in these cases it does not make a change divergent (above).

### Errors, API and CLI

- No new reasons, no schema change; `API_REVISION` stays 5 and the digest is unchanged.
- Comment rewordings (outside the digest): `EditFiles`, "afterwards those on a rewritten commit take the new tree" gains "; edits a workspace made after its snapshot are carried onto it in the same change, an overlap left as a conflict there"; `AbandonChange`'s and `UndoOperation`'s "take the result afterwards (ADR 0031)" gains "; a workspace that stays on its change keeps edits made after its snapshot, carried onto its new commit (ADR 0039)".
- The CLI and the MCP server do not change: divergence remains possible (two workspaces on one change, a move to another change, a restart, another client), and their notes are about divergence, not about #80. Two code comments that cite issue #80 as the cause (`reef-vcs/src/cli/changes.rs:3`, `scope.rs:359`) cite ADR 0018 instead.

## Deliverables

### `crates/reef-lib/`

- `src/jj/snapshot.rs`: `Carry`; `begin_snapshot(expected_commit, carry: Option<&Carry>, extra, prefixes)` with the carried arm between the adopt arm and arm 4, and its adopt variant when the merge equals `to`'s tree; `SnapshotPlan.carried`; `PendingSnapshot::carried()` and `check_out_carried`; `finish`'s debug assertion; the module and `begin_snapshot` docs list the fifth arm. `Repo::snapshot_unlogged` passes no carry. `jj/mod.rs` re-exports `Carry`.
- Every `begin_snapshot` caller passes `None` for the new parameter (`tests/jj_workspace.rs`, `jj_spike.rs`, `jj_narrowed.rs`).

### `crates/reef-daemon/`

- `src/workspace/mod.rs`: `WorkspaceEntry.carry` and its three methods.
- `src/workspace/rewrite.rs`: `carries`; registration at the start of step 5; `follow` in the plan's order; the module doc's step 5 says the follow carries late edits.
- `src/workspace/snapshot.rs`: the carry taken, passed, restored or replaced; `Pipeline::record`'s order for a carried snapshot; the log line; `SnapshotOutcome.carried` (`src/snapshot/mod.rs`).
- `src/snapshot/mod.rs`: two test hooks beside `hold_scans`, under `cfg(any(test, feature = "testing"))`: `Scheduler::hold_follows() -> FollowHold` with `held_follows()`, awaited by `Manager::rewrite` between step 4 and step 5 (`follow_hold_point`); and `Scheduler::fail_scans(bool)`, which makes every snapshot fail after its hold point and before its scan with an I/O error, as a full disk would.

### `crates/reef-api/` and `crates/reef-vcs/`

- The three proto comment rewordings; `api_digest_matches_revision` passes at revision 5 unchanged.
- The two `reef-vcs` code comments.

### Documentation

- `docs/adr/0039-carrying-edits-through-a-write.md` (accepted), indexed in `docs/adr/README.md`. Context: issue #80, jj's own `update-stale` diverging. Decision: the carry rule and why it is exact, the base, the merge and conflicts, the check-out order and the crash cases, carries for unlocked and failed follows, not persisted, descendants-first follows, which moves carry. Consequences: amends ADR 0018's arm 4 (a fifth arm for Reef's own writes; the arm itself is unchanged for everything else), ADR 0031's step 5 and its consequence on the window, and ADR 0032's consequence on busy workspaces (test 15 now has one commit); ADR 0034 item 10's note now fires for the remaining causes only. Alternatives: the two inferred conditions above; carrying across changes; folding late edits into the write's operation; holding writers off the disk (not possible); persisting carries.
- ADR 0018, 0031 and 0032 each gain one line under their header, `- Amended by: [0039](0039-carrying-edits-through-a-write.md)`, and nothing else (open question 2).
- `crates/reef-daemon/README.md`: the `Manager::rewrite` paragraph (line 42) and the `FileService` one (line 44) say that moved workspaces keep edits made meanwhile, carried onto the new commit, pointing at ADR 0039.
- `CHANGELOG.md` under Unreleased, a new "Fixed" section: "A write that rewrites the commit a busy workspace sits on (`EditFiles`, `AbandonChange`, `UndoOperation`) no longer makes that workspace's change divergent: edits made during the write are carried onto the new commit in the same change, and an overlap with the write is left as a conflict there (#80)."
- `docs/dev-tickets/phase-2/README.md`: the row `| #80 | [issue-80-follow-merges-late-edits.md](issue-80-follow-merges-late-edits.md) | M | R2.03, R2.04 | none listed |`. `docs/dev-phases.md`: unchanged.

## Tests

The daemon tests use `hold_follows` to put the late edit on disk after step 3's scan and before step 5's: take the hold, start the RPC as a task, wait until `held_follows() == 1` (step 4 has committed; poll every 10 ms with a bound, as `narrowed.rs::wait_until` does), write the late edit, drop the hold, await the RPC. "One visible commit" means `ListChanges` with revset `change_id(<reverse hex>)` returns one change, and `divergent()` returns none.

In `crates/reef-lib/tests/jj_workspace.rs`, beside `stale_adopts_when_clean` and `stale_diverges_when_dirty`, with one Reef workspace Y and the fixture's main repository writing `Repo::edit_commit` on Y's commit (which moves Y's working copy) as the write:

1. `stale_carries_onto_the_rewrite`: Y has `a.txt` and `b.txt` snapshotted at commit C; the write changes `a.txt` and adds `api.txt`, giving C'; Y then writes `late.txt` and changes `b.txt`. `begin_snapshot(Some(C), Some(&Carry { from: C, to: C' }), ..)` is `changed`, `carried`, not `adopted`, with `rewrite_of` C' and `divergent` false; `commit_snapshot` is not divergent and keeps the change; `check_out_carried` updates 2 files; after `finish`, Y's disk has the write's `a.txt` and `api.txt` and its own `late.txt` and `b.txt`, the change resolves to exactly one commit whose tree has all four, and a further edit in Y snapshots fresh (no carry, `rewrite_of` the carried commit).
2. `carry_leaves_overlap_as_a_conflict`: the write replaces line `one` with `two` in `a.txt`, Y's late edit replaces it with `three`. The carried commit's tree has a conflict at `a.txt` and none elsewhere; Y's `a.txt` on disk has conflict markers with both `two` and `three`; the change has one visible commit. Writing `resolved\n` to `a.txt` and snapshotting gives a tree without conflicts.
3. `carry_merges_modes_and_deletions`: the write sets the executable bit on `run.sh` and deletes `gone.txt`; Y late changes `run.sh`'s content, changes `gone.txt`, deletes `keep.txt` (which the write left alone), and adds `big.bin` over `snapshot.max-new-file-size` (the fixture's settings with a 1 KiB limit). The carried tree has `run.sh` with Y's content and the bit set (on disk mode `0o755`, `cfg(unix)`), no `keep.txt`, a conflict at `gone.txt`, and no `big.bin`, which `untracked()` reports and which stays on disk.
4. `carry_that_does_not_match_takes_arm_four`: (a) after the write, a second `edit_commit` rewrites C' into C''; Y's snapshot with the carry `{C, C'}` plans `rewrite_of` C and `divergent`, as `stale_diverges_when_dirty`; (b) a carry whose `from` is not the expected commit, likewise; (c) `two_on_one_change` with a carry naming X's snapshot commit as `to` and Y's commit as `from` carries, showing that the carry, not the shape of the change, decides (the daemon never makes such a carry). `stale_diverges_when_dirty` itself is unchanged apart from the `None`.
5. `carry_is_idempotent`: as test 1, but `pending` is dropped after `commit_snapshot` without `check_out_carried` or `finish` (a failed check-out). Y's next `begin_snapshot` with the carry `{C, <the carried commit>, base: Some(<the first plan's disk tree>)}` is `adopted`, not `changed`, checks out the write's files, and writes no operation; the change has one visible commit. Repeat with test 2's overlap: the re-merge is still `adopted` and the commit's tree has the same two-sided conflict at `a.txt`, not a deeper one. (`SnapshotPlan` exposes the disk tree it merged so the daemon can build that carry.)

In `crates/reef-daemon/tests/files.rs`:

6. `edit_carries_late_edits`: W with `a.txt` and `b.txt` snapshotted; at the follow hold of `EditFiles` on W's `@` (no `expected_commit_id`) replacing `a.txt` and adding `api.txt`, W writes `late.txt` and changes `b.txt`. Afterwards W's change has one visible commit, which is W's row's `commit_id`; its tree and W's disk both have the write's `a.txt` and `api.txt` and W's `late.txt` and `b.txt`; W's row has `divergent` false and no attention; `GetStatus` lists no conflicted paths. After the edit's operation, the log has exactly one more operation for W: `SNAPSHOT`, snapshot kind `EXPLICIT`, agent W's owner, with a `REWRITTEN` effect from the response's `change.commit_id` to W's commit. `UndoOperation` of the edit is `OPERATION_SUPERSEDED`; undoing that snapshot and then the edit both succeed, and W's files are as before the edit.
7. `edit_late_overlap_is_a_conflict`: as test 6 with the write and W both changing one line of `a.txt`: one visible commit with `Change.conflict` true, `GetStatus.conflicted_paths` `["a.txt"]`, markers in W's `a.txt`, W's row not divergent.
8. `edit_while_the_workspace_writes` (R2.04's test 15), tightened: the change has exactly one visible commit (assert, and drop the `println!` of the count); every `api/<i>.txt` and the last `busy.txt` value are in it; W's row has `divergent` false and no attention; `divergent()` is empty.
9. `failed_follow_carries_at_the_next_snapshot`: at the follow hold of an `EditFiles` on W's `@`, W writes `late.txt` and the test sets `fail_scans(true)`; the RPC succeeds and W's row still names the commit step 4 started from (watcher snapshots after the unpause fail too, and each restores the carry). With `fail_scans(false)` and a `SnapshotWorkspace` of W, exactly one `SNAPSHOT` operation for W follows the edit's in the log (the explicit one, or a watcher snapshot that ran first, which then leaves the explicit one nothing to record); the change has one visible commit with the write's file and `late.txt`, and W's row is not divergent.

In `crates/reef-daemon/tests/writes.rs`:

10. `abandon_carries_late_edits`: W with `base.txt` snapshotted; V on a new change on W's commit with `child.txt` snapshotted (as test 10, `abandon_change`). At the follow hold of `AbandonChange` of W's `@` from W, V writes `v-late.txt` and W writes `w-late.txt`. V: its change has one visible commit, its disk and tree have `child.txt` and `v-late.txt` and no `base.txt`, its row is not divergent. W (moved to another change, arm 4): W's row is on W's original change, its files have `base.txt` and `w-late.txt`, its row has `divergent` false and no attention, and the abandon's new empty commit is visible with no working copy (pinned, open question 1).
11. `resolving_abandon_carries_the_losers_late_edits`: A and B divergent on one change (as test 11). At the follow hold of `AbandonChange` of B's commit from A, B writes `late-b.txt`. B's files are A's version plus `late-b.txt`; the change has one visible commit, which B's row names; neither row is divergent nor has attention; after `SnapshotWorkspace` of A (idle), A's files have `late-b.txt` too (A adopts the shared change's new commit).
12. `undo_carries_late_edits`: W with `a.txt` snapshotted, then an `EditFiles` X on W's `@` changing `a.txt` and adding `x.txt` (W idle; it follows). At the follow hold of `UndoOperation` of X naming W, W writes `late.txt`. W's files have the original `a.txt`, no `x.txt`, and `late.txt`; the change has one visible commit with that tree; W's row is not divergent.

Unit tests where the code is: `carries` in `rewrite.rs` (same change with a new tree gives a carry; another change, the same tree, a workspace that did not move, and a non-Reef jj workspace give none); the carry slot (`restore_carry` does not replace a newer carry; `take_carry` empties it).

Every existing test passes unchanged except test 8 and the `None` arguments. In particular `writes.rs` 8 to 14 and `stale_diverges_when_dirty` show the arms this ticket keeps.

## Implementation notes

- **Order**: `Carry` and the carried arm with tests 1 to 5; the daemon's slot, registration and consumption, the `Pipeline::record` order and the follow order; the two hooks with tests 6, 7, 9 to 12; then test 8, the comments and the documents.
- The carried arm runs on the workspace's blocking thread like the rest of `begin_snapshot`; `MergedTree::merge` is async and is awaited there through `pollster`, as the scan is. It writes resolved trees to the store before any transaction, which is harmless when the snapshot is then dropped.
- `check_out_carried` takes the committed repository (`SnapshotCommit.repo`) to load the commit, since the `Repo`'s head is set to it only after `record`.
- The follow-up's hold point is after step 4's `call_async` returns and before `self.follow`; with the hook compiled out it is nothing.
- A carried follow can rebase descendants that already followed; with no newer edits they adopt at their next snapshot. Do not try to chain carries for them here (see "Known limits").
- Linux: nothing is `cfg(target_os = "linux")`, but the check-out writes into an overlay's upper layer: add `| (binary(files) & test(/carries/))` to the kernel overlay root filter in `ci.yml`, beside `abandon_change`. Before pushing, run the Linux clippy and that filter in the privileged `ubuntu:24.04` container (`docs/benchmarks.md`, "Linux").

## Acceptance criteria

- [ ] A write that keeps a busy workspace on its change never leaves that change divergent through the workspace's late edits; overlaps are conflicts in the change (tests 1 to 3, 6, 7, 10 to 12).
- [ ] `edit_while_the_workspace_writes` ends with one visible commit and a row that is not divergent (test 8).
- [ ] ADR 0018's arms are unchanged where no carry applies: another client's rewrite, two workspaces on one change, a move to another change (test 4, test 10's W, the existing `stale_diverges_when_dirty` and `abandon_resolves_divergence`).
- [ ] A workspace whose follow failed carries at its next snapshot (tests 5, 9); a crash leaves at worst a divergent change with nothing lost.
- [ ] No schema change: `API_REVISION` 5, digest unchanged, `buf lint` and `buf format --diff --exit-code` pass.
- [ ] ADR 0039 written and indexed, the three "Amended by" lines, the daemon README, CHANGELOG and the phase README updated; `just ci` passes; the Linux container checks pass.

## Verification

```sh
buf format --diff --exit-code && buf lint
cargo nextest run -p reef-api
cargo nextest run -p reef-lib --test jj_workspace --test jj_spike --test jj_narrowed
cargo nextest run -p reef-daemon --lib rewrite
cargo nextest run -p reef-daemon --test files --test writes --test snapshots
cargo nextest run -p reef-daemon --test narrowed --test reads --test presence
just ci
```

## Inherited scope

- **From R2.03**: the five steps, the locks and their order, step 3's descendants-first snapshots, the follow's three outcomes, and the divergent-mark reconciliation, all kept; step 5 gains carries and the descendants-first order. Its risk "The write window" and ADR 0031's consequence are narrowed by ADR 0039.
- **From R2.04**: `EditFiles` through `Manager::rewrite`, the dry run, and test 15, whose bound this ticket asserts.
- **From R2.27 and R2.13**: the divergence note after writes, `reef here`'s advice and the server's `workspace.divergent` text, unchanged.
- **From ADR 0018**: every arm, unchanged when no carry applies.

## Out of scope

- Carrying late edits across a move to another change (open question 1), and abandoning the plain abandon's empty commit when arm 4 moves the workspace back.
- The general case of a workspace on a descendant of another busy workspace's change: an ordinary snapshot of the ancestor rebases the descendant's commit, and the descendant's edits then take arm 4. Only Reef's writes carry here; a follow-up issue should decide whether rebased descendants carry too.
- Persisting carries across a restart; verifying files before jj's check-out overwrites them (upstream jj-lib's TODO).
- Reporting the carried commit in a write's response (`EditFilesResponse.change` stays the write's own commit).

## Known limits

- **An edit landing between the follow's scan and its check-out**, on a path the write changed or that conflicted, would be overwritten by the check-out: jj-lib checks nothing before writing (`local_working_copy.rs:2458`). Added in the implementation after review: the carried snapshot re-stats each file it would overwrite against stamps taken just after the scan, and fails into the re-merge carry (`ChangedAfterScan`) when one changed; what remains is the non-atomic gap around the re-stat, the adopt arm's size. Paths only the agent touches are never written by the check-out.
- **A conflicted carry checked out but not saved** (a crash, or a failed save after the row moved) leaves markers the next scan reads as text (found in review; the adopt arm has the same gap).
- **Undoing the write needs the carried snapshot undone first**: the write's working-copy move is superseded by the carried commit, so `UndoOperation` of the write is `OPERATION_SUPERSEDED` until that snapshot is undone, newest first, as for any snapshot after an edit (test 6).
- **A descendant edited while its ancestor's carried follow runs** takes arm 4 at its next snapshot, since that follow rebases it after its own follow. Descendants-first order makes the window one follow.
- **A restart between a write and the follow** drops carries; the workspace diverges as before.
- **A move to another change** keeps arm 4: the write does not take for that busy workspace, and an abandon leaves its empty commit visible.

## What later tickets inherit

- **Any new write through `Manager::rewrite`** (R3.09's lifecycle kinds, R3.26's undo with conflict objects) gets carries for free when it moves a workspace within its change: step 5 registers them from the before and after working copies.
- **R3.26** may extend carries across changes, or replace arm 4 for them with conflict objects, using the same slot.
- **R2.20 and R2.18** can show a carried snapshot's conflict through `Change.conflict` and `GetStatus.conflicted_paths`; nothing new to read.

## Open questions

1. Should a move to another change carry too (late edits from an abandoned `@` onto the abandon's new empty change; from the empty change onto the restored commit for an undo of an abandon), possibly as conflicts? Default: no, as the issue asks; arm 4 keeps them in the change they were made in, losslessly. A second, smaller question: should arm 4 abandon the empty commit it leaves behind, as jj's `edit` does with a discardable working-copy commit? Default: no.
2. ADR 0018, 0031 and 0032 are accepted, and the ADR README allows only status changes or a superseded note, so this brief adds ADR 0039 and an "Amended by" header line in each instead of editing them as the issue says. Default: that. The alternative is a status of "superseded in part by 0039", which the README's status list does not have.
3. Should the carried follow's operation say so, with an attribute such as `reef.carried-from: <commit>` or a snapshot kind `follow`, so a log reader can tell it from a plain explicit snapshot? Default: no new vocabulary; its `REWRITTEN` effect from the write's commit already shows it.
4. Should a write's response name the carried commit when its follow rewrote the write's commit? Default: no; the response agrees with its operation, and the CLI and MCP re-read the workspace.
