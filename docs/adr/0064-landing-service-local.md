# 0064. Landing service, local

- Status: accepted
- Date: 2026-11-08
- Ticket: R3.13

## Context

The [design](../design.md)'s landing gate verifies evidence against the repository's policy, refuses a conflicted change, rebases onto the current head, and is a merge queue: it batches rebased changes, tests the batch head with the trusted executor, and bisects on failure. [0061](0061-repository-policy-file.md) made the policy a file on the destination's tree whose evaluation reports data, not a boolean, and left enforcement here. [0063](0063-change-lifecycle.md) defined `queued` and `landed`, reserved them for the landing service, and gave it a per-repository lifecycle lock. [0056](0056-evidence-artifact-store-and-retention.md) expires evidence on old trunk commits until a landing pins it.

Two of Phase 3's exit criteria are a proposal with trusted evidence landing without a human, and forged or untrusted evidence refused with a structured reason. Batches and bisection are R3.14's; this ticket is the queue they run in.

Reef's own writes run on the repository thread, but another jj writer can move a bookmark at any time, and a Reef workspace snapshots whatever its working copy holds: a workspace whose working-copy commit became the destination's head would amend it, and jj moves a bookmark with a rewritten commit.

## Decision

1. **A landing is asked for.** `LandingService.LandChange` (and `reef land`) asks to land a proposed change. Landing what policy allows without a request, and requesting missing evidence from executors, are R3.45's.
2. **The gate refuses anything but `ALLOW`.** The proposal's recorded commit (the submitted tree, where the evidence and sign-offs are) is judged with the `reef-policy.toml` at the head of its destination bookmark, at the commit the landing moves; no policy, an invalid one, or a blocking rule is a landing with outcome `blocked`, and the change stays proposed. The evaluation is the refusal: for each missing evidence pair it lists the objects of that kind and target that did not count and why (`signature_invalid`, `error`, the key's status at witness time, `self`, `presence`, `condition`, `untrusted`).
3. **Commutation is a clean rebase.** When the destination's head is the change's only parent, the landing is a fast-forward. Otherwise the change is rebased onto the head; a rebase that conflicts writes nothing and refuses with its paths. A clean one lands. The paths both the change and the destination changed since the change's parents (the overlap) are recorded and given to policy as `landing.rebased` and `landing.overlap`, with `change.state`, so a repository can require fast-forward or path-disjoint landings until R3.14 tests the batch head. A change whose parents are not on the destination is refused (stacks are R3.10's).
4. **A queue with one worker per repository.** A change the gate passes is queued (R3.09's enqueue, cause `landing`). The worker lands queued changes oldest first, through `land_batch`, which takes a list (one change until stacks and batches). Under the repository's create lock shared from the judgment to the record, it previews and judges again at the destination's head, then holds the lifecycle lock, checks the change is still queued at the commit it was queued at, and lands through `Manager::rewrite` (given the lock already held): its live workspaces are snapshotted first, so an edit since the proposal returns the change as `amended`.
5. **One operation moves the destination.** Kind `land` (`OPERATION_KIND_LAND`), `reef.lifecycle` with the `QUEUED → LANDED` transition naming the landed commit, `reef.landing` with the landing's id, the rebase and its descendants, and a new empty working-copy commit for every workspace on the landed commit, so a later snapshot there cannot move the bookmark past the gate. A refusal after queueing returns the change to proposed (R3.09's dequeue) with a reason; the destination moving under three attempts is `busy`.
6. **Records per attempt.** `landings` (outcome, reason, destination before and after, policy commit, blob and SHA-256, operations, who asked) and `landing_changes` (queued and landed commits, rebase, overlap, conflicts, the evaluation as JSON), migration 13. A repeated identical blocked request counts on the latest row. Rows are written after the transitions they follow; a crash between them is reconciled at start from the lifecycle rows.
7. **Pins.** The evidence a landed change's satisfied rules used is pinned until `repo forget` (`landing_evidence`, part of retention's pin condition) and linked to the change; the policy bytes are copied into `landing_policies` by SHA-256; the landed change's provenance is exempt from compaction and expiry. Artifacts named by pinned evidence keep their own retention.
8. **Not undoable.** Enqueue, dequeue and land are refused by `UndoOperation` (`OPERATION_NOT_UNDOABLE`): others build on the destination at once, and `LANDED` is final.
9. **Local bookmarks only, no git export.** A proposal for a remote bookmark is refused; the jj CLI exports the moved bookmark to git on its next command, as for every Reef write.
10. **`LandingService`** (API revision 23): `LandChange` (WRITE, waiting up to `wait_ms`), `GetLanding` and `ListLandings` (READ). Through the remote listener any `write` token may ask: proposing is the request, and policy decides. The three RPCs are not MCP tools (`tools/list` is at its budget, #116); `reef land` and `reef landing` are in `--help-all`.

## Consequences

- [docs/landing.md](../landing.md) documents the gate, the queue, rebases and overlaps, the outcomes and reasons, pins and the limits.
- A clean rebase is not a test (S-120): until R3.14, a repository that wants the landed tree to be the tested one says `!landing.rebased`.
- A local `main` that also follows a git host diverges from it after a landing; R3.17 pushes landed history.
- The threat model gains S-119 to S-126.
- R3.45 adds triggers to the same gate; R3.14 widens `land_batch`; R3.08 (#246) can judge landings again from their records, pinned evidence and policy bytes.

## Alternatives considered

- **Judging the rebased tree.** Evidence and sign-offs are on the submitted tree, so nearly every landing after `main` moved would need new evidence and a new sign-off until R3.15.
- **Fast-forward only.** The landed tree is always the tested one, but every moved `main` means rebase, re-run and re-propose; a policy can still ask for it.
- **Landing on proposal without a request.** A second trigger system and per-proposal evaluation keys: R3.45.
- **Rebasing a change without its unlanded parents**, or landing the range: the evaluation judged the change's own diff, so the landed content would differ from what was judged.
- **Leaving a conflicted rebase in the change**: it would rewrite the owner's workspace under them before R3.25's conflict objects exist.
- **Exporting the bookmark to git in the landing's operation**: no other Reef write exports, and an export can fail on a ref git moved meanwhile.
- **Undoable landings**: a landing is what others build on next, and `LANDED` stays final.
- **The RPCs on `ChangeService`**, or **landings as lifecycle rows only**: a landing carries an evaluation, a policy, pins and two operations that a transition row has no place for, and a blocked request is never a transition.
