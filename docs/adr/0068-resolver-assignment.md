# 0068. Resolver assignment and queue

- Status: accepted
- Date: 2026-11-09
- Ticket: R3.27

## Context

The [design](../design.md) routes conflicts to agents: a conflict object "is assigned automatically to a resolver, by default the originating agent of the later change", and "a conflict whose owner's session has ended moves to a resolver queue with a priority". A person "can still take a conflict over and resolve it by hand". The triage inbox lists "a conflict nobody owns", and the fleet dashboard "the conflict queue".

[0065](0065-conflict-objects.md) made conflicts readable and stored nothing: a conflict is derived from jj on every read, with a conflict id that changes whenever a conflicted path's sides change, and `introduced`, the files no parent carries. Who should resolve a conflict, whether it waits in a queue, who claimed it and what became of it are facts only Reef knows, and they must survive the rewrites that keep moving a conflict's commit and, often, its id.

What an owner is had to be observable. [0058](0058-provenance-records.md)'s provenance sessions are per workspace and end when the workspace is released or reclaimed. A harness declares its own session id (`client_session`, R3.44), and subagents of one harness session share it. Presence ([0037](0037-presence-channel.md)) is never persisted, and a workspace created without a follower has no heartbeat on it at all. `reef ws run` releases its workspace when its command exits; an MCP server's workspaces are reclaimed 35 minutes after it stops.

## Decision

1. **An assignment is keyed by change and kind, one open at a time, for the conflicts a change introduces.** Not by conflict id: a partial resolution or a re-rebase changes the id, and an assignment keyed by it would be closed and reopened by its resolver's own progress. The row tracks the conflict's current and first id and when it last changed, and claims and assignments take an expected conflict id. A commit whose every conflicted file is inherited gets none. The same conflict id back within 24 hours (an undo of the resolution) reopens the closed assignment with its history and claim count.
2. **The later change is the conflicted change; its owner is the newest live session that recorded it.** A rebase that conflicts puts the conflict in the commit that was moved, the later one, and the snapshot that resolves it is that change's. The owner is the newest session whose verified provenance records name the change and which is live; else the newest such session, ended; else the live session of the one active workspace editing the change. Otherwise the assignment is `unowned`: Reef never recorded work on the change, so it is most likely a person's, and only a caller queues it. Sides never own the conflicted change.
3. **An owner's session has ended when its workspace has no live provenance session.** A revocation that restarts the session in the same workspace is the same owner. When it ends, the assignment follows a live session of the same repository, identity and declared client session; with none, it waits 5 minutes (`OWNER_GRACE`) and is queued. A queued assignment whose owner ended returns to a live session of that identity and client session while nobody has claimed it.
4. **The resolver queue is the `queued` assignments, by priority then time queued.** Priority is -100 to 100, 0 by default, set by a caller. Live owners' conflicts are not in it, so a resolver never races the change's own agent.
5. **A claim is held by the claiming workspace's provenance session.** No lease of its own: when that session ends with the conflict still there, the assignment goes through decision 3 again. Only queued assignments are claimed; `unowned` ones are refused until a caller queues them. Claims pass through one function R3.28 fills with tiers and attempt caps. The owner of an assignment releases it to the queue.
6. **Any WRITE caller assigns to a workspace, queues, or reprioritizes; a person takes over.** Every change records who made it (actor, agent, identity, session, remote token). A takeover gives the assignment to the caller's person identity (the daemon's default on the socket) with no session, survives any session's end, is refused over the remote listener and for an agent actor, and is never refused at the event cap. Assigning overrides a claim or a takeover; an agent assigning to a workspace is held to a claim's rules.
7. **A follower keeps the rows in step with the log, and closes an assignment when its change no longer carries the conflict.** One task, woken by every operation and by grace deadlines, passes over every repository at most once a second; every assignment RPC, and `GetConflict` and `ListConflicts` for their `assignment` field, looks first. A look reads `mutable() & conflicts()` again only when the head moved, other than by its own operations, and at most 1,000 conflicts that introduce a conflict (8,000 in all). Close reasons: `resolved` (the change's visible commit has no conflict; the session whose verified record names that commit, made after the assignment opened, is kept), `inherited`, `abandoned`, `immutable`. An assignment whose only owner was a workspace editing an unrecorded change goes back to `unowned` when that workspace ends, never to the queue.
8. **Rows are the truth; an operation is the notice.** Every change of a row is an event row in the same transaction, then part of a jj operation that changes nothing in the view, kind `conflict-assignment`, whose `reef.conflict` attribute carries the events (64 at most per operation). Filters match it by change and by its events' owners' workspaces, before and after, so an owner overridden by an assignment sees it. A crash between the two loses a notice, never state. Undo refuses these operations.
9. **Five RPCs on `ConflictService`, schema 17, API revision 28.** `ListConflictAssignments` and `GetConflictAssignment` (READ), `ClaimConflict`, `ReleaseConflict` and `AssignConflict` (WRITE; a remote claim or release names the token's own workspace). `Conflict.assignment` and `WorkspaceStatus.conflict_assignments` report assignments where agents already look; the five RPCs are in MCP's `NOT_TOOLS`. `reef conflict queue|assignments|assignment|claim|release|assign|take`, and `reef status` names a workspace's assignments. Closed assignments are deleted with their events 30 days after closing, by GC's tenth step.

## Consequences

- An agent that works in short-lived workspaces keeps its conflict across them when its harness declares a client session; without one, its conflict is queued 5 minutes after its workspace ends.
- A crashed agent holding a long lease keeps its conflict from the queue until the lease and its grace run out; a person queues it by hand.
- Subagents of one harness session share a client session, so a conflict can follow to a sibling; it stays within one identity.
- A change a person rebased into conflict in their own checkout is never handed to a resolver unless someone queues it.
- R3.28 gates claims and counts attempts from claims and events; R3.26 and R3.11 open assignments of their kinds through the same table; R3.33 and R3.35 read the rows and the operation kind.

## Alternatives considered

- **Keying by conflict id, or by file id.** The resolver's own progress would close and reopen it; per file, a change would have many.
- **Deriving assignments on every read.** Claims, takeovers, queue times and history are facts jj does not hold.
- **Owners from the sides.** One agent would be handed another's (or a person's) change to rewrite.
- **The first session as owner** ("originating" read literally). After a takeover the newest session has the context.
- **Presence or lease heartbeats as liveness.** Presence is not persisted, and most workspaces have no heartbeat.
- **A claim lease with its own heartbeat.** A second lease beside the workspace's, which a resolver needs anyway.
- **Every conflict in the queue, live owners ranked last.** Two resolutions of one change.
- **Ranking by blocked dependents or lifecycle state.** The order would reshuffle under a resolver reading it.
- **R3.09's pending-row protocol for the operation.** Nothing reads the operation back to decide state.
- **A stream of its own, or an MCP tool.** The operation log already reaches every client, and `get_status` tells an agent; `tools/list` is at its budget.
