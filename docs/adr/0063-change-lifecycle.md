# 0063. Change lifecycle

- Status: accepted
- Date: 2026-11-08
- Ticket: R3.09

## Context

The [design](../design.md)'s data model says a proposal "is not a separate object; it is the change in its proposed state. A change moves through draft, proposed, queued, landed, and one of declined, withdrawn, or superseded, and the operation log records each transition. The change ID identifies the proposal." Seven later tickets read that state: stacks (R3.10), dependents (R3.11), comments (R3.12), the landing service (R3.13), the proposal view (R3.31), the triage inbox (R3.33), and publishing to a store (R4.07).

[0031](0031-writes-over-jj-state.md) gave every operation Reef records `reef.*` attributes through one `OpMeta`, and left lifecycle kinds to this ticket. GC ([0040](0040-gc-and-the-users-jj-workspaces.md)) drops operations older than its window and gives the rest new ids, so a fact kept only in an operation's attributes is gone after a GC. jj's view has no place for Reef's own data: a ref Reef added would be overwritten by a git import or exported to git.

Until R3.13 exists, every landing happens outside Reef: a push and a fetch, or `jj bookmark set`.

## Decision

1. **Seven states.** `DRAFT`, `PROPOSED`, `QUEUED`, `LANDED`, `DECLINED`, `WITHDRAWN`, `SUPERSEDED`. A change is a draft until it is proposed and has no record until then. A change with no record reads `DRAFT`, or `LANDED` when it is immutable (an ancestor of `trunk() | tags()`), with `recorded: false`, which is what the web UI already showed. Testing, batching and bisecting are what the landing pipeline does to a queued change (R3.14), not states.
2. **The transitions.** Clients propose from `DRAFT`, `WITHDRAWN` or `DECLINED` (each proposal after the first is a new round of the same change), and withdraw, decline or supersede from `PROPOSED` or `QUEUED`. `LANDED` and `SUPERSEDED` are final. Proposing a proposed change whose commit moved records the new commit (`PROPOSED → PROPOSED`); a transition to the state a change is in, with nothing different, records nothing. Enqueue, dequeue and land are the landing service's (R3.13): clients asking for `QUEUED`, `LANDED` or `DRAFT` get `TRANSITION_RESERVED`, so a state never stands in for the landing gate.
3. **A transition is an operation and a row.** The operation has no view change, kind `transition` (`OPERATION_KIND_TRANSITION`), the description `reef: <verb> change <id>`, and an attribute `reef.lifecycle`: compact JSON, format 1, a list of transitions (id, change, commit, from, to, cause, round, reason, superseded-by, destination, undoes), so that R3.10 records a stack and R3.13 a landing in one operation. The row, in `reef.sqlite3` (`lifecycle_transitions`, migration 12), also records who performed it: the `reef-agent`, the actor, the operator identity, the named workspace's provenance session, and the remote token. `lifecycle` holds each recorded change's current state.
4. **Written in a fixed order.** Row pending, then the operation, then the row committed with the operation's id and end time, with `lifecycle` updated in the same SQLite transaction, in a task of its own so a caller that stops waiting cannot cut it in between. At start, a pending row whose operation exists (found by walking the log back to just before the row was written) is committed, and the others are dropped; one committed late changes the state only if its `from` is still the change's state. Reads never see a pending row. A backup copies SQLite before the operation files and the op heads last, so a restored copy agrees by the same rule.
5. **Locks.** Every transition takes the repository's create lock shared (so GC waits) and then its lifecycle lock, held from the state check to the commit. R3.13's landing takes the same lock and lands only a change still `QUEUED` at its recorded commit.
6. **A proposal follows its change; a queued change does not.** Every transition records the change's visible commit. An amended or rebased proposal stays `PROPOSED`, and reads report `amended: true` until it is proposed again. `QUEUED` pins the commit. A proposal may not be empty, conflicted, divergent, immutable or hidden. Its destination is `trunk()`'s bookmark, or a named local bookmark.
7. **Accept is not a state.** A reviewer's acceptance is a `signoff` evidence object on the change's tree, which [0061](0061-repository-policy-file.md)'s policy counts.
8. **Who may.** On the socket every caller is the user ([0027](0027-trust-boundaries-in-phase-1.md)): any client transition is allowed and recorded with a declared actor (`AGENT` when the call carries an agent name, else `HUMAN`). Through the remote listener (`Rule::Lifecycle`): any `write` token proposes a draft; only the token that proposed a change proposes it again, withdraws or supersedes it, or undoes a transition it recorded; nobody declines, since a remote human cannot be verified until R3.43. A remote actor is always `AGENT`.
9. **Observed landings.** The daemon records a proposed or queued change as `LANDED` (cause `observed`, actor `DAEMON`) when one of its visible commits is in `::trunk()`. A task woken by every new operation looks when `trunk()` moves, once per repository with open proposals, in chunks of 500, so its cost follows the open proposals, not every proposal ever made. Other changes, recorded or not, are left alone. A git host's squash or rebase merge gives commits new change ids, which this cannot follow (R3.17).
10. **Undo.** `UndoOperation` of an operation carrying transitions reverts them in the rows, not the view: each, still its change's latest, gets an inverse transition (cause `undo`) in an operation of kind `undo`, restoring the state, commit, round, destination and owner before it; undoing a first proposal returns the change to `DRAFT`; one that is no longer the latest is `OPERATION_SUPERSEDED`. An observed landing, an undo that would restore `QUEUED`, and an operation that changed the view and carries transitions (R3.13's), are `OPERATION_NOT_UNDOABLE` here. A change with no visible commit can still be withdrawn, declined or superseded by its change id. reef-lib's view revert refuses any operation with the attribute, so it can never silently do nothing.
11. **`AbandonChange` refuses a change in review** (`CHANGE_IN_REVIEW`, with `reef proposal withdraw` as `next`), except to resolve a divergence.
12. **GC keeps the rows.** After its jj step, GC maps each row's operation id through its rewrite and marks those it dropped `operation_collected`; it deletes no row. `repo forget` deletes them.
13. **Reads.** `Change.lifecycle` on every change the daemon returns (`GetChange`, `ListChanges`, `GetStatus` at head, the write responses), and at an operation the latest transition recorded by that operation's end. `ListProposals` and `ListTransitions` (READ) on `ChangeService`; `TransitionChange` (WRITE). `Operation.transitions` from the attribute, and `OperationFilter.changes` matches them, so a subscriber or `wait_for_operations` follows a proposal. API revision 21.
14. **Surfaces.** `reef propose` and `reef proposal list|show|withdraw|decline|supersede`, in `--help-all`; the state in `reef show` and `reef log`; no MCP tool until R3.13 (the kind only); the web UI's existing state colors; `change.state` joins the policy input in R3.13.

## Consequences

- The state survives GC, undo and restore, and every transition is in the log while its operation is.
- Before R3.13, a proposal reaches `LANDED` only by an observed landing; a squash merge on a git host leaves it `PROPOSED` until withdrawn or superseded.
- Transitions add one empty operation each to the log.
- Two records can disagree only across a crash, and start-up resolves them.
- On the socket, who performed a transition is what the caller said (S-107).
- The state is per daemon until a store (R4.07) reconciles two daemons' records of one change.

## Alternatives considered

- **The operation log alone, folding transitions on each read.** GC drops old operations, and listing every open proposal would walk the whole log.
- **A trailer in the commit.** It changes commit ids and leaks into git.
- **Refs in jj's view.** A git import overwrites them, and an export pushes them.
- **An `ACCEPTED` state.** A second, unsigned record of what a `signoff` evidence object already says signed, and policy would have to choose between them.
- **One RPC per verb, or a `LifecycleService`.** Four methods of one shape; and the state is the change's, so `ChangeService` owns it.
- **Amendments returning a proposal to draft.** A fix for a review comment would unpropose the change.
- **Clients recording `QUEUED` and `LANDED` until R3.13.** A state would then mean nothing.
- **No observed landing, or one derived at read time.** Proposals landed by git would stay `PROPOSED`, or land without an operation recording it.
- **Transitions not undoable.** It breaks "every mutation is undoable"; the inverse verb does not restore a round or a commit.
