# 0084. Dependent change lifecycle

- Status: accepted
- Date: 2026-11-19
- Ticket: R3.11 ([brief](../dev-tickets/phase-3/R3.11-dependent-change-lifecycle.md)); fixes [#91](https://github.com/reef-vcs/reef/issues/91) and [#202](https://github.com/reef-vcs/reef/issues/202)

## Context

The [design](../design.md)'s Stacks paragraph says what happens to a dependent when the change below it moves. Amending a change rebases its dependents and keeps their IDs. A dependent that no longer applies carries a conflict object. A declined or withdrawn dependency returns each dependent to its owner, or to the resolver queue. A superseded one moves its dependents onto the superseding change. Phase 3's exit criterion asks for the first two.

jj already rebases a change's descendants in the operation that rewrites it, keeps their change ids, and records a conflict in a commit that no longer applies. [0065](0065-conflict-objects.md) reads that as a `tree` conflict, and [0068](0068-resolver-assignment.md) assigns it to the dependent's owner. What was missing was everything an agent working on a dependent needs around that:

- A dependent held in another workspace kept its old files after an ancestor's snapshot (#202). One with edits not yet snapshotted diverged its change (#91), because a snapshot hands out no carries ([0039](0039-carrying-edits-through-a-write.md)).
- Every rewrite below a proposal left it `amended`, so `reef land` refused it until its agent proposed it again. Under `[history] message = "summary"`, that happened after every landing.
- A declined dependency was silent. `reef land` on the dependent answered `NOT_PROPOSED` with `next: reef propose <dependent>`, which put the declined change back in review.
- Supersede moved nothing.

## Decision

1. **Descendant workspaces follow an ancestor's snapshot.** In the repository-thread job that commits a snapshot, the daemon compares the working copies before and after. Every other Reef workspace on another change whose commit the snapshot's rebase moved to another tree gets a carry from its row's commit ([0039](0039-carrying-edits-through-a-write.md)), unless a write has paused it. The workspace is then snapshotted after the ancestor's snapshot, descendants first:
   - an idle workspace checks the new tree out (#202);
   - a busy one carries its edits onto the rebased commit, in the same change (#91);
   - another workspace on the same change still diverges, as [0018](0018-snapshot-staleness-and-divergence.md) says.

   A carry handed out while another is pending joins it: two ancestor snapshots before the follow give one carry from the first commit to the last. A descendant that scanned before an ancestor's snapshot rebased its commit is not diverged either. On the repository thread, its commit finds the view's commit for it is the one the carry moved its planned commit to. It rewrites that commit instead, with the three-way merge of the rebased tree and the scanned tree over the planned commit's tree. The check-out then writes the ancestor's files, after checking that each file it overwrites still holds, byte for byte, what the scan read; after a watcher's narrowed scan that includes the files the scan itself recorded. A snapshot that took a carry and recorded nothing gives it back unless the working copy ended on the carry's commit, so the follow still carries.
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
6. **Supersede moves the dependents.** If the rebase is refused before it writes (a divergent dependent, too many to move, a commit that moved meanwhile), the supersede is recorded alone and `moved` reports them with `rebase_refused`. `TransitionChange` to `SUPERSEDED` rebases the superseded change's mutable children (a new `RebaseMode::Children`) and their descendants onto the superseding change. The rebase and the transition are one operation of kind `transition`, written through `Manager::rewrite`, so the dependents' workspaces follow. Dependents that still apply are carried at once with event `superseded`, each naming the successor, its children's children too. `UndoOperation` of that operation reverts the rebase and records the inverse transition in one operation of kind `undo`, its workspaces following. A child the successor descends from is left, and `TransitionChangeResponse.moved` reports it.
7. **Bounds.** Schema 25 (`0027_dependent_lifecycle`: two columns on `lifecycle_transitions`), API revision 41, S-243 to S-249. The carry follower checks at most 64 moved proposals per look; missing dependencies cost one query when nothing was ever declined or withdrawn, else one index read before any stack is read. The daemon's instructions go out from the carry follower, outside every lock.

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
