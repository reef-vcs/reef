# 0039. Carrying edits through a write

- Status: accepted
- Date: 2026-10-27
- Ticket: issue #80

## Context

`Manager::rewrite` ([0031](0031-writes-over-jj-state.md)) holds still the workspaces a write moves: it snapshots them (step 3), writes (step 4), and has each take its new commit (step 5). It cannot hold the agents still. A workspace whose agent keeps writing files gets edits on disk after step 3's scan, and step 5's follow-up snapshot then meets [0018](0018-snapshot-staleness-and-divergence.md)'s fourth arm: a stale working copy with edits of its own. That arm records the edits as a new commit of the commit the workspace was on before the write, beside the write's. For a write that kept the workspace on its change (`EditFiles` of `@` or an ancestor, [0032](0032-files-without-a-workspace.md); an abandon of an ancestor or one resolving a divergent change; an undo within the change), that is a second visible commit of the change: it diverges. Nothing is lost, but the agent has a divergent change to resolve after an ordinary edit. R2.04's `edit_while_the_workspace_writes`, twenty edits of a workspace's `@` while it writes a file every 5 ms, ended with about fifteen visible commits of one change.

The fourth arm is jj's own behavior: `jj workspace update-stale` in a workspace whose `@` another workspace rewrote, with an edit on disk, leaves the change divergent too (jj 0.46.0). It is right where the rewrite is someone else's: two workspaces on one change keep their own files ([0018](0018-snapshot-staleness-and-divergence.md)). When the rewrite is Reef's own write, Reef knows what moved and why.

## Decision

A write by Reef that moves a workspace to another commit of the same change, with another tree, hands that workspace's next snapshot a **carry**: `reef_lib::jj::Carry { from, to, base }`, the commit the working copy was checked out from, the commit the write put it on, and the tree to measure its edits from (`None`: `from`'s tree). The write registers carries from step 4's working copies before and after it, for every Reef workspace that moved that way, locked or not, in the same repository-thread job as the write, so no snapshot can load the write's head first. A move to another change, or one that kept the tree (a describe), registers none. A locked workspace's `from` is its row's commit when that is in the change: step 3's snapshot of an ancestor rebases a descendant's commit without moving the descendant's row, and the descendant's snapshot expects the row's commit.

A snapshot given a carry uses it only when the working copy is stale with edits that neither adopt condition covers, the view's working-copy commit is `to`, and the commit the caller expects (the row's) is `from`. Otherwise ADR 0018's arms apply unchanged. When it applies, the snapshot's tree is jj's three-way merge of `to`'s tree and the scanned tree over the base, by default `from`'s tree, whose content `to` already has: the late edits are what the disk holds beyond it. It is recorded as a rewrite of `to`: one commit of the same change, not divergent. Late edits the write did not touch, and changes of the write the agent did not touch, resolve; an overlap is a jj conflict in the change, which `GetStatus.conflicted_paths` and `Change.conflict` report; jj's tree merge decides executable bits and deletions. A new file over `snapshot.max-new-file-size` is left out of the scan, as always. When the merge is `to`'s tree, the snapshot checks `to` out and records nothing, as the adopt arm does.

The merged commit holds the write's changes, which are not on disk, so a carried snapshot checks its new commit out after the commit and before the row and the working-copy state are written: commit, check out, row, save. jj's check-out overwrites the files it updates without reading them, and between the scan and the check-out the commit waits for the repository thread, so the snapshot first re-stats each file the check-out would overwrite or remove (size, modification time, and on Unix inode and change time) against what it saw just after the scan; one that changed fails the check-out (`ChangedAfterScan`) with nothing written. A crash before the check-out leaves the row on `from` and, with carries in memory only, the next snapshot takes the fourth arm: the change diverges, as before this decision, and nothing is lost. A crash after it leaves files that already are the commit's, which the next snapshot adopts, when the merge has no conflict (see Consequences). A check-out that fails, or finds a file changed, sets a new carry `{ from, to: <the merged commit>, base: <the disk tree merged> }`: merging again over the tree already taken gives the merged commit back. The base must be that tree, not the recorded one; merging a conflicted result over the recorded tree again would add the late edit's side twice, which jj does not simplify.

The carry slot is per workspace, in memory, set by a write and taken by the next snapshot of any kind, under the scan lock and the jj handle's lock; a snapshot that fails before committing puts it back unless a newer write set one. Step 5 follows workspaces descendants first, as step 3 snapshots them: an ancestor's carried follow rewrites its commit and rebases its descendants, and done first it would move a descendant off its carry's `to`.

The carried snapshot is one `snapshot` operation for the workspace, of kind `explicit` when it is step 5's follow-up, as step 5 recorded before (else the kind of the snapshot that carried), with the write's commit `REWRITTEN` into the merged one. No new attribute, snapshot kind or error reason, and no schema change: the API stays at revision 5. A write's response still names the commit the write made, so it agrees with its operation; a carried follow has rewritten that commit by the time the response arrives.

## Consequences

- ADR 0018's fourth arm gains a sibling for Reef's own writes; the arm itself is unchanged for everything else (another client's rewrite, two workspaces on one change, a move to another change, a restart between a write and its follow-up).
- ADR 0031's step 5 now carries late edits, and its consequence on the window between step 3 and step 5 narrows to the cases above. ADR 0032's busy-workspace consequence holds with one commit: `edit_while_the_workspace_writes` asserts it.
- ADR 0034's note after a divergent write, and `reef here`'s advice, fire only for the remaining causes.
- Undoing a write that carried needs the carried snapshot undone first, newest first, as for any snapshot after an edit.
- An edit that lands on a path the write changed between the re-stat and jj's check-out of that path is overwritten: the check is not atomic with the write. The window is the adopt arm's, whose scan-to-check-out gap has no check at all. An edit in the gap between jj's scan and the stamps taken just after it is missed the same way.
- A conflicted carry whose check-out succeeded but whose working-copy state was not saved (a crash, or a failed save after the row moved) leaves conflict markers on disk that the next scan reads as text: jj materializes conflicts at check-out and parses them back only for paths its saved state records as conflicted. The change then diverges with the markers as text, or, when the row had moved, takes them as a resolution. The adopt arm has the same gap for a conflicted commit it checks out ([#92](https://github.com/reef-vcs/reef/issues/92)).
- A descendant edited while its ancestor's carried follow runs still diverges at its next snapshot: that follow rebases it after its own follow. Only Reef's writes carry; an ordinary snapshot that rebases a busy descendant is a follow-up ([#91](https://github.com/reef-vcs/reef/issues/91)).
- Abandoning a workspace's own change while it writes keeps the fourth arm: the change comes back with the late edits, and the abandon's new empty change is left without a workspace.
- Any later write through `Manager::rewrite` that moves workspaces within their change carries without new code.

## Alternatives considered

**Infer the condition from the change.** Carry whenever the view's commit is in the same change as the expected one, or descends from it in the evolution log. Either would also carry another workspace's snapshot of a shared change, silently undoing ADR 0018's rule, and the second misses undo, which restores a predecessor, and resolving abandons, whose survivor is a sibling. The carry names exactly the move Reef made.

**Carry across changes.** Put an abandoned `@`'s late edits on the abandon's new empty change, or an undone abandon's on the restored commit. They were made in a change the write replaced; carrying them would put them where they were not made, possibly as conflicts with content the write just dropped. The fourth arm keeps them, losslessly, where they were made, and does not make a change divergent in these cases.

**Fold the late edits into the write's operation.** They reach the disk after step 3's scan, so the write cannot have seen them.

**Hold the agents off the disk.** Not possible: a workspace is an ordinary directory.

**Persist carries.** A restart between a write and its follow-up's snapshot is rare, and the fallback is today's divergence with nothing lost; persisting the slot would mean a schema migration for that window.
