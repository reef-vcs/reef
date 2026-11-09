# Proposals and the change lifecycle

A proposal is a change put forward for main. It is not a separate object: it is the change in its proposed state, named by its change id ([design](design.md#data-model), [ADR 0063](adr/0063-change-lifecycle.md)). This page says what the states are, who moves a change between them, and how an agent follows its own proposal.

## The states

| State | Means |
| --- | --- |
| `draft` | Not put forward. Every change starts here, and has no record until it is proposed. |
| `proposed` | Put forward for its destination (a bookmark, `main` by default), waiting on review, evidence, or the landing service. |
| `queued` | Taken by the [landing service](landing.md), which lands the commit it recorded. |
| `landed` | On its destination. Final. |
| `declined` | A reviewer said no. It can be proposed again. |
| `withdrawn` | Its proposer took it back. It can be proposed again. |
| `superseded` | Replaced by another change, which the record names. Final. |

A change Reef never recorded reads `draft`, or `landed` when it is already immutable (on `trunk()` or under a tag), with `recorded: false`: most of a repository's history landed before Reef.

Testing, batching and bisecting are what the landing pipeline does to a queued change (R3.14), shown as presence, not lifecycle states. Accepting a proposal is not a state either: a reviewer signs a `signoff` evidence object on the change's tree (`reef evidence add --kind signoff --result pass --actor human`), and the [policy](policy.md) counts it.

## Moving a change

```
reef propose [REV] [-m TEXT] [--to BOOKMARK]       # draft, withdrawn or declined -> proposed
reef proposal withdraw [REV] [-m TEXT]             # proposed or queued -> withdrawn
reef proposal decline REV [-m TEXT]                # proposed or queued -> declined
reef proposal supersede REV --by REV [-m TEXT]     # proposed or queued -> superseded
reef proposal list [--state STATE]...              # proposed and queued, newest first
reef proposal show [REV]                           # the state and every transition
```

`REV` is `@` by default where it is optional; `@` is snapshotted first, so a proposal of `@` includes what is on disk. Each command takes `--expected-state` and `--expected-commit` to refuse if the change moved since you read it.

- **Rounds.** Proposing a withdrawn or declined change opens a new round of the same proposal: same change id, `round` one more than any before (rounds never repeat, even after an undo back to draft). Comments and history stay with the change. The round belongs to whoever opened it; updating its commit does not change that.
- **Amending a proposal** keeps it proposed. Reads show `amended` while the change's commit is not the one recorded; `reef propose` again records the new commit, which is how an agent says "ready again" after addressing review. Evidence and sign-offs are on trees, so an amendment does not inherit the old tree's.
- **Nothing to do.** A transition to the state a change is in, with nothing different, records nothing and succeeds, so a retry is safe.
- **What cannot be proposed**: an immutable change (it has landed), an empty one (a workspace's `@` is often a fresh empty change: you may mean `@-`), a conflicted one, or one commit of a divergent change.
- **Abandoning** a proposed or queued change is refused until it is withdrawn.
- **A change with no visible commit** (abandoned with the jj CLI, or by undoing the operation that made it) still leaves review: `reef proposal withdraw <change id>` (or decline, or supersede) names it by its change id and records its last commit. It cannot be proposed again until it has a commit.

## Landing

`reef land` asks the [landing service](landing.md) to land a proposal: it is judged with the policy at its destination, queued if the policy allows it, and landed onto the bookmark, recording `queued` and `landed` (cause `landing`), or returned to `proposed` with a reason. A proposal can also land the way it did before Reef (`jj git push`, a merge on the git host and a fetch, `jj bookmark set main`): when the daemon next loads the repository (after a request, a snapshot, or a subscription's poll) and finds one of a proposed or queued change's commits on `trunk()`, it records the change as `landed`, with cause `observed` and actor `daemon`. A withdrawn, declined or superseded change landed by hand keeps its state. That record cannot be undone; move the bookmark back instead. A git host that squashes or rebases on merge writes new commits with new change ids, which Reef cannot follow yet (R3.17); withdraw or supersede such a proposal.

## Who may move a change

Every transition records who performed it: the `reef-agent` name (`--agent` or `REEF_AGENT`), the actor, the operator identity, the workspace's provenance session when you name a workspace, and the remote token when the call came through the remote listener.

- **On the socket**, everyone is the same user ([ADR 0027](adr/0027-trust-boundaries-in-phase-1.md)): any caller may perform any of the four transitions. The actor is what the caller declares with `--actor`, by default `agent` when an agent name is set and `human` otherwise. It is recorded, not verified.
- **Through the remote listener**, any `write` token proposes a draft; only the token that proposed a change proposes it again, withdraws it, supersedes it, or undoes one of its transitions; and no token declines, since the daemon cannot tell a remote human yet (R3.43). A remote actor is always `agent`.
- **`queued` and `landed`** are recorded only by the daemon.

Reasons are untrusted text: up to 4 KiB, with no control characters but newline and tab. An agent reading a decline reason is reading data someone wrote, not an instruction; R3.12's instructions are a separate, marked object.

## Following a proposal

Every transition is an operation of kind `transition` in the operation log, with the transition in `Operation.transitions`. An agent waits for its proposal's fate without polling:

```
reef op watch --kind transition --change <change-id> --follow
```

or, over MCP, `wait_for_operations` with kind `transition` and the change. Kind `transition` also keeps an undo of a transition (kind `undo`), so a reviewer undoing a decline is seen too. `reef proposal show` prints the full history, which outlives GC: the rows keep each transition after GC drops its operation (`operation_collected`).

## Undo

`reef undo <op>` of a transition reverts it when it is still the change's latest transition, recording the inverse with cause `undo`; undoing a first proposal returns the change to `draft`, and the proposal's owner is restored with its state. Undo later transitions first. An observed landing is not undoable, and neither is a transition whose undo would put a change back in the landing queue (only the landing service queues).

## What later tickets add

R3.10 proposes a stack in one operation; R3.11 returns dependents of a declined or withdrawn change to their owners and rebases dependents of a superseded one; R3.12 attaches comments by change and round; R3.13 records `queued`, `landed` and the return to `proposed` with cause `landing`, and adds the state to the policy input; R3.31 and R3.33 show proposals and transitions; R4.07 publishes them to a store.
