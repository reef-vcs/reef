# Executor protocol 1

This document is what an executor author builds against. An executor is a program that runs checks (tests, builds, lints) on a tree and reports signed results to Reef. It needs nothing from Reef's source but this page, [evidence.md](evidence.md) (the evidence format), [identity.md](identity.md) (session keys), and the API's protobuf schema (`reef schema api`, or `crates/reef-api/proto/reef/api/v1/executor.proto`). The decisions behind it are [ADR 0059](adr/0059-executor-protocol.md).

## 1. Roles

- A **requester** asks for checks on a tree: the landing service (R3.45, R3.14), `reef job submit`, or any client that calls `SubmitJob`. The landing service asks for a batch's missing evidence on a trial commit carrying the batch's tree, one job per kind (so an executor that has only some of the kinds can still claim one), at priority 1,000 and with a 15-minute queue timeout, as `reef-landing` ([landing](landing.md#batches)).
- The **daemon** keeps the queue: it validates requests, leases jobs to executors, checks the evidence they return, and publishes progress. It schedules; it never executes.
- An **executor** claims jobs, runs whatever it runs for the requested kinds and targets, and returns signed evidence.

The daemon never connects to an executor and never sends it a command. A job names a tree, evidence kinds and targets; what runs for them is the executor's decision, from its own configuration or from a declaration in the repository at that tree. An adapter that forwards Reef jobs to a CI system, a remote execution service or a runner fleet is an executor. Reef ships one, `reef executor` ([executor.md](executor.md), [ADR 0060](adr/0060-reference-executor.md)), as a reference and for a laptop or a small CI setup.

## 2. Connecting

An executor is a gRPC client of the daemon, like any other.

- **On the same machine**, dial the daemon's Unix socket: `REEF_SOCKET`, or the default (`reef daemon status` prints `socket`). Every process of the daemon's user is trusted there (the one-uid model, [threat-model.md](threat-model.md)).
- **From another machine** (a CI runner), dial the remote listener over TLS with a `write` token **linked to an operator identity**: `reef remote token create <name> --scope write --identity <ci>`. A token with no link cannot claim jobs (`EXECUTOR_REFUSED`, rule `token_not_linked`), since it could not get the session keys that make its evidence weigh anything.
- Send `reef-agent: <name>` metadata to name yourself in the daemon's logs.

Every error carries `google.rpc.Status` details with a `reef.api.v1.ErrorDetail`: `kind`, `reason` (a constant such as `JOB_NOT_CLAIMED`), `metadata`, `hint`, and sometimes `next`. Branch on `reason`, never on the message.

## 3. The loop

```text
loop:
    response = ClaimJob(executor = {name, version, kinds, labels}, protocol_version = 1,
                        wait = 60s, lease = 60s)
    if response.job is unset: continue            # nothing matched within the wait
    job, token = response.job, response.claim_token
    key = IssueSessionKey(actor = executor)        # one per job; never written to disk
    tree = materialize(job.spec.tree_id)           # section 4; must be exactly that tree
    for each (kind, target) pair of the job:
        run it; upload its artifacts (UploadArtifact) first
        sign a summary claim for the pair (and any finer ones) about tree_id
        UpdateJob(job_id, token, progress, evidence)   # at least every lease / 3
    CompleteJob(job_id, token, outcome = DONE, usage)
on JOB_NOT_CLAIMED from any call: stop work on that job; do not retry
on a lost connection: retry UpdateJob until the lease would have lapsed, then give up
```

The claim token is the capability for one attempt. Keep it in memory; it is never shown again (not by `GetJob`, not in presence, not in logs), and it is worthless once the attempt ends.

## 4. Getting the tree

Any means that yields exactly `spec.tree_id`:

- a git checkout of `spec.commit_id` (set when the job was submitted by revision), checked with `git rev-parse <commit>^{tree}` equal to `tree_id`;
- the repository's own store, for an executor on the daemon's machine (`GetRepository` gives its root);
- `FileService.ListFiles` and `ReadFile` at `spec.commit_id`, a slow fallback that works over the remote listener.

A job submitted by tree alone has no `commit_id`, so an executor that reads through `FileService` cannot get its tree and completes it `FAILED`. A requester that wants any executor to be able to run a job submits it by revision.

Whatever the means, check the tree yourself rather than trusting anyone's word for it. With `FileService`, the recursive listing's entries (path, type and `file_id`, which is the git object id) hash, as git hashes trees, to `tree_id`; each blob's bytes hash, as a git blob, to its `file_id`; and what you laid out on disk can be hashed back to `tree_id` before anything runs. `reef executor` does all three ([executor.md](executor.md#getting-the-tree)).

Evidence must be about `tree_id` and nothing else: the daemon refuses an object about another tree (`JOB_EVIDENCE_MISMATCH`, `field` `tree`).

## 5. Messages and states

### The request: `JobSpec`

| Field | Meaning | Bounds |
| --- | --- | --- |
| `tree_id` | The tree to check: the `gitTree` value evidence about it names | Lowercase hex of the repository's object id length |
| `kinds` | Evidence kinds expected | 1 to 16, `^[a-z][a-z0-9-]{0,62}$`, sorted, unique |
| `targets` | Targets in the executor's own naming; empty for the whole check of each kind | 0 to 1,000, 1 to 512 bytes, no control characters, sorted, unique; `kinds × max(1, targets)` at most 1,000 |
| `budget.wall_time` | How long one attempt may run, from its claim | 1 s to the daemon's `[executor] max_wall_time` (6 h by default) |
| `budget.cpu_time` | CPU time one attempt may use; unset for no limit | 1 s to 7 days |
| `requirements` | Labels the executor must have, `key=value` | 0 to 16; key `[a-z0-9][a-z0-9._-]{0,62}`, value 1 to 128 bytes of visible ASCII |
| `priority` | Higher is claimed first | any `int32` |
| `commit_id`, `change_id` | Set when submitted by revision; context only | |

**Targets are data.** Match them against what you know how to run and pass them as arguments; never build a shell command from them. A request carries no command, script, image or environment, by design (S-75).

### The claim: `ClaimJobRequest`

| Field | Meaning | Bounds |
| --- | --- | --- |
| `executor.name`, `executor.version` | What you call yourself; declared, never verified | name 1 to 256 bytes of visible ASCII and spaces; version at most 64 |
| `executor.kinds` | The kinds you can produce | 1 to 64 |
| `executor.labels` | Your labels; by convention `os` and `arch` as Rust's `std::env::consts` names them | 0 to 32, as requirements |
| `protocol_version` | The highest protocol version you speak | at least 1 |
| `repos` | Only these repositories; empty for all | |
| `wait` | How long to wait for a job | 0 to 60 s |
| `lease` | How long the claim lasts without an `UpdateJob` | 15 s to 10 min; 60 s by default |

A job matches when all its kinds are among yours, all its requirements among your labels, its repository is in `repos` (when given), and its protocol version is at most yours. Among matching queued jobs, the highest priority wins, then the oldest. `ClaimJobResponse.protocol_version` is the daemon's, set with or without a job.

`ClaimJob` is not idempotent: a retried claim may claim a second job, and the first lapses after its lease. A lost response costs one attempt.

A waiting claim holds one of its remote token's 32 request slots. At most 16 claims of one token wait at once; past that, a claim answers at once with no job, so the token's heartbeats and completions always find a slot. A fleet of more than 16 runners should use a token per runner, or per group of runners. A token linked to a revoked identity can still claim; its new session keys are refused, and evidence reads report the identity's status.

An identical open job (same tree, kinds, targets and requirements) is deduplicated only within one submitter: the same remote token, or the socket. Another token's identical request is a job of its own.

### The state machine

| From | To | Caused by |
| --- | --- | --- |
| (none) | `QUEUED` | `SubmitJob` |
| `QUEUED` | `RUNNING` (attempt + 1) | `ClaimJob` |
| `RUNNING` | `QUEUED` | the lease lapsing, or `UpdateJob` with `release`, with attempts left |
| `RUNNING` | `COMPLETED` | `CompleteJob`; `outcome` is `DONE`, `BUDGET_EXHAUSTED` or `FAILED` |
| `RUNNING` | `EXPIRED` (`ATTEMPTS`) | the lease lapsing, or a release, on the last attempt (3 by default) |
| `RUNNING` | `EXPIRED` (`DEADLINE`) | the claim, `wall_time` and 60 s passing |
| `QUEUED` | `EXPIRED` (`QUEUE_TIMEOUT`) | nobody claiming it before `queue_expires_at` (1 h by default) |
| `QUEUED`, `RUNNING` | `CANCELLED` | `CancelJob` |

No state says anything about pass or fail: the evidence does. A requeue keeps the original queue expiry, extended to at least one lease from the requeue.

### `UpdateJob` and `CompleteJob`

Both take `job_id` (the full id), `claim_token`, and up to 50 `SignedEvidence { envelope, public_key }`, exactly `AddEvidence`'s pair. `UpdateJob` also takes `progress` (`phase` and `unit` at most 64 and 32 bytes of visible ASCII) and `release`; it extends the lease. `CompleteJob` takes `outcome` (required), `usage` and a `message` of at most 1 KiB.

A claim check fails with `JOB_NOT_CLAIMED` and metadata `state`: `wrong_token`, `lapsed` (a later attempt or a requeue holds the job), `cancelled`, `completed` or `expired`. The claim is checked before the call's evidence, so a refused claim is reported even when an envelope is also bad. Through the remote listener, only the token that claimed a job may update or complete it (`JOB_NOT_OWNED`), even holding its claim token; a token whose own attempt is over gets `JOB_NOT_CLAIMED` instead, whoever holds the job now. `message` and a cancel's `reason` may contain no control characters (newlines are allowed in `message`). A completion repeated after it succeeded, with the same claim token and evidence the job already accepted, returns the completed job as it is, whatever outcome the repeat names.

## 6. Evidence

Evidence is [R3.01's format](evidence.md): an in-toto Statement v1 with predicate `urn:reef-vcs:evidence:v1` about one `gitTree`, in a DSSE envelope signed once with ed25519. `reef schema evidence` prints its JSON Schema.

- **The binding.** Each object must be about the job's `tree_id` and of one of its kinds; its target may be anything. `JOB_EVIDENCE_MISMATCH` names the first that is not (`index`, `field`, `evidence_id`).
- **All or nothing.** Each envelope goes through `AddEvidence`'s checks; one refused object (`INVALID_EVIDENCE` or `SIGNATURE_INVALID` with `index`, or a mismatch) stores nothing from the call.
- **Artifacts first.** Upload logs and reports with `UploadArtifact` (resumable, by digest) before sending the evidence that names them. The protocol carries no bytes. An artifact never uploaded is not refused; it reads `absent`.
- **Completeness.** `DONE` is accepted only when the job holds, for every requested pair (`kinds × targets`, or each kind with no target when none were requested), an object of exactly that kind and target, from any call of any attempt, including the completing call. Otherwise `JOB_INCOMPLETE` names up to ten missing `kind:target` pairs, and the job stays running. Finer objects (`crates/reef-lib/parser::tests::empty` under `crates/reef-lib`) are welcome beyond the summaries. A summary is your verdict on the whole target: pass only when everything under it passed.
- **`error` versus `FAILED`.** A check that ran and reached no verdict (a crash, a timeout) is evidence with result `error`. `FAILED` is for a job you could not run at all (no tree, a broken runner). Neither blames the change.
- **Freshness.** Each accepted object records `created` (stored for the first time by this call) and `fresh` (created, and its `startedOn` no earlier than five minutes before the claim). A re-sent old envelope is accepted but not fresh; policy may require fresh evidence. An object the job already accepted keeps its first place: re-sending it never makes it the latest word on its pair.
- **Order.** A job's coverage shows, for each pair, the object it accepted last: within a call, the later in the call.
- **Trust.** The daemon checks the envelope, the key, the signature and the binding. It does not decide whether your key is trusted: every read reports the key's identity and status ([identity.md](identity.md)), and the repository's [policy](policy.md) decides what counts. Evidence from an untrusted signer is stored but weighs nothing.

## 7. Budget, cancellation, release, deadlines

- Enforce `wall_time` and `cpu_time` yourself, and complete with `BUDGET_EXHAUSTED` and whatever evidence you have. The daemon's deadline (claim + `wall_time` + 60 s) is a backstop: past it the job expires and your calls are refused (`expired`).
- Send `UpdateJob` at least every third of your lease, and at most once a second. A heartbeat with no evidence is fine.
- A requester may cancel a running job. You learn of it on your next `UpdateJob` (`JOB_NOT_CLAIMED`, `cancelled`). Evidence you already sent stays stored.
- Shutting down: `UpdateJob` with `release: true` gives the job back at once, after storing that call's evidence.
- A daemon restart keeps every job. Running claims get at least one more lease, so an executor that keeps running reconnects and goes on.

## 8. What a trusted executor must do

No protocol can check these; a repository trusts an executor's identity on the assumption that it does them.

- Run where the submitting agent cannot reach: not in its workspace, not as its process, not with its credentials.
- Never run a command or a shell built from request strings. Kinds and targets select from what you were configured to run.
- Use one session key per job, held in memory and never written to disk, with actor `executor`.
- Report `error`, not `pass`, when you cannot tell.
- Check that what you ran on is exactly `tree_id`.

[executor.md](executor.md) describes one way to meet each of these: `reef executor`'s sandbox, its keys, its target rules and its tree checks.

## 9. Versioning

This is protocol **1**. `ClaimJobResponse.protocol_version` and `reef schema` (`reef.executor_protocol`) report the daemon's. The version is separate from the API revision: it moves only when an executor that speaks the old version would do the wrong thing with a new job, and such a job records the version it needs, so `ClaimJob` gives it only to executors that declare at least that version. An added field that an old executor can ignore and still produce correct evidence does not move it. Protocol 1 only grows: no field is removed, renumbered or reinterpreted.

## 10. Conformance checklist

Each rule names the assertion in `crates/reef-daemon/tests/executor.rs` that exercises it (or `crates/reef-api/src/lib.rs` for the schema). R6.03 turns this list into a suite third-party executors run.

- [ ] A request carries no command, script, image or environment: `job_requests_carry_no_command` (reef-api).
- [ ] Specs are validated with the bounds in section 5, each refusal naming its `field`: `submit_and_refusals`.
- [ ] An identical open job is returned (`created` false, priority raised); `duplicate` makes a new one; a finished job does not dedupe; past the cap, `TOO_MANY_JOBS`: `submit_and_refusals`.
- [ ] A claim matches kinds, requirements, repositories and protocol version, by priority then age: `claims_match_and_order`.
- [ ] A waiting claim returns soon after a submit; `wait` 0 returns at once with the protocol version: `claims_match_and_order`.
- [ ] Two claims of one job: one wins: `claims_match_and_order`.
- [ ] `UpdateJob` extends the lease; a lapse requeues with the old token `lapsed`; `release` requeues; the last attempt's lapse expires with `ATTEMPTS`: `leases_lapse_and_restart`.
- [ ] A restart keeps jobs, extends running leases, and republishes presence: `leases_lapse_and_restart`.
- [ ] Queue timeouts and deadlines expire jobs, and a requeue extends the queue expiry: `deadlines_and_queue_timeouts`.
- [ ] Evidence is bound to the job's tree and kinds, all or nothing, at most 50 per call, with `index` on refusals: `evidence_is_bound_to_the_job`.
- [ ] Freshness and artifact states are reported; no link to the change is written: `evidence_is_bound_to_the_job`.
- [ ] `DONE` needs a summary per pair; finer objects do not count; a repeated completion is safe; `BUDGET_EXHAUSTED` and `FAILED` take partial evidence and a message: `completion_needs_every_pair`.
- [ ] Cancellation of queued and running jobs, and of finished ones (`JOB_FINISHED`): `cancellation`.
- [ ] Remote claims need a linked token; updates and completions only through the claiming token; cancels only through the submitting one; no claim token in reads, presence, logs or the database: `remote_rules_and_secrets`.
- [ ] The full loop over the socket and over the remote listener, with progress seen by a presence subscriber and evidence bound to the token's identity: `conformance`.
