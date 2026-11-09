# 0067. Batching and bisect

- Status: accepted
- Date: 2026-11-09
- Ticket: R3.14
- Amends: [0064](0064-landing-service-local.md) (decisions 3 and 4: a landing's `[[require]]` evidence is judged again on the batch's tested tree, and the worker lands batches)

## Context

The [design](../design.md)'s landing gate is a merge queue: "It batches rebased changes, asks the trusted executor to run the required checks on the batch head, lands the batch if the checks pass, and bisects the batch to find the culprit if they fail." [0064](0064-landing-service-local.md) built the queue with a batch of one and judged only the tree a change was proposed at, so a clean rebase landed a tree nobody tested (S-120). [0059](0059-executor-protocol.md) gave the daemon a durable job queue that executors pull from, deduplicated by spec, and [0060](0060-reference-executor.md) an executor that reads a job's tree through a commit.

A batch's checks take minutes to hours. The repository's create lock is writer-preferring, and GC takes it exclusively: holding it shared across a check would hold every create behind GC as long. jj's index is rebuilt by GC from the operations' views, so a commit only ever written outside a view does not survive it.

## Decision

1. **Every landing runs in a batch.** The worker takes the queued changes for the destination of the oldest, oldest first, up to the destination policy's `[landing] batch_max` (8 by default, 1 to 32), waiting up to `batch_wait` (0 by default, at most 10 minutes) for more unless the oldest was in a batch before. Each is judged as the gate judges it, then they are rebased in memory one on another onto the destination's head, the **chain**; a change that conflicts only with an earlier member waits for the next batch. A change that touches `reef-policy.toml`, `reef-checks.toml` or an `alone` path is a batch of its own. One batch is in flight per repository.
2. **`[landing] checks` says where `[[require]]` evidence must hold.** `batch` (the default): a change's submitted tree is judged with every rule at the gate and again at landing, and its `[[require]]` rules are judged a second time over the evidence on the **tested tree**, the chain's tree with every member up to the tested prefix, whose other rules do not count. `proposal`: the submitted tree only, as 0064 landed.
3. **The tested tree's evidence counts for every member at or below it.** A passing head lands every member; the trees below it are not tested themselves, and each landing records which tree was (`tested_tree_id`). `batch_max = 1` makes every landed tree a tested one. Evidence stays bound to the tree it attests; nothing is copied.
4. **Jobs only for what is missing, on a trial commit.** A prefix whose tree already has the evidence needs no job: evidence is by tree, so a fast-forward batch of one lands as before, and a tree tested once is never tested again. Otherwise a **trial commit**, a new change on the destination's head whose tree is the tested tree, is written in an operation of a new kind, `batch` (`reef: test batch bat_… on main`), and one job per kind is submitted by revision for the missing pairs (an executor claims only a job whose kinds are all its own), at priority 1,000 with a 15-minute queue timeout. Trial commits are visible while their batch is open, so GC keeps them, and abandoned when it ends; a visible one with the same parent and tree is reused.
5. **Bisect is a binary search over prefixes.** A failing head (a trusted `fail` as a required pair's newest verdict; the first one cancels the trial's other jobs) is bisected: at most ⌈log2 n⌉ more prefixes are tested. The passing prefix lands in one operation, the first failing member returns to proposed with reason `checks` and the failing evidence, and the members after it stay queued and lead the next batch. A prefix is judged by its own members' requirements.
6. **Infrastructure is not the change's fault.** A job that ran and gave no verdict (`FAILED`, out of budget or attempts, or only `error` or untrusted evidence) is submitted once more; a job nobody claims within its queue timeout, or a second such end, makes the trial `unavailable`, and its members return to proposed with reason `unavailable`. The landing's message says whether any executor claimed a job of the repository in the last day, and `reef land`'s hint names `[landing] checks = "proposal"` as the way out for a repository no executor serves.
7. **Nothing is locked while checks run.** The create lock is taken shared for each step that reads for a judgment or writes (forming, a trial commit, ending a batch, landing), never across waiting. On every wake-up (each change to a job, and a five-second tick) the batch checks the destination's head and its members; a move or a departure cancels its open jobs, abandons its trial commits and re-forms the batch, which costs nothing for a tree already tested (evidence by tree, jobs deduplicated by spec). Three batches in a row re-formed because the destination moved dequeue their members with `busy`.
8. **One operation lands a prefix.** Kind `land`, every member rebased in order onto the previous, refused unless each lands as the tree the batch planned (and tested), with a new working-copy commit for every workspace on any landed commit, the batch's trial commits abandoned, each member's `queued → landed` transition, `reef.batch`, and `reef.landing` when one change lands.
9. **A restart re-forms.** An open batch is finished `interrupted` at start; the next forms from the queue and reuses its trial commits, its open jobs and its verdicts, so a bisect replays from evidence.
10. **Records and API.** `landing_batches`, `landing_batch_members` and `landing_batch_trials` (migration 15), and `batch_id` on landings and the tested tree, commit and evaluation on landed changes, which pin the tested evaluation's evidence too. `LandingService.GetBatch` and `ListBatches` (READ, API revision 26), `LANDING_REASON_CHECKS` and `UNAVAILABLE`, `OPERATION_KIND_BATCH`, `Operation.batch_id`; the policy's `[landing]` table and `batch` input; `reef landing batches|batch`. The two RPCs are not MCP tools (`tools/list` is at its budget, #116).

## Consequences

- [docs/landing.md](../landing.md) documents batches, the tested tree, trial commits, bisect and the reasons; [docs/policy.md](../policy.md) the `[landing]` table and `batch`.
- A repository with `[[require]]` rules and no executor serving its kinds waits 15 minutes for a rebased landing, then gets `unavailable`; `checks = "proposal"` is the way out, and `reef policy init` writes `[landing]` explicitly.
- The threat model gains S-143 to S-150; S-120 is fixed under `checks = "batch"`.
- R3.15 adds per-kind transfer from the submitted tree to the tested evaluation; R3.18 replaces the requested pairs with a minimal set; R3.20 budgets the batch's jobs; R3.21 orders the candidates and sets the jobs' priority; R3.34 draws `ListBatches`; R3.45 reuses the job requests for proposals; R3.10 lets a member's parents be earlier members.

## Alternatives considered

- **Testing every prefix's tree in parallel.** Every landed tree is tested, at n times the executor load.
- **Hidden trial commits.** No log clutter, but GC's index rebuild loses a commit no view ever held, so GC would have to learn about open batches.
- **A `FileService` read by tree**, so a job needs no commit: an API and executor change, and an executor that fetches through git still needs a commit.
- **The batch settings in the daemon's `reef.toml`.** Per machine, and not reviewed with the policy.
- **Testing each member alone on failure.** One round, but blames only changes that fail alone and misses interactions.
- **Landing each passing probe's prefix at once.** Earlier landings, at the cost of an operation per probe and a destination the batch itself moves.
- **Keeping members queued when no executor answers.** No owner action, but the queue stalls with no signal.
- **Persisting and resuming the bisect position.** More state and recovery code for a replay that evidence by tree already makes cheap.
- **Batch-only checks, with no `[[require]]` evidence at the gate.** Halves executor load, but a broken proposal then costs a bisect; R3.15's transfer rules are where that trade is made per kind.
