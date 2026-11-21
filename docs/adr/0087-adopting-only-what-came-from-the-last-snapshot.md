# 0087. Adopting only what came from the last snapshot

- Status: accepted
- Date: 2026-11-20
- Ticket: [#344](https://github.com/reef-vcs/reef/issues/344) ([brief](../dev-tickets/phase-3/issue-344-adopt-clock-skew.md))
- Amends: [0018](0018-snapshot-staleness-and-divergence.md) (decision 3), [0084](0084-dependent-change-lifecycle.md) (the walk also runs for every clean, stale snapshot)
- Amended by: [0089](0089-another-clients-undo-keeps-a-workspaces-files.md) (decision 1: another jj client's move counts only when it rewrote the commit, and a move only when its own operation's history reached its start)

## Context

[0018](0018-snapshot-staleness-and-divergence.md)'s third arm checks the view's working-copy commit out on disk when the working copy is stale and has no edits since its last update: "Nothing of the workspace's own is lost, so following the rewrite is correct." That is true only when the view's commit came from the commit the workspace last snapshotted.

Issue #344 shows a commit that did not. Another jj client loads the operation head, Reef then snapshots a new file in a workspace (commit A), and the client commits a describe of the workspace's commit as it was before A. When the daemon loads the head it merges the two operation heads. jj merges the head stamped later into the one stamped earlier, and for a workspace both heads moved it keeps the commit of the head it merges into. A client whose clock is behind wins: the view now has the client's commit, made before the file. The workspace is clean, so the third arm checks that commit out and the file leaves the directory. It survives only in A, which stays visible, so the change is divergent while the workspace row says it is not.

A client that rewrites the change below the workspace does the same: its rebase moves the workspace onto a rebase of the commit before A, and the merge keeps that.

[0084](0084-dependent-change-lifecycle.md)'s walk already answers where the view's commit came from. It reads the operations since A from the operation graph, so no clock changes the set, and its second condition asks whether the moves among them lead from A to the view's commit. A carry needs a first condition as well, that every move is Reef's. Adopting does not: the workspace has nothing on disk to merge, so another client's rewrite of A is as safe to follow as Reef's.

## Decision

1. **A clean, stale workspace checks the view's commit out only when that commit came from A**, the commit the workspace last snapshotted (its row's commit): it is A, or the operations since A moved the workspace there from A, each move starting at a commit that A or an earlier move reached (`reef_lib::jj::snapshot::came_from`). Who wrote the moves does not matter. Another client's describe of A, another workspace's snapshot of the shared change, a landing, an abandon, an undo and `reef move` all start at A.
2. **A merge of operation heads can make the commit.** A merge whose view has a commit for the workspace that none of its heads had made that commit by its rebase. It is a move from each predecessor jj recorded for the commit in the merge operation (`Operation::predecessors_for_commit`). A merge of three heads or more rebases after each head it merges in, so a predecessor can be a commit the merge wrote itself and rebased again; the move is then from that commit's predecessors. So when the merge keeps A and rebases it onto a change the other client rewrote, the workspace follows the rebase. A merge that keeps one head's commit is no move: that head's own operations moved it, and they are judged where they were written. The carry still skips merges, as [0084](0084-dependent-change-lifecycle.md) decided.
3. **Any other commit takes the fourth arm.** The snapshot plans a rewrite of A, the change is marked divergent with attention `divergent`, and the directory stays as it is. Refusing to snapshot was rejected in [0018](0018-snapshot-staleness-and-divergence.md): it leaves the workspace's files unprotected.
4. **No operation limit, and the workspace diverges only when the walk cannot tell.** The carry gives up after 1,024 operations since A ([0084](0084-dependent-change-lifecycle.md)), because a follow is queued by Reef's own move. A move another jj client writes queues nothing: an idle workspace meets it at its next snapshot, at the latest the fallback timer's (10 minutes for a watched workspace), and a busy repository can write any number of operations in between. The walk has read every one of them before a limit would count, so `came_from` judges them all (`a_clean_workspace_follows_a_rewrite_past_the_carry_limit`). It cannot tell when the store no longer has an operation it needs (garbage collection took it), or the workspace was forgotten and added again since A, and then the workspace diverges with its files on disk.
5. The third arm's other case is unchanged: a working copy whose files already are the view commit's tree (its own snapshot committed and its state not saved) checks it out.

## Consequences

- A clean workspace never loses a file its last snapshot recorded to a commit made before that snapshot.
- Where main lost the file, the change is now divergent with both commits visible, and the workspace says so. When the merge also rebased A (the client rewrote the change below), the fourth arm rewrites A on its old parent, so the change has three visible commits and the change below two. Nothing is lost, and resolving the divergence keeps the one to keep.
- A stale, clean snapshot now reads A (the row's commit) before it decides. If the store no longer has A, the snapshot fails with `ExpectedCommitMissing` where main checked the view's commit out. Diverging needs A too, so there is nothing else to do; it takes a row whose commit garbage collection pruned while the workspace never snapshotted. An error reading the operation log, other than an operation the store no longer has, fails the snapshot the same way, with the directory as it was.
- An undo, an operation restore, an abandon or `reef move` that starts at A still takes A's files off disk, as on main: each is a request to leave A, and A stays in the operation log. A `jj undo` on the registered root undoes whatever operation is last, which may be an agent's snapshot; requiring that A be in the view commit's evolution for a move Reef did not write would make that diverge instead. That is left for a follow-up.
- A stale, clean snapshot reads the operations since A once more. After a move of Reef's the follow runs at once, so that is a few operations (0.7 ms, [0084](0084-dependent-change-lifecycle.md)). After another client's move it is every operation since, read with its view: 0.60 s for 10,000 ([0084](0084-dependent-change-lifecycle.md)'s figure for the same walk).
- `stack_stress`'s rewrite scene no longer waits for every workspace to follow before the other client loads the head. With the wait removed, main failed 1 of seeds 1 to 600, and this code none.

## Alternatives considered

- **Requiring Reef's moves, as the carry does.** Another client's rewrite of A is a legitimate edit, and a clean workspace has no edits to keep. `stale_adopts_when_clean` (two workspaces on one change) fails with that rule.
- **Comparing stamps.** Adopting only a commit written after A by its operation's end time is the clock heuristic the issue rules out: the client's stamp is what was wrong.
- **Keeping A in the view during the merge.** The merge of operation heads is jj's, inside `load_at_head`. Reef could write a correcting operation after it, but the snapshot that decides whether to check anything out is where Reef first acts on the merged view, and it already reads the graph.
- **Adopting when A is no longer visible.** A visible A means no operation rewrote it, but a landing that leaves A on trunk, a `reef move` to another change and `jj new` all move a workspace off a visible A on purpose.
