# 0059. Executor protocol

- Status: accepted
- Date: 2026-11-07
- Ticket: R3.05
- Amends: [0029](0029-daemon-api-v1.md) (decision 12's exceptions gain `SubmitJob`, `CancelJob`, `ClaimJob`, `UpdateJob` and `CompleteJob`), [0037](0037-presence-channel.md) (a daemon-only kind, `JOB`, bounded by `[executor] max_open_jobs`)

## Context

The [design](../design.md) keeps execution outside Reef: "Scheduling is Reef's, execution is the executor's, and the protocol between them is what Reef must get right: a request names a tree hash, the targets to check, the evidence kinds expected, and a budget; a response is signed evidence plus artifacts; progress streams over the presence channel. The protocol is written down before the reference executor is built, so other executors can be built against it."

[0055](0055-evidence-object-format.md) fixed what an executor returns (evidence about a `gitTree`), [0056](0056-evidence-artifact-store-and-retention.md) how its artifacts arrive (uploaded by digest), and [0057](0057-operator-identities-and-session-keys.md) what signs them (a session key with actor `executor`, bound to a person or a `ci` identity). Nobody requests checks yet: the landing service (R3.13) and the batching queue (R3.14) will. R3.06 builds the reference executor against this protocol.

## Decision

1. **Executors pull.** The daemon keeps a durable queue of jobs in `reef.sqlite3` (`jobs`, `job_evidence`; migration `0011_jobs`, schema 11) and leases them to executors that dial the socket or the remote listener. It never connects to an executor. One binding ships: `reef.api.v1.ExecutorService` over gRPC (API revision 19), whose seven RPCs serve both sides: `SubmitJob`, `GetJob`, `ListJobs` and `CancelJob` for requesters, `ClaimJob`, `UpdateJob` and `CompleteJob` for executors. The messages and the state machine are written down apart from gRPC in [docs/executor-protocol.md](../executor-protocol.md), so a store (R4.11) and the specification (R6.03) can restate them.
2. **A request names a tree, kinds, targets, a budget, requirements and a priority, never a command.** What runs for a kind and a target is the executor's. Requests are validated (1 to 16 kinds, 0 to 1,000 targets, at most 1,000 pairs, label syntax, budget ranges); an identical open job of the same submitter (remote token, or the socket) is returned rather than queued twice, unless the request asks for a duplicate; at most `[executor] max_open_jobs` jobs are open, and a submission past it is refused, never evicting.
3. **A claim is a long poll with a lease.** `ClaimJob` waits up to 60 s for a matching job (all its kinds among the executor's, its requirements among its labels, its protocol version at most the executor's), picks by priority then age in one immediate transaction, and returns a per-attempt claim token, kept only as its blake3 digest. At most 16 claims of one remote token wait at once, half of its request slots, so its heartbeats always find one. The lease (60 s by default, 15 s to 10 min) is extended by each `UpdateJob`; a lapse or a release requeues the job, up to `[executor] max_attempts`. The daemon enforces a deadline (claim, wall time, 60 s) and a queue timeout as backstops to the executor's own budget enforcement. Every transition is one immediate transaction that reads the job again, and what follows it (the presence publish) runs before the database lock is released, so publishes keep commit order; the timer reads due times from the open rows and acts through the same transactions.
4. **Evidence travels by value in `UpdateJob` and `CompleteJob`,** at most 50 envelopes per call, each checked by [0055](0055-evidence-object-format.md)'s `Verified::check`, bound to the job's tree and kinds, and stored through `AddEvidence`'s insert (shared, `add_evidence_in`) with a `job_evidence` row each, all or nothing per call. Evidence is added by tree, with no link to the change. Artifacts are uploaded first, by digest, through [0056](0056-evidence-artifact-store-and-retention.md)'s `UploadArtifact`.
5. **`DONE` promises a summary object per requested (kind, target) pair;** finer objects are accepted and kept. Freshness (stored by this call, and started no earlier than five minutes before the claim) is reported on each accepted object and on the job's coverage, never enforced. Whether the signer is trusted is not checked at receipt: every read reports it, and policy decides ([R3.07](../dev-phases.md)).
6. **Progress is a daemon-reported presence kind, `JOB`,** one entry per open job, built from executors' `UpdateJob` calls, ended (`CLEARED`) with its terminal state, so a subscriber sees how a job ended. Clients may not report it (`DAEMON_PRESENCE`). Its entries are bounded by `max_open_jobs`.
7. **Remote rules.** Any `write` token may submit (`Rule::None`). Only a token linked to an operator identity may claim (`Rule::LinkedToken`), and the job records the identity. Updates and completions go only through the claiming token (`Rule::OwnClaim`), and cancellation only through the submitting one (`Rule::OwnJob`).
8. **Protocol version 1**, an integer apart from the API revision (`reef_api::EXECUTOR_PROTOCOL`, `ClaimJobResponse.protocol_version`, `reef schema`'s `executor_protocol`), moved only by a change an executor speaking the old version would get wrong; such a job records the version it needs.
9. **Finished jobs are kept `[executor] job_retention`** (30 days) after they finish, never before `keep_newer`, and then deleted by GC's ninth step with their `job_evidence` rows; their evidence stays under its own retention. `reef repo forget` deletes a repository's jobs. Nothing in a job's life records an operation: a job is daemon metadata, not jj state.

## Alternatives

- **The daemon calls an executor endpoint (push).** Needs configured endpoints, credentials for them and outbound network access, a configuration and an attack surface (the daemon would make requests for whoever can submit). A CI runner behind NAT can dial out; a daemon cannot dial in to it.
- **A stream of job offers instead of a long poll.** Still needs a claim call to take one, holds one of a token's 16 streams per idle executor, and needs idle-timeout handling at every proxy.
- **The executor adds evidence with `AddEvidence` and `CompleteJob` names the ids.** Two paths to keep consistent, and a window where evidence is stored but not yet bound to the job.
- **Evidence by digest only, as artifacts are.** An envelope is at most 64 KiB, so 50 fit in a request; a second upload path buys nothing.
- **Commands or images in the request.** Every submitter could run code on every executor.
- **Executors report `PROGRESS` entries themselves.** An executor could report anything about any job, and an entry's end would not say how the job ended.
- **`PROGRESS` with a job id instead of a new kind.** A job also has a state, an executor and an outcome, which `PROGRESS` does not read as.
- **The API revision as the protocol version.** It moves with every schema change, most of which mean nothing to an executor.
- **Completeness by kind only, or not at all.** A requester could not tell which targets ran.
- **Refusing evidence that is not fresh.** Old evidence on the same tree is still true; whether a job's result must be new is policy's.
- **Any `write` token claiming jobs.** An unlinked token gets no session keys, so its evidence weighs nothing; what it could still do is sit on jobs.

## Consequences

- R3.06 builds `reef executor` against [docs/executor-protocol.md](../executor-protocol.md); any gap it finds is fixed there.
- R3.13 and R3.14 call `Executor::submit` in process and read coverage; R3.18 computes targets, R3.19 composes finer objects, R3.20 accounts `elapsed_ms` and `usage`, R3.21 sets priority, R3.22 adds a base tree, R3.34 draws `ListJobs(queue_order)` and the `JOB` entries.
- The threat model gains S-74 to S-81. Executor-forged evidence is not detectable by the daemon; the spec says what a trusted executor must do.
- MCP gains no tools: agents propose, they do not schedule; executors speak the protocol directly.
