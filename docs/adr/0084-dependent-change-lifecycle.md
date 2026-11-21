# 0084. Dependent change lifecycle

- Status: accepted
- Date: 2026-11-19
- Ticket: R3.11 ([brief](../dev-tickets/phase-3/R3.11-dependent-change-lifecycle.md)); fixes [#91](https://github.com/reef-vcs/reef/issues/91) and [#202](https://github.com/reef-vcs/reef/issues/202)
- Amended by: [0087](0087-adopting-only-what-came-from-the-last-snapshot.md) (the walk's second condition also decides ADR 0018's adopt arm, for every clean, stale snapshot, with no operation limit), [0089](0089-another-clients-undo-keeps-a-workspaces-files.md) (a move counts only when its own operation's history reached its start)

## Context

The [design](../design.md)'s Stacks paragraph says what happens to a dependent when the change below it moves. Amending a change rebases its dependents and keeps their IDs. A dependent that no longer applies carries a conflict object. A declined or withdrawn dependency returns each dependent to its owner, or to the resolver queue. A superseded one moves its dependents onto the superseding change. Phase 3's exit criterion asks for the first two.

jj already rebases a change's descendants in the operation that rewrites it, keeps their change ids, and records a conflict in a commit that no longer applies. [0065](0065-conflict-objects.md) reads that as a `tree` conflict, and [0068](0068-resolver-assignment.md) assigns it to the dependent's owner. What was missing was everything an agent working on a dependent needs around that:

- A dependent held in another workspace kept its old files after an ancestor's snapshot (#202). One with edits not yet snapshotted diverged its change (#91), because a snapshot hands out no carries ([0039](0039-carrying-edits-through-a-write.md)).
- Every rewrite below a proposal left it `amended`, so `reef land` refused it until its agent proposed it again. Under `[history] message = "summary"`, that happened after every landing.
- A declined dependency was silent. `reef land` on the dependent answered `NOT_PROPOSED` with `next: reef propose <dependent>`, which put the declined change back in review.
- Supersede moved nothing.

## Decision

1. **Descendant workspaces follow an ancestor's snapshot.** In the repository-thread job that commits a snapshot, the daemon compares the working copies before and after. Every other Reef workspace on another change whose commit the snapshot's rebase moved is marked as owed a follow, and is snapshotted after the ancestor's snapshot, descendants first:
   - an idle workspace checks the new tree out (#202);
   - a busy one carries its edits onto the rebased commit, in the same change (#91);
   - another workspace on the same change still diverges, as [0018](0018-snapshot-staleness-and-divergence.md) says.

   **A carry is read from the operation log. Nothing is handed to the workspace.** This amends [0039](0039-carrying-edits-through-a-write.md), whose carry was a value the mover put in a slot of the moved workspace. A snapshot now asks one question: did Reef move this workspace's working-copy commit from commit A to the commit the view has for it? It asks at the two places where the answer decides between a merge and a divergent change:
   - **After the scan**, when the working copy is stale and has edits. A is the commit the workspace's row names. If Reef moved it, the snapshot records [0039](0039-carrying-edits-through-a-write.md)'s three-way merge as a rewrite of the view's commit. If not, it diverges ([0018](0018-snapshot-staleness-and-divergence.md)).
   - **In the repository-thread job that commits**, when the view's commit is no longer the one the scan planned to rewrite. A is the planned commit. If Reef moved it, the job rewrites the view's commit with the three-way merge of the rebased tree and the scanned tree over the planned commit's tree. The check-out then writes the ancestor's files, after checking that each file it overwrites still holds, byte for byte, what the scan read; after a watcher's narrowed scan that includes the files the scan itself recorded.

   The answer is a function of the head the snapshot holds (`reef_lib::jj::snapshot::followed`), and it does not depend on the shape of the operation log. It reads every operation **since A**: each operation in the head's history that is in the history of no operation whose view has A for the workspace. Among them, an operation with one parent whose view has another commit for the workspace than its parent's **moved** it. Reef moved the workspace from A to the view's commit when both of these hold:
   - **Every move since A is Reef's.** The operation is one Reef wrote, by its `reef.kind`. It kept the workspace on its change, or put it on a new empty change on that change (a landing, [0071](0071-rebasing-and-following-trunk.md)). The tree does not matter: a describe, or an ancestor that adds a descendant's own file with the same bytes, is a move like any other.
   - **The moves lead from A to the view's commit**, each one starting at a commit that A or an earlier move reached.

   These moves are not Reef's, and the snapshot diverges as before:
   - another jj client's operation;
   - another workspace's snapshot of the same change, told by its `reef.workspace` ([0018](0018-snapshot-staleness-and-divergence.md));
   - the workspace's own snapshot, when its row was never written (a crash between the commit and the row);
   - a move with more than 1,024 operations since A, or with one of them among the operations garbage collection took.

   **A merge of operation heads is no move, whatever its shape.** A jj client that runs on the registered root while the daemon writes leaves an operation head beside the daemon's, and the daemon's next load merges the heads ([0018](0018-snapshot-staleness-and-divergence.md), arm 2). Two clients leave three heads, merged in one operation. A second jj process that merges the same heads leaves two merges, and the merge of those has two closest common ancestors. None of this is read: the operations on every head are since A or they are not, and each move among them is judged where it was written. A client that loaded the head before A was written and committed afterwards is since A too. Its head has an older commit for the workspace, and it moved nothing.

   Two shapes with a merge in them diverge. Each is another client's move:
   - **Another client moved the workspace's commit on any head.** This holds even when Reef moved it on another head and the merge kept Reef's commit. jj keeps the commit of the head it merges into when two heads moved it, so which commit the view has is an accident of the order of the heads. The slot [0039](0039-carrying-edits-through-a-write.md) used carried in the one order and diverged in the other. The change is divergent in both, because the client's rewrite stays a visible commit of it, so a carry would not have kept it whole. Reading every head gives one answer for both orders.
   - **The merge left the workspace on a commit that no head has.** The merge rebased Reef's commit over another client's rewrite of a commit below it. No move leads to that commit. The slot diverged here too: it named the commit Reef's move made.

   **Which operations are since A is read from the graph, and no clock changes it.** The log is read back from the head. An operation whose view has A is before A, and so is its whole history, including any part of it that was read earlier and taken to be since A. The history of those operations is read on, without views, until every operation still unread is in the history of each operation found to be since A. An operation is never in its own history, so none of those found can then turn out to be before A. The order of the reads follows the end times, as jj's own log does. That order only decides what is read: with right clocks, an operation before A is reached after the operation that shows it is, and its view is not read. Operations stamped by a clock that was wrong, or two in one millisecond, cost more reads and give the same answer. The 1,024 counts the operations since A. Those before A do not count, however many of them a client that loaded the head early leaves between its load and A.

   Two things stay in memory, and each has one owner. The follow mark is set by the commit that moves a workspace and cleared by the next job that loads a head for that workspace's snapshot, both on the repository thread. It only saves a second follow. The commit of a carried snapshot whose check-out failed is kept by the workspace with the tree that snapshot scanned, and read by the workspace's next snapshot under its own scan lock. It measures the edits from that tree, so they are not taken twice, when the walk finds that commit written by the workspace's own snapshot with no undo after it.

   A write's follow ([0031](0031-writes-over-jj-state.md) step 5) updates only the row of a workspace whose row was on the commit the write moved and whose tree it kept. Any other locked workspace it moved is snapshotted, since its files are its row's commit's. A snapshot that commits while a write holds a descendant marks and queues that descendant's follow like any other. The follow waits for the write to let go, so a commit between the write and the write's own follow is not missed.
2. **A proposal follows its dependency when its own work did not change.** A `proposed` change is **carried** to its visible commit when that commit meets all of these:
   - its parents are new commits of the recorded commit's parent changes (or of a superseded parent's successor);
   - it has no conflict;
   - rebasing the recorded commit onto those parents gives exactly its tree and description.

   The daemon records a `proposed → proposed` transition at the new commit, with cause `dependency` (`TRANSITION_CAUSE_DEPENDENCY`), actor `daemon` and agent `reef-lifecycle`. `ChangeTransition.dependency` names the parent change and what happened to it: `amended`, `landed` or `superseded`. A follower woken by every operation does this, and `LandChange` does it first.

   A verdict that does not carry is a function of the two commits and the superseded changes, so the daemon remembers it, and a later look checks that pair at no cost. Only checks that may still carry count toward a look's limit. A recorded commit that garbage collection took does not carry and does not hold a stack. It is logged once, and the other rows are read as before. `LandChange` carries its stack's members first, with no limit. A stack proposal judges a declined or withdrawn member the same way, under the lifecycle lock, without waiting for the follower.

   Evidence and sign-offs bind to trees and do not follow. A change moved by its own `reef rebase` onto other parent changes is not carried ([0071](0071-rebasing-and-following-trunk.md)'s decision 6 stands). A declined or withdrawn change is carried the same way (`declined → declined`), so a rewrite below it is never mistaken for its owner's revision. The automatic service and a stack proposal carry first too.
3. **A dependent whose dependency left review returns to its owner, still proposed.** A change below a proposal that was declined or withdrawn at its current commit is a **missing dependency**. It is derived on every read from the stack ([0078](0078-stacks.md)) and the lifecycle rows, never stored, and it brings five things:
   - **The conflict object.** A `missing_dependency` conflict (`CONFLICT_KIND_MISSING_DEPENDENCY`, `Conflict.dependency`). Its id hashes the dependent, the missing change and the transition that took it out: the missing change's latest transition other than a carry, so a rewrite below it is not a new decline. `ListConflicts` lists these first; `GetConflict` with `kind` reads one.
   - **The assignment.** An R3.27 assignment of kind `dependency`, with the owner rules unchanged: the newest live session that recorded the dependent, else after the grace the resolver queue, and reopened within R3.27's window. It has no tier and no attempt cap. It closes `resolved` when the conflict goes away, and `withdrawn` (a new reason) when the dependent leaves review. A failed read closes nothing, and neither does a dependent whose stack cannot be read (divergent, a destination that is gone, a stack too large). A declined change that turns divergent still holds the changes above whichever of its commits is still the declined one. Lookups of a change's assignment by change alone (claims, steps, help requests, resolutions) read its `tree` assignment.
   - **Landing is held.** A `QUEUED` dependent is dequeued with reason `dependency`. `LandChange` refuses it with `DEPENDENCY_LEFT_REVIEW`, and its `next` moves the changes above the missing one off it, past every member above it that left review too: `reef rebase <change above them> --source --onto <the change below, or the destination>`.
   - **The agent's next step.** `reef here` has the step `dependency` (`CHANGE_STEP_DEPENDENCY`, `Workspace.missing_dependency`).
   - **Nothing is moved.** Reef never moves the dependent off by itself.
4. **A stack proposal does not put back a member that left review.** `reef propose` of a stack refuses a member below the revision that is declined or withdrawn at its current commit (`MEMBER_LEFT_REVIEW`). Proposing it alone, or after revising it, still works.
5. **The dependent's agent is told.** The daemon writes one instruction, on every destination, through [0081](0081-automatic-landing.md)'s writer: fixed templates over ids, and none while another of the daemon's is unfinished on the change. It goes to the session that proposed the dependent when its dependency leaves review, or when a rewrite below leaves it conflicted. A clean carry writes nothing. The same text is not written again since the dependent's latest transition other than a carry, nor since the dependency left review.
6. **Supersede moves the dependents.** `TransitionChange` to `SUPERSEDED` rebases the superseded change's mutable children (a new `RebaseMode::Children`) and their descendants onto the superseding change. The rebase and the transition are one operation of kind `transition`, written through `Manager::rewrite`, so the dependents' workspaces follow. Both changes are named by change id and resolved in the repository-thread job that uses them, so a snapshot of either one's workspace at any point changes nothing: the dependents are the children of the commit the superseded change has when the write plans. If the rebase is refused before it writes (a divergent change among them, too many to move), the supersede is recorded alone and `moved` reports each dependent with `rebase_refused`. A supersede never answers with no dependents while the superseded change has mutable children. Dependents that still apply are carried at once with event `superseded`, each naming the successor, its children's children too. The supersede records those carries under the lifecycle lock it took, so the carry follower cannot record one first, and `moved[].carried` says what happened. `UndoOperation` of that operation reverts the rebase and records the inverse transition in one operation of kind `undo`, its workspaces following. A child the successor descends from is left, and `TransitionChangeResponse.moved` reports it.
7. **Bounds.** Schema 25 (`0027_dependent_lifecycle`: two columns on `lifecycle_transitions`), API revision 41, S-243 to S-249. The carry follower checks at most 64 moved proposals per look; missing dependencies cost one query when nothing was ever declined or withdrawn, else one index read before any stack is read. The daemon's instructions go out from the carry follower, outside every lock.

## Why the carry is derived

Five review rounds each found one more interleaving in which a descendant diverged or missed files. Each was in the same mechanism: a carry put in a per-workspace slot by the mover (set, join, take, give back) while snapshots, writes, pauses and ancestor commits interleaved. Each fix closed one ordering: a second carry replaced the first; a snapshot raced the ancestor's commit; the head was loaded before a joined carry; a same-tree rebase handed out no carry; a paused workspace was skipped.

Every one of those fixes assumed that a workspace learns how its commit moved from what the mover hands it. We tested that assumption instead of patching a sixth ordering.

**What the slot held, and where each part is read now.**

| In the slot | Read from |
| --- | --- |
| `from` and `to` | The row's commit (or the plan's) and the view's commit at the head the snapshot holds. `begin_snapshot` already compared the carry with exactly these two. |
| That Reef made the move | Each operation's `reef.kind` and `reef.workspace`. This is the one fact the two commits do not give. [0039](0039-carrying-edits-through-a-write.md) rejected inferring the carry from the change for that reason: another workspace's snapshot of a shared change and a Reef edit of it look the same in the commit graph. The operation log tells them apart. |
| Two carries joined | The walk crosses every operation that moved the workspace. |
| A carry for a move that kept the tree | The walk does not read trees. |
| The carry taken in the job that loads the head | The answer is a function of that head. |
| No carry for a paused workspace | There is no carry to withhold. |
| A landing onto a new empty change | The same shape test, made per operation. |
| The tree a failed check-out had merged (`base`) | Not in the repository. It stays in memory, written and read only by the workspace's own snapshots. |

The stamps of a narrowed scan were never in the slot: they belong to the snapshot in flight. A working copy ahead of the daemon's head (the jj CLI) reloads the head and asks again.

**Evidence.** `crates/reef-daemon/tests/stack_stress.rs` drives a stack of two or three workspaces through seeded scripts of agent writes, snapshots, and describes and edits by change id, with the daemon's hold hooks taken and released between them. Another jj client loads the head and commits later, so the daemon merges its operation with its own. Half the seeds end by superseding the bottom change while the workspaces above write, and in half of those a snapshot of the superseded change's workspace is held between its scan and its commit when the supersede starts. After each scene it checks that no change diverged, no agent's bytes were lost, and every workspace holds the files of the changes below it before any snapshot of it is asked for. A supersede that does not move the workspaces above fails the seed.

| Code | Seeds 1 to 30 that fail |
| --- | --- |
| The slot, as reviewed (29b863f4) | 7: five workspaces that never follow, two divergent changes |
| The slot, without the commit-time merge (second review, 1b) | 10, eight of them divergent |
| The slot, without joined carries (second review, 1a) | 12, eleven of them divergent |
| Derived | 0 |

The three rows for the slot were measured before the supersede scene was added. The derived carry then passed seeds 1 to 1,500 with it, in 28 minutes on four daemons. A plain test run covers 12 seeds; `REEF_STACK_STRESS_SEEDS` sets the count.

The seventh review found what that test could not see: it ran no other jj client, and it accepted a supersede that moved nothing. With the other client and the stricter supersede check added, the derived carry as first written (8db3762b) failed 55 of seeds 1 to 120:

| Failure on 8db3762b | Seeds |
| --- | --- |
| A change diverged at a merge of operation heads, which the walk refused outright. In 13 of them the script's next write by change id then failed as ambiguous | 33 |
| The supersede refused the rebase because the superseded commit had moved, and left the workspaces above | 21 |
| The supersede named no dependent and left the workspaces above | 1 |

With a rule for a merge of two heads and decision 6's reading by change id (628642f7), seeds 1 to 600 passed, in 11 minutes on four daemons. That run merged operation heads 803 times and superseded 317 times.

The eighth review found the next shape. That rule followed one line back through the log and chose a side at each merge. It refused a merge of three heads, which two jj clients beside one write of the daemon's leave, and a merge of two merges with two closest common ancestors. The slot, which never read the log, had carried in both. Seven rounds had each fixed one shape of a mechanism, in the slot and then in the walk, so this round tested the walk's premise instead: that the answer has to be found along one line through the graph. It does not. Whether only Reef moved the commit is a property of the set of operations since A, and decision 1 now reads that set. The rule has no case for a merge, so there is no shape left to add.

The stress test changed with it, because it could not have failed on those shapes or on the opposite fault:

- Other clients load the head one or two at a time, so the daemon merges three heads.
- A quarter of the seeds end with another client rewriting the commit of a workspace whose agent has an edit that no snapshot took, beside a write of the daemon's elsewhere and a third client's bookmark. That change must come out divergent, with the agent's bytes on disk and in its row's commit. A walk that carried across another client's move, through the merge, fails the seed.
- A supersede that is meant to race a snapshot of the superseded change's workspace releases every hold first and waits until that snapshot is at its commit hold. Before, another workspace's commit hold let the snapshot commit at once.
- A run of 12 seeds or more fails unless it merged two heads, merged three or more, superseded, superseded while a snapshot was held, and had a commit rewritten by another client.

| Code | Seeds 1 to 120 that fail |
| --- | --- |
| The walk along one line (628642f7) | 15, each a change that diverged at a merge of three heads. In six of them the script's next write by change id then failed as ambiguous, and in one a supersede was refused |
| The set of operations since A | 0 |
| The set, with another client's operation counted as Reef's (seeds 1 to 40) | 12 of the 13 seeds that end with another client's rewrite |

Seeds 1 to 600 pass on the set, in 11 minutes on four daemons. That run merged operation heads 1,161 times, 479 of them of three heads or more, superseded 308 times, 158 of them while a snapshot was held, and had 156 commits rewritten by another client.

The ninth review found that the set was right and the way the walk found it was not. The walk read operations latest end time first, and took one to be since A unless an operation read earlier had shown it was before A. So the answer depended on the clocks. An operation stamped later than its child, by a clock that was ahead or in the same millisecond with a larger id, was read first and judged with the history below it, and the follow diverged. The walk's limit also counted the operations before A that it read, so a client that had loaded the head more than 1,024 operations before A made the follow diverge. The slot had carried in both cases. The walk now finds the set from the graph, as decision 1 says, and its limit counts the operations in the set.

The stress test could not have failed on either fault, so it changed again:

- The other clients of a seed share a clock. In two seeds of three it is wrong: an hour ahead, or stopped when the seed began, so that their operations fall in one millisecond. The operation they load the head at is stamped by that clock too.
- Every seed's first scene ends with a client that loaded the head too early. A workspace snapshots, a write by change id moves it, its agent writes, and the client commits before the write's follow-ups.
- Each daemon keeps one client from an earlier seed, and uses it as that stale client once its log has 1,100 operations more.
- A run of 12 seeds or more fails unless it had a stale client with each wrong clock. A run of 60 seeds or more for each daemon fails unless one such client had loaded the head that long before.
- A seed that ends with another client's rewrite waits until every workspace has followed before that client loads the head. Without the wait, 2 of 600 seeds lost a file from a workspace's directory, and main loses it the same way. jj merges the operation head stamped later into the one stamped earlier, so a client whose clock is behind has its rewrite kept over a snapshot of Reef's that committed after it loaded. The workspace has no edits, so it checks that commit out ([0018](0018-snapshot-staleness-and-divergence.md)), and the file its snapshot had just recorded stays only in the other commit of the change, which is now divergent. This ticket does not change that.

| Code | Seeds 1 to 120 that fail |
| --- | --- |
| The walk by end time (8a9a9e70) | 51: all 39 seeds whose clock is ahead, 12 of the 34 whose clock has stopped, and none of the 47 whose clock is right. In 50 a change diverged. In the other, the stale client's scene waited 20 seconds for a write by change id that never reached its follow-ups |
| The set, found from the graph | 0 |

Seeds 1 to 600 pass, in 14 minutes on four daemons. That run merged operation heads 2,071 times, 534 of them of three heads or more, superseded 308 times, 139 of them while a snapshot was held, and had 156 commits rewritten by another client. Its stale clients had a clock ahead 177 times and a stopped clock 194 times, and 16 had loaded the head more than 1,100 operations before.

The tenth review found one bound that the clocks still decided. The walk gave up after the views of 4,096 operations. A clock that ran fast could stamp more operations than that later than the operation that shows they are before A, below a client that loaded the head early, and the follow then diverged. main carried there. The walk now reads as far as the graph takes it, and of its limits only the 1,024 operations since A can make it diverge (`a_stretch_stamped_ahead_below_a_stale_client_carries`).

The first version of these clocks stamped other clients' operations years back. jj's own merge of operation heads reads the log in the order of its end times too (`closest_common_ancestors` in its `op_walk`), and on a daemon whose every seed had such operations one merge ran for more than 15 minutes on a log of 2,700 operations. That walk is jj's, and main runs it for the same merge: it is no change of this ticket's. The test's clocks are an hour ahead or stopped, which keeps that merge near the head.

Reading the change alone, without asking who moved it, is not enough: with that one condition removed, six tests fail, among them `stale_diverges_when_dirty` in `reef-lib` and in the daemon, which hold [0018](0018-snapshot-staleness-and-divergence.md)'s rule for two workspaces on one change.

The regression tests of all six reviews pass against the derived carry with their assertions unchanged. The unit tests of the slot and of the function that built carries went with them.

**What it costs.** One operation and one view are read per operation since A, on every head, only for a snapshot that is stale with edits or whose commit moved under its scan. A follow runs right after the move, so that is a few operations, and the snapshot that follows takes 0.7 ms in a release build on an Apple M-series laptop. A client that loaded the head before A was written costs one more operation, without its view, for each operation between its load and A. With 1,100 of them the snapshot takes 39 ms, and with 10,000 it takes 0.44 s, about twice what jj takes to load those operations. Each workspace pays that at its next follow, and no longer once a snapshot of its own comes after the merge. Clocks that were wrong add views of operations that turn out to be before A: one for each operation stamped later than the operation that shows it is before A. They cost reads and change no answer. With 4,200 of them the snapshot takes 0.30 s, and with 10,000 it takes 0.80 s. Nothing bounds those reads, as nothing bounds the reads before A, because a bound would make the answer depend on the clocks. A follow more than 1,024 operations after the move reads each of them with its view and then diverges: 0.60 s for 10,000.

**What else changes.**

- A daemon restart between a move and the follow no longer diverges the change: the operation log is still there (`a_restart_between_a_move_and_its_follow_still_carries`). One restart still does. A carried snapshot whose check-out failed keeps the tree it scanned in memory only, and a crash before its next snapshot loses that tree. The next snapshot then finds the workspace's own snapshot between its row's commit and the view's, cannot tell which edits that snapshot already took, and diverges. Both commits are kept. A crash between any carried snapshot's commit and its row does the same. [0039](0039-carrying-edits-through-a-write.md) accepted this window, and persisting the tree would need a schema change for it.
- Any operation Reef writes that moves a workspace within its change is followed, with no code at the writer. That now includes a fetch or a git ref import that rebases a workspace's commit, which diverged a busy workspace before. Since [0089](0089-another-clients-undo-keeps-a-workspaces-files.md)'s amendment of 2026-11-21, a git ref import counts only where it recorded the new commit as a rewrite of the old one.
- A workspace with unsnapshotted edits that garbage collection of operations separates from its last snapshot diverges, where the slot would still have carried. So does one whose follow comes more than 1,024 operations after the move, counting the operations of other clients beside it. A follow is queued by the move itself, so that takes a workspace whose snapshots fail for that long while the rest of the repository keeps writing. Operations before the move are not counted: a client that loaded the head long before it, and commits beside it, does not make the follow diverge.

## Consequences

- Amending the bottom change of a stack held in three workspaces keeps every workspace current, carries what still applies, and leaves a `tree` conflict, an assignment and an instruction on what does not.
- A dependent of a landed change lands with no new proposal. Under `summary` history that is every dependent.
- The inbox (R3.33) and the proposal view (R3.31) read the link (`ProposalEntry.dependencies`), the carry (`ChangeTransition.dependency`) and the return (`missing_dependency` conflicts) from the API.
- Undoing a supersede reverts the rebase and the transition in one operation. The carries recorded right after are their own operation, and a dependent then reads `amended` until it is proposed again or the carry is undone too.
- A descendant of an agent that writes all the time snapshots between its ancestor's snapshots as a matter of course, and its change does not diverge. Where its edits and the ancestor's touch the same lines, the merge is jj's conflict: the change has a conflicted tree and the files have markers, and the agent resolves them as any conflict. A file the ancestor changed that the agent edited after the scan is not overwritten: that snapshot fails before its check-out, and the next one merges the edit in, as a failed check-out does ([0039](0039-carrying-edits-through-a-write.md)).
- With a merge in the stack, moving one parent off a missing change leaves the other on it; the conflict id names the change to move, so the next look reports the next one and tells the agent again.
- An abandoned dependent's assignment closes `resolved`, not `abandoned`: the lifecycle has no abandoned state for the follower to read.
- [0081](0081-automatic-landing.md)'s rule that a waiting change holds back its stack's prefix is unchanged. Relaxing it needs the gate's write to leave a waiting member unblocked, which is a follow-up.

## Alternatives considered

- **Carrying any clean rebase, the change's own `reef rebase` too.** That reverses [0071](0071-rebasing-and-following-trunk.md)'s decision 6 (S-179): an agent that moves its change onto a new base states it.
- **Never carrying.** Every landing under `summary` history would stop the stack above it until each agent proposed again.
- **Withdrawing, or moving off, a dependent whose dependency was declined.** Whether to drop the dependency's work, wait for it, or give up is the owner's call. Withdrawing also takes the dependent out of the inbox.
- **A stored dependency edge or missing-dependency row.** jj's parent edges and the lifecycle rows already say it, and a copy could only disagree with them.
- **Telling the agent through the assignment alone.** A proposing agent waits on its instructions (`wait_for_operations` with kind `comment`), not on assignment operations.
- **A tier and attempt cap for missing dependencies.** There are no files or sides for a policy to judge.
