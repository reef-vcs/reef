# Issue #344: a clean workspace adopts only a commit that came from its last snapshot

Phase 3 · Size S · Fixes: issue #344 · Builds on: R1.13 (ADR 0018, the adopt arm), R3.11 (ADR 0084, the operation-graph walk) · Interacts with: #345 (jj's merge of operation heads hangs on clocks far behind; upstream, not this ticket) · Unblocks: nothing listed

## Context

Reef must never take a file a user wrote off disk. Issue #344 shows a case where it does. A workspace with no edits checks out an older commit of its own change, and the file its last snapshot recorded leaves the workspace directory.

1. Another jj client loads the operation head and describes a workspace's commit.
2. The agent writes a file, and Reef snapshots it (commit C2).
3. The client commits. Its clock is behind, so its operation is stamped earlier than the snapshot's.

The daemon's next load merges the two operation heads. jj merges the head stamped later into the one stamped earlier, and for a workspace both heads moved it keeps the earlier head's commit: the client's, made from the commit before C2. The workspace is clean, so ADR 0018's adopt arm checks that commit out. The file stays only in C2, which is still visible, so the change is divergent, but the row says it is not. With a right clock the merge keeps C2 and nothing happens.

### What was verified while writing this ticket

At main `1e2fc4e7` (API revision 42, schema 26, last ADR 0085).

- **The probe reproduces it.** `probe_r9_adopt.rs` (R3.11's tenth review), run against main: right clock `two.txt on disk Some("two")`; clock behind `row before 171a6729 now 5e7e3e51 divergent false ... two.txt on disk None`.
- **A second shape loses it too.** The client rewrites the change *below* the workspace instead of the workspace's own commit. Its rebase moves the workspace onto a rebase of the commit before C2, the merge keeps that, and a clean workspace checks it out: `two.txt on disk None` with the clock behind. With a right clock the merge rebases C2 itself onto the rewritten change, and checking that out is right.
- **The adopt arm trusts any view commit.** `Repo::begin_snapshot` (`reef-lib/src/jj/snapshot.rs`) adopts whenever the scanned tree equals the working copy's old tree. It never asks where the view's commit came from.
- **The question already has an answer in the code.** ADR 0084's `followed` reads the operations since the workspace's last snapshot A from the operation graph (no clock changes the set) and checks two things: every move since A is Reef's, and the moves lead from A to the view's commit, each one starting at a commit that A or an earlier move reached. The second half is exactly "this commit came from A". In the issue's shape, the client's move starts at the commit before A, so it does not lead from A.
- **The stress lever.** `stack_stress.rs`'s rewrite scene waits for every workspace to follow before the client loads the head, because of this bug ("main does the same"). With that wait removed, main fails seed 568 of 600: `workspace 2 lost w2/s568-204.txt on disk: None`.
- **jj records where a merge's rebase came from.** A merge of operation heads rebases a commit whose parent the other head rewrote, and records the old commit as its predecessor in the merge operation (`Operation::predecessors_for_commit`). `followed` skips merges, which is right for a carry (ADR 0084: a merge's rebase diverges a busy workspace) but would turn the right-clock shape above into a divergence for a clean one.

## Root cause

The adopt arm reasons "no edits since the last update, so nothing of the workspace's own is lost". That holds only when the view's commit came from the commit the workspace last snapshotted. A commit made by an operation that never saw that snapshot is older than the workspace's files, and checking it out removes what the snapshot added. jj's merge picks such a commit whenever the other client's clock is behind, and the adopt arm takes it.

## Decisions

1. **Adopt only what came from the last snapshot.** A clean, stale workspace checks the view's commit out when that commit is A, or when the operations since A moved the workspace there from A, whoever wrote them (`came_from`). A merge of operation heads that made the view's commit by its rebase counts as a move from that commit's recorded predecessor. Another jj client's describe of A, another workspace's snapshot of the shared change, a landing, an abandon, an undo and `reef move` all start at A and still adopt.
2. **Otherwise the workspace diverges, with its directory unchanged.** It takes ADR 0018's fourth arm: a rewrite of A, the change marked divergent with attention `divergent`. Refusing instead would leave the workspace stale and unprotected, which ADR 0018 rejected. This is not a product call: ADR 0018 already settles it for a busy workspace, and the issue names it.
3. **When the walk cannot tell, the workspace diverges.** That is more than 1,024 operations since A, or operations garbage collection took. A follow is queued by the move itself, so a clean workspace reaches that only when its snapshots fail for that long. Keeping the files wins over a clean change.
4. **No API, schema or threat-model change.** ADR 0087 amends ADR 0018's decision 3. The API revision, schema and threat-row slots stay unused.
5. **The stress test loses its wait.** The rewrite scene no longer waits for the follows before the client loads the head, so it covers this shape.

## Tests

- `reef-lib` (`tests/jj_workspace.rs`), with the merge order fixed by `merge_in_order`:
  - the issue's shape: the merge keeps the client's describe of the commit before C2, and the snapshot plans a divergent rewrite of C2, with two.txt on disk and in the commit;
  - the change below rewritten, client's head merged into: the same;
  - the change below rewritten, Reef's head merged into: the merge rebased C2, and the clean workspace adopts it (the merge-predecessor rule);
  - another client's rewrite of C2 after loading it: adopted, though the move is not Reef's.
- `reef-daemon` (`tests/snapshots.rs`): the probe as a test. A client with a clock in 2001 describes the commit; after the next snapshot two.txt is on disk, the row is divergent with attention `divergent`, and its commit has the file.
- Mutations: adopting every clean workspace again; dropping the merge predecessors; requiring Reef's moves. Each must fail a test.
- `stack_stress`, 600 seeds on four daemons, wait removed: main fails at least one seed, the fix none.
