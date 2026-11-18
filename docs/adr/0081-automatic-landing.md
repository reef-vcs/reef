# 0081. Automatic landing and evidence requests

- Status: accepted
- Date: 2026-11-18
- Ticket: R3.45 ([brief](../dev-tickets/phase-3/R3.45-automatic-landing.md))

## Context

The [design](../design.md)'s Landing section has an agent propose a change with its evidence and the landing service land what policy allows. Until this ticket, two steps still needed a command. The landing service (R3.13, [0064](0064-landing-service-local.md)) landed only when asked (`LandChange`, `reef land`). Its gate also required every `[[require]]` pair on the change's own tree, under `[landing] checks = "batch"` too, while only a batch (R3.14, [0067](0067-batching-and-bisect.md)) asked an executor for anything. So an agent ran `reef job submit` on its own change, waited, then ran `reef land`.

Agents are Reef's users, and a standard laptop must run five tickets at once. Doing these steps without a request means judging open proposals again as their inputs change, and that must cost nothing while nothing changes. Evidence has no generation counter, the destination moves with every landing, and the gate is the only judgment that must decide a landing.

## Decision

1. **One automatic status per open proposal, kept in memory.** Each change `proposed` for a destination has a status: `off`, `pending`, `waiting` (evidence it lacks was asked for, jobs open), `ready`, `queued`, `needs` (blocked by something no job provides), `no_executor` or `returned`. Each status carries the gate's message, the pairs, the jobs, the landing, a reason and a `next` argv. `ProposalEntry.automatic` reports it, and `reef proposal show|list` print it. Only `landings.automatic_key` is stored (migration `0023_automatic_landing`). A start judges every open proposal once.
2. **The gate's own judgment, as a dry run.** The gate splits into `decide` (resolve, the proposal checks, the stack, `judge_slots`) and `write`. The automatic service calls `decide` on each stack's head, and queues through `write`, which checks every proposal again under the lifecycle lock. The worker judges again at the destination's head. A stale status can delay a landing and never admit one.
3. **Re-judged only when the evaluation key changes.** The key is a SHA-256 over:
   - the stack's changes and commits;
   - the blob of `reef-policy.toml` at the destination;
   - the evidence ids on the stack's trees (an index-only read);
   - each change's provenance count and newest time;
   - the destination's head, but only when the policy reads `landing` or `batch` (the static check now records which input variables expressions read), or when the last judgment was blocked by the preview.

   A destination that moves without changing its policy re-judges nothing. Events the key cannot see mark proposals instead: transitions, revocations, and pruned evidence or artifacts. A judgment older than an hour is made again.
4. **Evidence requests follow `checks = "batch"`.** Under the default, every open proposal's missing `[[require]]` pairs on its own tree are asked of executors, whether or not the destination lands automatically. The request is made by revision, one job per kind, as `reef-landing`, at priority 500 (a batch's is 1,000), deduped by the executor's spec digest.
   - **Bounds.** At most 32 open jobs per repository. A job that ended with no verdict is retried once; a second such end is the operator's.
   - **No executor.** For a kind whose job of the repository expired unclaimed, with no claim since, the service keeps one job of that kind open, for the next executor to claim, and the other proposals read `no_executor` with `reef executor run`. One kind with no executor never holds back another.
   - **Cancellation.** A queued job is cancelled when its proposal leaves review, unless another proposal or a batch still needs it.

   Under `checks = "proposal"` nothing is asked for.
5. **`[landing] automatic = true` queues what the gate allows.**
   - **Stacks.** A stack waits while its first unpassed change only waits for requested evidence. Landing the prefix below that change would rewrite it, so its proposal would read `amended`. A change blocked for good lets the prefix below it land.
   - **Returned landings.** A landing the service queued that comes back is queued again only when the key changes. Two exceptions: `busy` is retried after a minute, at most three times per key, and `unavailable` is retried after an executor claims a job again. This rule reads the landing rows, so it holds across restarts.
6. **`reef land` waits for requested evidence.** `LandChange` on a `waiting` proposal waits, within `wait_ms`, for the jobs, then judges. If the wait ends first, it returns `LandChangeResponse.waiting` with no landing and records nothing, and `reef land` exits 6.
7. **The daemon tells the proposing agent.** On an automatic destination, when automatic landing gives up for a reason the agent can fix, the daemon writes one R3.12 instruction to the session that proposed the change. The reasons are a failed check on its tree, a batch's culprit, a conflict, an amendment, or an empty change.
   - **Author.** Actor `daemon`, agent `reef-landing`, no identity. The instruction is created `pending`, with no estimate confirmation.
   - **Text.** A fixed template filled only with ids, the reason, the policy's kinds and targets, and the destination's name, each cut to a safe alphabet.
   - **Limits.** One per change, commit and text, and none while one of the daemon's is unfinished on the change.
8. **`EvaluatePolicy` over a stack (#328).** `EvaluatePolicyRequest.stack` judges the revision's stack as the gate would, with no check of proposals, and the destination is the bookmark `policy_revision` names. It answers `members` (each change's verdict and evaluation) and `prefix`. `reef policy eval --stack` prints one line per change.
9. **Bounds.** One task serves every repository, so there is one judgment at a time daemon-wide. A pass judges at most 16 stacks, with one pass per repository every 2 seconds, and wakes only on events, with no polling while nothing waits. API revision 37 and schema 22 (one column, migration `0023_automatic_landing`; #262 holds 37 and `0022` in parallel, and the second to merge renumbers), S-221 to S-229.

## Consequences

- An agent proposes, and its change is checked and lands, or comes back with an instruction saying why, with no command. `reef land` stays the one command to wait for the outcome on any destination.
- The status is a cache in front of the gate. A missed input costs latency, never a wrong landing.
- A laptop with no executor holds at most one round of queued jobs per repository, which expire after `[executor] queue_timeout`.
- R3.18 replaces the requested pairs with a minimal set, R3.20 budgets them, and R3.21 orders proposals and replaces the two priorities. R3.31 and R3.33 show the status. R3.24 builds its `stack` input from `judge_slots`'s read.

## Alternatives

- **Evidence requests only under `automatic`.** That is one switch, but on a destination landed by hand an agent still runs `reef job submit` before `reef land` can pass.
- **A stored status table.** Every input except the re-queue rule can be derived, and a start judges again within the same bounds. A table would be one more thing to keep consistent with the gate.
- **The destination's head in every key.** Every landing would judge every proposal again, which is the cost the row asked to avoid.
- **A generation counter on evidence and provenance.** Every writer would have to bump it, and evidence has no rowid. The ids on a tree are an index-only read.
- **Landing a stack's passing prefix while a change above waits for evidence.** That rewrites the waiting change, which must then be proposed again by hand until R3.11.
- **Instructions with the failing check's output.** That output is another party's text in an agent's context (S-228). The template names the check and its evidence id, and `reef landing show` has the rest.
