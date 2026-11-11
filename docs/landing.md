# Landing

The landing service puts a proposed change onto its destination, a local bookmark (`main` by default), when the repository's [policy](policy.md) at that bookmark allows it ([ADR 0064](adr/0064-landing-service-local.md)). Nothing lands without a policy: a destination with no `reef-policy.toml`, or one that does not load, lands nothing. Queued changes land in batches, and the required checks run again on the tree they land as ([ADR 0067](adr/0067-batching-and-bisect.md)).

```
reef propose @- -m 'ready'                 # draft -> proposed
reef land @-                               # judged, queued, landed (waits up to 2m)
reef land @- --no-wait                     # returns once queued, or refused
reef landing list [--outcome OUTCOME]...   # newest first
reef landing show [ID|REV] [--policy]      # one landing, its evaluation in full
reef landing batches [--open]              # batches, newest first
reef landing batch [ID|REV]                # one batch: its members and the trees it tested
```

`reef policy eval` answers beforehand what a landing would decide now: it previews the landing onto `trunk()` (or `--policy-revision`) and shows the policy the same input.

## What happens

1. **The gate.** The change must be proposed, at the commit its proposal recorded (`reef propose` again after an edit). Its destination must be a local bookmark that exists and is not conflicted. The gate previews the landing at the bookmark's head and evaluates the policy there over the recorded commit. Anything but `allow` is a landing with outcome `blocked`; the change stays proposed. A retry with nothing new counts on the same record (`attempts`).
2. **The queue.** A change the gate allows is queued (the `queued` state, recorded by the daemon), and the repository's landing worker lands queued changes one at a time, oldest first. At most 1,000 changes are queued per repository.
3. **The batch.** The worker takes queued changes for one destination together (below), judges each again at the bookmark's head (the destination may have moved, and its policy with it), and rebases them one on another onto it. Under `checks = "batch"`, the required checks must also pass on the tree the batch lands as: the executor is asked for whatever evidence that tree lacks.
4. **The landing.** In one operation: rebases the changes onto the head (none for a change already on it), gives every workspace sitting on a landed change a new empty change on top, moves the bookmark to the last landed commit, and records each change `landed`. If something stops a change, it returns to `proposed` with a reason.

The landing judges the commit the proposal recorded, which is where the evidence and sign-offs are: evidence binds to a tree ([evidence](evidence.md)).

## Fast-forward, rebase, and what a clean rebase proves

- **Fast-forward**: the bookmark's head is the change's only parent. The landed commit is the change's commit, and the landed tree is the judged one.
- **Rebase**: otherwise the change is rebased onto the head, keeping its change id. A rebase that conflicts writes nothing and refuses with the conflicted paths; rebase it yourself (`reef rebase --onto main`, [rebasing](rebasing.md)), resolve, propose again, and land. A clean rebase lands.
- **Overlap**: the paths both the change and the destination changed since the change's parents. They are in the record and in the policy's input.

A clean rebase is not a test: two changes that merge cleanly can still break each other (one changes a function's signature, the other adds a caller). That is why, under `checks = "batch"` (the default), the required evidence must also pass on the rebased tree (below). A repository can still refuse rebases outright:

```toml
[[rule]]
name = "fast-forward-only"
expr = '!landing.rebased'
message = "rebase onto main and run the checks again"
```

or, for landings that touch nothing the destination changed, `expr = 'landing.overlap_total == 0'`.

## Batches

A **batch** is the queued changes for one destination landed together: `bat_` and 16 hex digits. The worker takes them oldest first, up to the policy's `batch_max`, and rebases them as a **chain**: the first onto the destination's head, each next one onto the one before. The chain's tree after each change is a **prefix**; the last is the batch's **head tree**. A change that conflicts with an earlier change of the chain (but not with the destination) waits for the next batch, which judges it against what landed.

```toml
[landing]                 # in reef-policy.toml, at the destination's head
checks = "batch"          # batch (default) | proposal
batch_max = 8             # 1 to 32
batch_wait = "0s"         # wait this long after the oldest was queued for more, up to 10m
alone = ["migrations/**"] # changes touching these land in a batch of their own
```

A change that touches `reef-policy.toml` or `reef-checks.toml` always lands in a batch of its own: changes after it must be judged by the policy it lands, and checked with the checks it declares.

### What is checked on the batch's tree

With `checks = "batch"`, each change is judged twice at landing: its own submitted tree with every rule, as the gate did, and its `[[require]]` rules again over the evidence on the **tested tree**: the head tree, or the prefix bisection lands. Sign-offs, limits, forbidden paths and free rules are about the change and its own tree, and are not judged there.

**The tested tree's evidence counts for every change at or below it.** That is a merge queue's assumption: the trees below the head were not themselves tested, and each landing says which tree was (`reef landing show`: `tested`). Set `batch_max = 1` if every landed tree must be exactly a tested one.

The worker asks an executor only for what is missing. A tree that already has passing trusted evidence (a fast-forward batch of one lands as its own tree, which passed the gate; a tree tested before) needs no job. Otherwise it writes a **trial commit**, a new change on the destination's head whose tree is the tested tree (`reef: batch bat_… prefix 3 of 3 onto main`), and submits one job per evidence kind on it ([executor protocol](executor-protocol.md)). Trial commits show in the log while their batch is open and are abandoned when it ends; `reef op log --kind batch` lists the operations that wrote them.

With `checks = "proposal"`, only the submitted trees are judged, as before batches: for a repository whose trusted evidence comes from somewhere other than executor jobs, such as a CI system signing with its own `ci` key.

### Bisect

When a required check's newest trusted verdict on the head tree is a failure, the batch searches for the first change that breaks it: it tests the middle prefix, then the middle of whichever half failed, at most ⌈log2 n⌉ more trees (five for 32 changes). The prefix that passed lands; the first change whose prefix failed returns to `proposed` with reason `checks` and the failing evidence; the changes after it stay queued and lead the next batch. Each prefix is judged by its own changes' requirements: a check only a later change requires does not run on an earlier prefix.

A flaky check can blame the wrong change. The newest verdict on a tree wins, so landing that change again (a passing run on the same tree) lands it.

### When no executor answers

A job that ran but gave no verdict (it failed, ran out of time or attempts, or returned only `error` or untrusted evidence) is asked for once more. A job nobody claims within 15 minutes, or a second failure, makes the batch `unavailable`: its tested changes return to `proposed` with reason `unavailable`, which is never the change's fault. `reef land` says whether any executor has claimed a job of the repository in the last day, and how to go on: run one (`reef executor run`), or, if this repository's evidence does not come through executor jobs, set `[landing] checks = "proposal"` in `reef-policy.toml`.

### Moves, departures and restarts

Nothing is locked while checks run. Every few seconds, and whenever a job changes, the batch checks the destination's head and its changes. If the destination moved, or a change was withdrawn, amended or landed some other way, the batch cancels its jobs, abandons its trial commits and forms again (outcome `reformed`): a tree tested before is not tested again. After three batches in a row re-formed because the destination moved, their changes return with reason `busy`. A daemon restart finishes an open batch `interrupted`; the next one reuses its trial commits, jobs and evidence.

| Batch outcome | Means |
| --- | --- |
| `landed` | Every change landed. |
| `split` | A prefix landed (possibly none), the culprit returned with `checks`, the rest stay queued. |
| `unavailable` | No verdict could be had; the tested changes returned with `unavailable`. |
| `reformed` | The destination moved or a change left; the rest form the next batch. |
| `interrupted` | The daemon stopped; the next batch forms from the queue. |
| `busy` | The destination moved under three batches in a row. |

A change whose parents have not landed is refused (`unlanded_parents`): land the parent first. Stacks land together with R3.10.

## Outcomes and reasons

| Outcome | Means |
| --- | --- |
| `blocked` | The gate refused it. The change is still proposed. |
| `queued` | Waiting for, or in, the worker. |
| `landed` | The bookmark moved to the landed commit. |
| `dequeued` | Queued, then returned to proposed. |
| `cancelled` | Someone withdrew, declined or superseded it while it was queued, or it landed some other way. |

| Reason | What to do |
| --- | --- |
| `policy` | The evaluation says which rules blocked and what they lack: add the evidence or sign-off, then land again. |
| `conflicted` | Rebase the change onto the destination yourself, resolve, propose again. |
| `unlanded_parents` | Land the parent first. |
| `amended` | The change moved since it was proposed: `reef propose` again. |
| `destination` | The bookmark is gone or conflicted: fix it, or propose for another with `reef propose --to`. |
| `busy` | The bookmark moved three times while the landing tried: land again. |
| `failed` | An internal error, in `message`. |
| `checks` | A required check failed on the batch's tree with this change and passed without it: fix it, propose again, land again. `reef landing batch` shows the trees and the failed checks. |
| `unavailable` | No executor gave a verdict on the batch's tree: land again, run an executor, or set `[landing] checks = "proposal"` (above). |

`reef land` exits 0 when the change landed (or was queued, with `--no-wait`), 5 when it was blocked or dequeued (reason `LANDING_BLOCKED` or `LANDING_DEQUEUED`, with what to do in `hint`), and 6 when it is still queued as the wait ends (`LANDING_PENDING`).

## Rejected evidence

The policy evaluation is the structured refusal. For every evidence pair a `[[require]]` rule misses, it lists the objects of that kind and target that were on the tree and did not count, and why:

| Why | Means |
| --- | --- |
| `signature_invalid` | The stored object no longer verifies. |
| `error` | The check gave no verdict. |
| `key_unbound`, `key_expired`, `key_revoked`, `key_invalid` | The signing key's status when the daemon witnessed the object. |
| `self` | Signed in the change's own provenance session or by its agent. |
| `presence` | The key's presence is below the attestor set's. |
| `condition` | An attestor set's `when` was false for it. |
| `untrusted` | No attestor set of the rule accepts its signer. |

## What a landing keeps

Every attempt is a record: `reef landing show` prints it, with the evaluation in full (and the evaluation on the batch's tested tree), the policy commit, blob and SHA-256, the operations, the batch, and who asked. For a landed change, the evidence its satisfied rules used, on its own tree and on the tested tree, is kept until the repository is forgotten, whatever `[evidence] retention` says, and linked to the change; the policy text is kept (`reef landing show --policy`); and the change's provenance records are kept whole. Artifacts that evidence names keep their own retention (`[artifacts] retention`), and read as collected once it passes.

## Workspaces on the landed commit

An agent often proposes `@`. After the landing, that workspace sits on a new empty change on top of the landed commit, so its next edits make a new change rather than amending the destination's head. In a batch this holds for every landed change, not only the last.

## What a landing is not

- **Not undoable.** `reef undo` refuses the queue, return and land operations: others build on the destination at once. Land a revert, or move the bookmark back with jj. It refuses a batch's operations too: withdraw a change to take it out of its batch.
- **Not a push.** The landing moves a local bookmark. In a colocated repository, git sees it when the jj CLI next runs. A local `main` that also follows a git host diverges from it after a landing, and a later `jj git fetch` that moved `main@origin` can leave it conflicted: land onto a bookmark nobody fetches into, or wait for R3.17, which pushes landed history. A team that merges through pull requests publishes each change to a review branch instead, with `reef publish` ([docs/publishing.md](publishing.md)).
- **Not safe from other jj writers.** A `jj` command that moves the bookmark while a landing writes can leave it conflicted; the operation log shows both, and `jj bookmark set` resolves it.

## Who may land

On the socket, anyone (one uid is one trust domain, [ADR 0027](adr/0027-trust-boundaries-in-phase-1.md)). Through the remote listener, any `write` token: proposing a change is the request to land it, and the policy decides. Who asked is recorded on the landing and its transitions.
