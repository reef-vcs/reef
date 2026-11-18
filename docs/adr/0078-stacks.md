# 0078. Stacks

- Status: accepted
- Date: 2026-11-18
- Ticket: R3.10 ([brief](../dev-tickets/phase-3/R3.10-stacks.md))

## Context

The [design](../design.md)'s data model gives a change "dependency edges on other changes, so a stack of changes is explicit"; its Landing section says an agent proposes "a stack of dependent changes as one proposal" and that "a stack lands as a prefix: if a later change fails, the earlier ones still land"; its human interface reviews "stacks, not commits", where "the reviewer accepts the proposal or a prefix of its stack". Phase 3's exit criteria include "a stack lands as a prefix when a later change fails".

Before this ticket, R3.09 ([0063](0063-change-lifecycle.md)) proposed one change per operation and left the rule for unlanded ancestors to R3.10. R3.13 ([0064](0064-landing-service-local.md)) refused a change whose parents had not landed (`unlanded_parents`). R3.14 ([0067](0067-batching-and-bisect.md)) checked every batch member against the destination alone, so a member could never be the child of another. R3.16 ([0075](0075-landed-history-projection.md)) projects one commit per change and named squashing a stack as this ticket's.

One fact shaped the design: landing a change rewrites its unlanded descendants (a rebase, or a summary under `[history]`), so a proposal above a landed change reads `amended` afterwards. A stack that lands in two operations strands its upper half.

## Decision

1. **The edges are jj's parent edges, and nothing stores a stack.** A change depends on the changes of its commit's parents. The stack of a change `C` for destination `B` is `(::C) ~ (::B's head)`: `C` and its ancestors not yet on the destination, parents first, merges allowed, at most 32 (`land::MAX_STACK`, R3.14's largest batch). jj keeps these edges through every rebase, squash and `jj` command; a stored copy could only disagree with the commit that lands.
2. **One operation proposes, withdraws or declines a stack.** `TransitionChangeRequest.stack` applies the transition to every member: a proposal skips members already proposed at their commit for the destination, and queued ones; a withdrawal or decline skips members not in review. A member the transition refuses refuses the request, naming it; through the remote listener, R3.09's ownership rule holds per member. One undo reverts every member. `reef propose` proposes the stack by default (`--only` for one change); `reef proposal withdraw|decline` take `--stack`.
3. **Landing a change lands its stack, by prefix.** `LandChange` on `C` requires every member below it to be proposed for the same destination at its visible commit, or queued (else `NOT_PROPOSED`, `PROPOSAL_AMENDED` or `STACK_DESTINATION_MISMATCH`, naming the member). The gate judges the members in order, each previewed as rebased onto the ones below it, so a member's parents may be the members below (`checked_commit` takes them). The members before the first refusal are queued in one operation; the first refused is `blocked` with its reason, the members above it `blocked` with `unlanded_parents` naming it. A change to `reef-policy.toml` or `reef-checks.toml` ends the prefix: at the bottom it is queued and the rest wait; above the bottom it is `blocked` with the new reason `alone` (S-210).
4. **A stack lands in one land operation.** Formation reads the queued commits' parents and works on stack groups: an open landing with every queued change above it, bottom first. A group joins a batch whole, may make it larger than `batch_max` only as its first group, and makes a batch of its own when a member touches an `alone` path. A member returned (blocked at formation, excluded from the chain while a member below it lands, or the culprit of a bisect) takes the queued members above it with it, `unlanded_parents` naming it. A group of one is R3.14's batch member, unchanged.
5. **A sign-off on a member accepts the members below it.** The evidence kinds a policy counts as sign-offs (`[owners]`' and each `[[signoff]]`'s) on the submitted trees of the members above a change in the stack being landed count for it, verified as any evidence is. Accepting a prefix is signing its top. Tests and other `[[require]]` evidence stay on each change's own tree and R3.14's tested trees.
6. **One commit per change.** A stack lands as one commit per member, each with its change id, projected by `[history]` like any batch. `squash = "stack"` is not built: the members' change ids would vanish from the destination, which the history index reads as abandoned, and prefix landing makes "the stack" more than one unit anyway.
7. **The record and the surface.** Schema 21 (`0021_landing_stacks`): `landings.stack`, the landing id of the stack's head in the request, and `stack_position`, which record and decide nothing. API revision 35: `TransitionChangeRequest.stack`, `TransitionChangeResponse.transitions`, `Landing.stack`, `LandChangeResponse.stack`, `GetLandingResponse.stack`, `LANDING_REASON_ALONE`, and the reasons `STACK_TOO_LARGE`, `STACK_NOT_ALLOWED` and `STACK_DESTINATION_MISMATCH`. Two MCP tools, `propose_change` and `land_change` (which waits up to two minutes, as `reef land` does), so an MCP agent proposes and lands without leaving Reef (the user's decision 8).

## Consequences

- An agent that splits a task into several changes proposes and lands them with the two commands it already uses, and sees per change what landed and why the rest did not.
- A stack never lands across two operations, so no landing strands queued members at a commit that no longer exists. Members a prefix landing leaves behind are returned with a reason; if the prefix was rebased or summarized, their proposals read `amended` and are proposed again with one command. R3.11 carries them across the rewrite.
- `reef land` on a change with unproposed ancestors is now a request error naming the ancestor, where R3.13 recorded a blocked landing.
- R3.24 builds its `stack` policy input from `land::stack` and the gate's and formation's chains; #268 publishes `land::stack`'s members as stacked review branches; R3.21 orders groups, not changes.

## Alternatives

- **A stored edge table** (`change_dependencies`), able to name a dependency the graph lacks: every rebase could make it wrong, and landing reconciles it with the graph anyway.
- **A stored stack object** (`stk_…`) with members recorded at proposal: a second record of what the graph already says, which a later rebase or extension of the stack makes stale.
- **All-or-nothing stacks**: the design asks for prefix landing; a request that wants nothing landed unless the head lands withdraws and fixes first.
- **Splitting groups at `batch_max` or `alone` paths**: the members above the split are rewritten by the landing below them and return `amended`.
- **Every evidence kind on the head counting for the members below**: a head that passes says nothing about a prefix that does not build.
- **`squash = "stack"`**: see decision 6.
