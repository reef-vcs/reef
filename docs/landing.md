# Landing

The landing service puts a proposed change onto its destination, a local bookmark (`main` by default), when the repository's [policy](policy.md) at that bookmark allows it ([ADR 0064](adr/0064-landing-service-local.md)). Nothing lands without a policy: a destination with no `reef-policy.toml`, or one that does not load, lands nothing.

```
reef propose @- -m 'ready'                 # draft -> proposed
reef land @-                               # judged, queued, landed (waits up to 2m)
reef land @- --no-wait                     # returns once queued, or refused
reef landing list [--outcome OUTCOME]...   # newest first
reef landing show [ID|REV] [--policy]      # one landing, its evaluation in full
```

`reef policy eval` answers beforehand what a landing would decide now: it previews the landing onto `trunk()` (or `--policy-revision`) and shows the policy the same input.

## What happens

1. **The gate.** The change must be proposed, at the commit its proposal recorded (`reef propose` again after an edit). Its destination must be a local bookmark that exists and is not conflicted. The gate previews the landing at the bookmark's head and evaluates the policy there over the recorded commit. Anything but `allow` is a landing with outcome `blocked`; the change stays proposed. A retry with nothing new counts on the same record (`attempts`).
2. **The queue.** A change the gate allows is queued (the `queued` state, recorded by the daemon), and the repository's landing worker lands queued changes one at a time, oldest first. At most 1,000 changes are queued per repository.
3. **The landing.** The worker judges again at the bookmark's head (the destination may have moved, and its policy with it), then, in one operation: rebases the change onto the head when it is not already on it, gives every workspace sitting on the change a new empty change on top, moves the bookmark to the landed commit, and records the change `landed`. If something stops it, the change returns to `proposed` with a reason.

The landing judges the commit the proposal recorded, which is where the evidence and sign-offs are: evidence binds to a tree ([evidence](evidence.md)).

## Fast-forward, rebase, and what a clean rebase proves

- **Fast-forward**: the bookmark's head is the change's only parent. The landed commit is the change's commit, and the landed tree is the judged one.
- **Rebase**: otherwise the change is rebased onto the head, keeping its change id. A rebase that conflicts writes nothing and refuses with the conflicted paths; rebase it yourself (`jj rebase -d main`), resolve, propose again, and land. A clean rebase lands.
- **Overlap**: the paths both the change and the destination changed since the change's parents. They are in the record and in the policy's input.

A clean rebase is not a test: two changes that merge cleanly can still break each other (one changes a function's signature, the other adds a caller). Until batches test the rebased head (R3.14), a repository that wants every landed tree to be a tested one says so in its policy:

```toml
[[rule]]
name = "fast-forward-only"
expr = '!landing.rebased'
message = "rebase onto main and run the checks again"
```

or, for landings that touch nothing the destination changed, `expr = 'landing.overlap_total == 0'`.

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

Every attempt is a record: `reef landing show` prints it, with the evaluation in full, the policy commit, blob and SHA-256, the operations, and who asked. For a landed change, the evidence its satisfied rules used is kept until the repository is forgotten, whatever `[evidence] retention` says, and linked to the change; the policy text is kept (`reef landing show --policy`); and the change's provenance records are kept whole. Artifacts that evidence names keep their own retention (`[artifacts] retention`), and read as collected once it passes.

## Workspaces on the landed commit

An agent often proposes `@`. After the landing, that workspace sits on a new empty change on top of the landed commit, so its next edits make a new change rather than amending the destination's head.

## What a landing is not

- **Not undoable.** `reef undo` refuses the queue, return and land operations: others build on the destination at once. Land a revert, or move the bookmark back with jj.
- **Not a push.** The landing moves a local bookmark. In a colocated repository, git sees it when the jj CLI next runs. A local `main` that also follows a git host diverges from it after a landing, and a later `jj git fetch` that moved `main@origin` can leave it conflicted: land onto a bookmark nobody fetches into, or wait for R3.17, which pushes landed history.
- **Not safe from other jj writers.** A `jj` command that moves the bookmark while a landing writes can leave it conflicted; the operation log shows both, and `jj bookmark set` resolves it.

## Who may land

On the socket, anyone (one uid is one trust domain, [ADR 0027](adr/0027-trust-boundaries-in-phase-1.md)). Through the remote listener, any `write` token: proposing a change is the request to land it, and the policy decides. Who asked is recorded on the landing and its transitions.
