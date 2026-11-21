# Issue #357: another jj client's undo keeps a workspace's files

Phase 3 · Size S · Fixes: issue #357 · Builds on: #344 (ADR 0087, `came_from`), R3.11 (ADR 0084, the operation-graph walk) · Interacts with: #358, #359, #360 (follow-ups from #344; not folded in, see below)

## Context

Reef must never take a file a user wrote off disk. Issue #357 shows that another jj client can still make it do so. An agent's workspace snapshots a file (commit A). Someone at the registered root then runs `jj undo`, `jj op restore`, `jj op revert` or `jj abandon`, and the workspace's next snapshot checks out a commit without the file. The file survives only in the operation log.

The issue also records an order dependence the #344 review found (R4-1). `leads` keeps one `reached` set across operations that ran beside each other, so a move on one branch of the operation graph can count because a move on another branch reached its start. The merge-parent order then decides whether the file stays.

### What was verified while writing this ticket

At main `28c49024` (API revision 43, schema 27, last ADR 0087).

- **It reproduces with the jj CLI.** `repro-cli.sh` (in the ticket run's scratch directory, beside the #344 probe) starts a scratch daemon built from main, registers a fixture repository, creates a workspace, snapshots `w/one.txt` and then `w/two.txt` (A), runs the jj CLI 0.47.1 at the root, and snapshots again with no edits:

  | the other client runs | `w/two.txt` after the snapshot |
  | --- | --- |
  | `jj undo` | refused: "performed in workspace reef-…" |
  | `jj undo --allow-cross-workspace` | gone |
  | `jj op restore <op before A>` | gone |
  | `jj op revert` | gone |
  | `jj abandon <the change>` | gone |

  Each run ends with `row divergent: false` and one visible commit (none after the abandon).
- **A busy workspace loses it too, after `jj undo`.** With `w/three.txt` written after A and not yet snapshotted, `jj undo --allow-cross-workspace` leaves three.txt and removes two.txt. `jj op restore` in the same shape diverges and keeps both files.
- **Reef reads `jj undo` as its own.** The jj CLI describes its undo as `undo: restore to operation <id>`. `oplog::classify` falls back to descriptions for operations without Reef's `reef.kind` attribute, and `UNDO_PREFIX` is that text, copied from jj for the spike's `Repo::undo_last` (tests only). So `jj undo` classifies as `OpKind::Undo`, a Reef write. The daemon's own undo is `reef: undo operation <id>` and carries `reef.kind = undo`. A busy workspace then carries its edits onto the undone commit (ADR 0084): the carry merges the undone tree with the disk over A's tree, so files A added and the agent did not touch since leave the disk.
- **`came_from` counts every move.** ADR 0087 decision 1: "Who wrote the moves does not matter." A clean workspace adopts when the moves since A lead from A to the view's commit, and an undo, restore, revert or abandon starts at A. ADR 0087's Consequences name this case and leave it for this follow-up.
- **jj records where a rewrite came from.** A describe, a rebase, a squash or a split records the old commit as the new one's predecessor in its operation (`Operation::predecessors_for_commit`). An undo, a restore and a revert set an older view and record nothing. An abandon puts the workspace on a new commit with no predecessor.
- **`leads` is order dependent.** It walks the moves in one topological order with one `reached` set. In R4-1's shape (Reef's undo of A on one head, a stale client's describe of the commit before A on the other), the client's move counts when the undo is ordered before it, though the client never saw the undo.

## Root cause

Two mistakes, one per path.

1. A clean workspace's adopt arm (`came_from`) trusts any move that starts at A. A move another jj client made without rewriting A is a jump to some other view, not an edit of the workspace's files, and checking its commit out takes A's files away. A busy workspace's carry already requires Reef's moves, but `classify` reads the jj CLI's undo as Reef's because of a description Reef no longer writes.
2. `leads` asks whether an earlier move in a topological order reached a move's start, where it means whether a move in that operation's own history did.

## Decisions

1. **Another jj client's move counts only when it rewrote the commit.** For a clean workspace, a move in an operation Reef did not write counts only when its operation recorded the new commit as a rewrite of the old one, following commits it wrote and rewrote again (`rewrote`). A merge of operation heads is such an operation, and its rebase records predecessors, so ADR 0087 decision 2 still holds. Reef's own moves count as before: the daemon's undo, abandon and `reef move`, a landing, another workspace's snapshot of the shared change. Otherwise the workspace takes ADR 0018's fourth arm: a rewrite of A, the change divergent, the directory unchanged.
2. **A move counts only when its start was reached in its own operation's history.** `leads` carries a set of reached commits per operation, from A for every operation before A, and takes the union at a merge. The view's commit must be in the head operation's set. This changes the carry (`followed`) too, which uses the same `leads`.
3. **`jj undo` is another client's.** `classify` stops reading `undo: restore to operation` as Reef's undo. Every undo Reef writes has carried `reef.kind` since R2.03; the description fallback only ever matched the jj CLI and the spike's tests. A busy workspace then diverges under `jj undo`, as it does under `jj op restore`, and `reef op log` and the activity feed show the jj CLI's undo as external.
4. **ADR 0089 amends ADR 0087 decision 1.** No API, schema or threat-model change; those slots stay unused.
5. **Not folded in.** #358 (three visible commits after a merge's rebase), #359 (a right-clock rewrite not marked divergent) and #360 (no follow for a foreign move) are other code paths. An abandon by another client now diverges onto a rewrite of the hidden A, so the change has one visible commit while the row says divergent; that is #359's question (count visible commits) from the other side, and is recorded there as a follow-up candidate.

## Tests

- `reef-lib` (`tests/jj_workspace.rs`):
  - a jj CLI undo of `y`'s last snapshot (another client restores the parent view, described as the jj CLI does, no attributes): the clean snapshot keeps two.txt and diverges;
  - the same for `jj op restore` and an abandon by another client;
  - a busy `y` under the jj CLI undo: diverges, two.txt and the new edit on disk;
  - R4-1: Reef's undo of the snapshot on one head, a stale client's describe of the commit before it on the other, merged in both orders: whichever commit the merge keeps, a clean `y` adopts it only when it is the undo's;
  - another client's describe of A still adopts (`a_clean_workspace_follows_a_clients_rewrite_of_its_snapshot`, kept), and Reef's undo still adopts.
- `reef-lib` (`tests/jj_reads.rs`): the classify table's `undo: restore to operation` row becomes external.
- `reef-daemon` (`tests/snapshots.rs`): the CLI reproduction as a test, with another client writing the jj CLI's undo through jj-lib: after the next snapshot two.txt is on disk, the row is divergent with attention `divergent`.
- Mutations, each must fail a committed test: counting every foreign move again; the classify fallback back; one `reached` set again.
- Stress: the #344 probe extended with a foreign undo, an op restore and both merge orders of R4-1, many seeds: main loses files, the fix none. `repro-cli.sh` against the fix: every row keeps two.txt.
