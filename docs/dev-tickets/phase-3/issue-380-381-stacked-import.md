# Issues #380 and #381: a stacked workspace keeps the change below

Phase 3 · Size S · Fixes: issues #380, #381 · Builds on: #374 (PR #379, ADR 0090), #368 (PR #373, ADR 0089's amendment), #357 (PR #366, ADR 0089) · Interacts with: #382, #383, #359, #369

## Context

A Reef workspace must never lose files because of something git or another jj client did to the repository. #373 and #379 made that true for a single workspace and for Reef's own import of a deleted branch. Two holes remain, both for a workspace on a change above another workspace's change (W2 above W1):

1. **#380, a branch moved back.** W1 snapshotted `first` (adds `w/one.txt`) and then `last` (adds `w/two.txt`). A git branch on `last` is moved back to `first` (`git branch -f review first`). jj's import records `last` as rewritten into `first` (`record_synthetic_predecessors` in jj-lib 0.47 `git.rs`), and `rebase_descendants` rebases W2's commit onto `first`. W2 follows that rebase, so `w/two.txt` leaves its disk and its commit, and its row says `divergent: false`. #379's pins do not stop it: a pin stops an abandon, not a rewrite.
2. **#381, the jj CLI's import.** Any `jj` command at the root imports git refs first with jj's own rules, which pin nothing for Reef. After the branch on W1's commit is deleted or pruned, that import abandons the commit and rebases W2's commit off it, and W2 follows and loses W1's files. It happened to #374's own brief workspace.

### What was verified while writing this ticket

At main `fe2b1214` (API revision 44, schema 28, last ADR 0090).

- **Both reproduce.** The tests below fail on main: Reef's import of a branch moved back (clean and busy W2), the jj CLI's import of a branch moved back, the jj CLI's import of a deleted branch. Each fails on the first assertion, a lower-change file missing from W2's disk (`w/two.txt stays on disk`, `w/one.txt stays on disk`).
- **The jj CLI's import is its own operation.** With jj 0.47.1 in a colocated scratch repository, `git branch -D` and then `jj st` wrote an operation described `import git refs` that printed `Abandoned 1 commits that are no longer reachable` and `Rebased 1 descendant commits off of commits rewritten from Git`, and removed the abandoned commit's file from the child's checkout.
- **Where it goes wrong.** jj records the rebase with W2's old commit as the new commit's predecessor, as every rebase does. `came_from` (`crates/reef-lib/src/jj/snapshot.rs`) counts another client's move, or Reef's import, when the operation recorded the new commit as made from the old (`origins`). So it counts this rebase, and a clean W2 checks the rebased commit out. A busy W2 under Reef's import goes through `Walk::reefs_move`'s `ImportRefs` arm, which asks the same, and carries its edits onto the rebase. Under another client's operation a busy W2 already diverges (`reefs_move` never counts an external move).
- **Persistent pins do not work.** Keeping a real jj local tag at each Reef workspace's commit (the issue's first option) makes the jj CLI treat those commits as immutable (`tags()` is in jj's default `immutable_heads()`): with a tag on the working-copy commit, the next `jj st` printed `The working-copy commit became immutable; a new commit has been created on top of it`. Every `jj` command then also printed `Warning: Failed to export some tags: "reef:pin:x"@git ... invalid byte: ":"`. Pins also never stop #380's rewrite. The daemon re-pinning after an external operation (option 2) comes too late: the abandon and rebase are already in the view. jj has no setting that protects working copies (option 3; #374's brief).
- **A spike passes.** The fix below, on main: every test below passes, #373's and #379's import tests (deleted branch, moved back, git amend, first describe) pass unchanged, and the reef-lib suite passes except `jj_cli_lists_reef_workspace` (#247). Mutation: with `reefs_move`'s `ImportRefs` arm left on `origins`, the busy test fails (`w/two.txt stays on disk`).

## Root cause

Reef lets a stacked workspace follow any rebase another client or an import recorded for its commit. jj rebases a descendant whenever the commit below is rewritten or abandoned, and records the rebase as made from the descendant, so the record does not say whether anyone decided anything about the workspace's commit. When the commit below was abandoned, or moved back to an earlier commit that is not made from it, no move counts for the lower workspace (ADR 0089 decision 1 and its amendment), yet the upper workspace followed the rebase that move caused.

## Decisions

1. **A rebase counts only when what moved the commit below counts.** Another client's move, or Reef's import's, from `was` to `now` counts toward a workspace's adopt and carry when the operation recorded `now` as made from `was`, as before, and each parent of `was` that `now` no longer has was left by a decision. Either the parent is still visible after the operation (the workspace's own commit was rebased off it: `jj rebase -r`), or the operation rewrote the parent into a parent of `now` and that move counts by the same rule (a describe, a `jj restore` or a git amend of the change below, and its rebase). Otherwise the upper workspace takes ADR 0018's fourth arm: the snapshot rewrites its last snapshot, the row is marked divergent, and its directory stays as it is. `reef_lib::jj::snapshot::rewrote` holds the rule; `came_from` and `Walk::reefs_move`'s `ImportRefs` arm use it.
2. **Every other client's move, not only imports.** *The user's choice, answered in chat on 2026-11-21 (inbox request `cmt_a4990cf386da9882`, withdrawn by the main session after it).* One rule, read from the operation's recorded predecessors and its view, with no matching of jj's operation descriptions. It covers the jj CLI's `import git refs` and `jj git fetch`, and a person's `jj abandon`, `jj op revert` or `jj rebase -r` of the change below. In each a stacked workspace now keeps the lower change's files and diverges where it followed. This reverses the sentence of ADR 0089's Consequences that accepted following "a rebase that a revert gives a workspace stacked above".
3. **Unchanged.** Reef's own moves count as before (`reef_decided`): `reef abandon`, `reef undo`, `reef move` and a landing are requests made through Reef. A merge of operation heads keeps its rule (the merge's rebase is made from the snapshot, ADR 0087 decision 2): the rewrite below it was recorded in one of the merged heads, not in the merge, so the new rule would refuse every merge's rebase. #379's pins, the lower workspace's own moves, and `leads` are unchanged.
4. **ADR.** The rule changes ADR 0089's decision 1 for stacked workspaces, so it gets its own record, ADR 0092, which amends ADR 0089 and ADR 0090. ADR 0090's Consequences read as if only the lower workspace moves when a branch is moved back, and say a stacked workspace still follows the jj CLI's rebase; both bullets are corrected in place to point at ADR 0092. No API, schema or threat-model change: slots 46, 30 and S-276 to S-279 stay unused.

## Scope

- Covered: a workspace stacked above another, clean or busy, when an import by Reef or by the jj CLI rebases it because the commit below went away or moved back, and when another jj client abandons, reverts or rebases away the change below.
- Not covered, other root causes: #382 (after a branch moves back, the lower workspace's first write acts on the earlier snapshot: the lower workspace's `@` resolution, not the follow) and #383 (the divergent flag is a sticky mark). Neither is folded in.
- Not covered: a merge of operation heads that rebased a stacked workspace over another client's abandon in one of the heads (decision 3). It keeps today's behavior.
- Not covered: after the move, both workspaces' changes are divergent and the rebased commit jj made for W2 stays visible. That is ADR 0018's divergence, resolved by abandoning the commit to drop, as for a single workspace since #357.

## Tests

- `reef-daemon` (`tests/snapshots.rs`), each failing on main:
  - **#380, Reef's import**: W1 snapshotted twice, W2 on a new change on W1's last snapshot with `w/b.txt`; `git branch review <last>`, Reef's import, `git branch -f review <first>`, Reef's import, both snapshot: W2 has `w/one.txt`, `w/two.txt` and `w/b.txt` on disk and in its commit, and is divergent; W1 likewise (`a_stacked_workspace_keeps_the_change_below_over_reefs_import_of_a_branch_moved_back`);
  - **#380, busy**: the same with `w/c.txt` written in W2 before the second import; it keeps `w/c.txt` too (`a_busy_stacked_workspace_...`);
  - **#380, the jj CLI's import**: the same imported by the jj CLI (`..._over_the_jj_clis_import_of_a_branch_moved_back`);
  - **#381**: the branch deleted (`git branch -D`) and imported by the jj CLI (`..._over_the_jj_clis_import_of_a_deleted_branch`);
  - **another client's abandon** of W1's last snapshot (`..._over_another_clients_abandon_of_it`).
- `reef-daemon` (`tests/snapshots.rs`), passing on main and after the fix: a git amend of W1's last snapshot, imported by Reef or by the jj CLI, is followed by W2 with the amended file, not divergent (`a_stacked_workspace_follows_*_import_of_a_git_amend_below`); another client's rebase of W2's own commit onto W1's parent is followed, the lower files leave, not divergent (`a_stacked_workspace_follows_another_clients_rebase_of_its_change`).
- The jj CLI's import is emulated in-process, as `jj_undo` emulates `jj undo`: jj-lib's `import_refs` with the repository's `git.*` settings, `rebase_descendants`, described `import git refs`. A run against a scratch daemon with the real jj 0.47.1 CLI checks the same scenes on the shipped binary.
- #366's, #373's and #379's tests in `reef-lib` (`tests/jj_workspace.rs`, `tests/jj_reads.rs`) and `reef-daemon` (`tests/snapshots.rs`, `tests/history.rs`) pass unchanged.
- Mutation: with `rewrote` reduced to `origins`, the five failing tests fail again; with only `reefs_move`'s arm on `origins`, the busy test fails.

## Done

- The tests above, the full suite, `just preflight` and `just doc`.
- ADR 0092; ADR 0089 and ADR 0090 cross-referenced and ADR 0090's Consequences corrected; README "Phase 3 so far" and CHANGELOG Unreleased.
