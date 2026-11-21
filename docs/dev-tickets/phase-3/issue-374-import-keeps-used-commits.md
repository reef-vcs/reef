# Issue #374: Reef's git import keeps the commits workspaces use

Phase 3 · Size S · Fixes: issue #374 · Builds on: #368 (PR #373, ADR 0089's amendment of 2026-11-21), #357 (PR #366, ADR 0089) · Interacts with: #359, #369, #305, #306

## Context

Reef must never take a user's files off disk, and an agent's write must land on its own change. Reef imports git refs before most writes (`refresh_for_resolve` in `crates/reef-daemon/src/workspace/create.rs`, through `Repo::import_git_refs` in `crates/reef-lib/src/jj/git.rs`). The import uses jj's default `git.abandon-unreachable-commits = true`. So when someone deletes, prunes or squash-merges away the git branch on an agent's snapshot, or moves it back, the import abandons commits that workspaces still use. #366 and #373 stopped a single workspace from following that abandon. Three problems remain, all from this one cause:

1. **Stacked workspaces lose the lower change's files, on main as well.** W1 is on change A, W2 on a change above it. When the branch on A's snapshot is deleted, the import abandons A and rebases W2's commit with A recorded as its predecessor. W2 follows the rebase, so A's files leave its disk and its commit, and its row says `divergent: false`.
2. **The first write after the deletion acts on the wrong commit.** The rewrite's step 1 imports, which moves the workspace's `@` to the import's empty commit, and step 4 resolves `@` to that commit. `reef describe` describes the empty commit and leaves the agent's change undescribed. `reef publish` fails with `CHANGE_EMPTY`.
3. **The row says divergent with one visible commit, beside a stray empty head (#359, #369).** `reef here` then tells the agent to abandon a commit, and following that would abandon its only commit.

### What was verified while writing this ticket

At the #373 head `b959fb63` (API revision 44, schema 28, last ADR 0089).

- **jj itself does not protect working copies.** In a colocated repository with the default workspace on A (bookmark `feat`) and a second jj workspace above A, `git branch -D feat` and then any `jj` command printed `Abandoned 2 commits that are no longer reachable`, put `@` on a new empty commit, and removed A's file from the disk (`Added 0 files, modified 0 files, removed 1 files`). The second workspace was rebased off A. jj-lib's `abandon_unreachable_commits` (`git.rs`) pins local bookmarks, local tags and untracked remote refs only. The issue's premise that this "matches what jj protects for its own working copies" is wrong: Reef protects more than jj does.
- **A tracked remote bookmark does not pin.** `reef publish` leaves a tracked `<branch>@origin`. Deleting only the local git branch is enough for the import to abandon the change, even while the remote branch exists.
- **The flag switches two behaviors.** In jj-lib 0.47, `GitImportOptions::abandon_unreachable_commits` also decides whether `record_synthetic_predecessors` records rewrites (`rewrite_commits`, marked TODO upstream). A plain `false`, as the issue proposes, makes a git amend of a snapshot (same jj change id header) a second visible commit of the change instead of a rewrite. Two #373 tests pin that follow (`a_clean_workspace_follows_a_git_amend_of_its_snapshot`, `a_busy_workspace_carries_its_edit_over_a_git_amend_of_its_snapshot`). So the fix keeps jj's import, abandon included, and narrows what it may abandon.
- **Release keeps a non-empty commit visible.** `Repo::forget_workspace` (`crates/reef-lib/src/jj/workspace.rs`) drops the workspace's `wc_commit_ids` entry and abandons the working-copy commit only when it is empty, undescribed and a head. Nothing records which commits an import kept.
- **Reef records no publish.** Only the jj operation `reef: publish change ...` names the branch, and GC drops it after `[gc] undo_window`. `step` never reads bookmarks, so `reef here` says the same before and after a publish.
- **`reef fetch` never abandons.** Its own import replays jj's calls with `abandon_unreachable_commits: false` and leaves a branch the remote lacks as it is (ADR 0071 decision 3). It runs `refresh_for_resolve` first, which this fix covers.
- **A spike passes.** The fix below, with the tests below, on the #373 head: the deleted-branch, stacked and first-describe tests pass, and #373's git-amend and moved-back tests pass unchanged.
- **A branch moved back is a rewrite, not an abandon.** When git moves a branch from the last snapshot back to an earlier snapshot of the same change, jj's import records the last snapshot as rewritten into the earlier one, which a pin does not stop (the same mechanism follows a git amend). The workspace moves to the earlier snapshot, and #373's rule keeps its files by diverging. The spike confirmed it: with pins the workspace still moves there.

## Root cause

Reef handed jj's import the decision of which commits are unreachable, and jj counts only refs. A commit that a workspace is on, or builds on, is in use whether or not git still names it.

## Decisions

1. **Every import Reef runs pins the working-copy commits of Reef's workspaces.** `Repo::import_git_refs` sets a local tag at the working-copy commit of every jj workspace named `reef-<id>`, for the length of the import's transaction, under a name git cannot hold (`reef:pin:<workspace>`), and removes the tags before it commits. jj's abandon then keeps every commit that a Reef workspace is on or descends from, and abandons the rest as before. jj's synthetic rewrites are unchanged, so a git amend is still followed and carried (ADR 0089's amendment). The tags never reach git or the committed view. The user's `git.abandon-unreachable-commits = false` still turns the abandon off.

   The user's own jj workspaces, the root checkout's `default` among them, are not pinned, and the import treats them as the jj CLI does. *Changed after review round 1 (F1), the main session's decision.* The first draft pinned `default` too. Reef never imports git's HEAD after `reef init`, so in a root the user drives with git, `default`'s commit is often stale, and the pin kept commits the user dropped with `git reset --hard`, `git branch -D`, `git rebase` or `git pull --rebase` visible for good, as heads without a bookmark that the history index keeps `OPEN`. With the jj CLI alone, or on main, jj abandons them.
2. **A kept commit stays visible after release.** When the workspace is released, the commit stays visible like any released workspace's change today, until someone abandons it or a later import moves a git ref that reached it. Reef's import then abandons it only if no Reef workspace uses it; the jj CLI's import abandons it even then. *The user's choice, answered in chat on 2026-11-21 (the inbox request was withdrawn after it).* No table records kept commits, and release does not abandon them.
3. **The history index and `reef here` describe it as any open change.** The history index keeps the change `OPEN` (on main the import made it `ABANDONED`), and `reef here` says what it said before the deletion. A publication record and a "branch deleted or merged" row are a follow-up for #305 and #306. *The user's choice, answered in chat on 2026-11-21 (the inbox request had been withdrawn).*
4. **`reef fetch` is unchanged.** Its own import abandons nothing and removes no ref, and its first import is `refresh_for_resolve`, which decision 1 covers. Adding an abandon there would be new behavior outside this bug, and the ticket requires fetch to behave as before.
5. **Unchanged.** `came_from`, `reef_decided`, `Walk::reefs_move` and `leads` keep their rules. In the issue's scenarios they no longer see an import move. They still decide when the jj CLI imports first, which still abandons with jj's default. That case is outside Reef's control and stays as #373 left it.
6. **ADR 0090** records decision 1 and amends ADR 0089's amendment: an import no longer moves a workspace off a commit git stopped reaching. No API, schema or threat-model change; those slots stay unused.

## Scope

- Covered: Reef's own import after a git branch on a Reef workspace's commit, or on a commit below it, is deleted, pruned or reset to another change (`git update-ref`), for clean, busy and stacked workspaces, and the first write after it.
- Not changed: the user's own jj workspaces, the root checkout's included. Reef's import abandons what only they use, as the jj CLI does.
- Not covered: a git branch moved back to an earlier snapshot of the same change. The import's rewrite still moves the workspace there; #373 keeps its files by diverging, and the first write after it acts on the earlier snapshot of the agent's own change. A follow-up.
- Not covered: the jj CLI importing first. It abandons the commit, and #366 and #373 keep a single workspace's files by diverging. A stacked workspace above it still follows the rebase.
- #359 (a right-clock rewrite by another client not marked divergent) and #369 (another client's abandon leaves an empty commit) have other triggers. This fix removes the import's instance of each. Neither issue is fully fixed.

## Tests

- `reef-daemon` (`tests/snapshots.rs`), each failing on main:
  - **stacked**: W1 snapshotted twice, W2 on a change above it with `w/b.txt`, the git branch on W1's snapshot deleted, then both snapshot: both rows on their own commits, one visible commit each, not divergent, every file on disk and in its commit;
  - **first write**: the branch deleted, then `describe_change` on `@` as the first Reef write: the agent's change carries the description and has one visible commit, the row is on it, not divergent;
  - **one-commit divergence**: #368's deleted-branch tests (clean and busy) now assert the workspace stays on its change, not divergent, with one visible commit and attention unset.
- #368's git-amend and moved-back tests pass unchanged. #366's tests in `reef-lib` (`tests/jj_workspace.rs`, `tests/jj_reads.rs`) and `reef-daemon` (`tests/snapshots.rs`) pass unchanged.
- `reef-daemon` (`tests/history.rs::outcomes`): moving `main` back with git after it landed the workspace's change leaves the change `OPEN`, where main abandoned it.
- `reef-lib` (`tests/jj_workspace.rs`): the import keeps a Reef workspace's commit and the commit it builds on after a branch deletion, abandons a commit no Reef workspace uses, and leaves no `reef:pin:` tag in the view (`import_keeps_the_commits_reef_workspaces_use`).
- `reef-lib` (`tests/jj_workspace.rs`): after `reef init` in a git repository, a commit the root checkout builds on and the user drops with `git reset --hard` or `git branch -D` is abandoned by Reef's import, as by the jj CLI's (`import_abandons_what_only_the_root_checkout_uses`, from review round 1's reproduction). It fails with `default` pinned.
- Mutation: without the pins, the stacked, first-write and one-commit tests fail.
- `rev373/flows.sh` (`SCENARIO=describe|publish|stacked|prune`) against a scratch daemon on the fix: no file leaves disk, no row is divergent, and the describe and the publish act on the agent's change.
- `reef fetch`, landing, rebase and undo: the existing `fetch`, `landing`, `landing_push`, `rebase`, `follow_trunk` and `writes` tests pass unchanged.
