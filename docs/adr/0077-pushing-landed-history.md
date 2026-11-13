# 0077. Pushing landed history to a git remote

- Status: accepted
- Date: 2026-11-13
- Ticket: R3.17 ([brief](../dev-tickets/phase-3/R3.17-degraded-landing-to-a-git-remote.md))

## Context

The roadmap promises phase three "a degraded form against a git remote": the landing service lands locally, a team on GitHub sees landed history, and evidence, provenance and conflicts stay on the machine. R3.13 ([0064](0064-landing-service-local.md)) and R3.14 ([0067](0067-batching-and-bisect.md)) land onto a local bookmark only, and R3.16 ([0075](0075-landed-history-projection.md)) made that bookmark the projection a team should read. #258 ([0070](0070-publishing-to-a-git-remote.md)) built the one push path (plan, push on a dropped transaction under a lease, record) and refused trunk's branch on purpose: "Landing on `main` is `reef land` and R3.17." #259 ([0071](0071-rebasing-and-following-trunk.md)) built the fetch, which merges a tracked local bookmark as jj's import does.

Two facts shaped the design (the brief's spikes). A push with a stale lease to a remote already at the target commit reports `[up to date]` and succeeds, so a push whose record was lost is repaired by pushing again. A fetch that merges a tracked `main@origin` that someone else moved leaves the local `main` conflicted, and R3.13 refuses every landing onto a conflicted destination.

The direction since 2026-11-09: agents are the users, git remotes and workflows stay compatible, and Reef is not hooked further into git inside workspaces. Landing stays local-first: a push failure never undoes or blocks a landing.

## Decision

1. **The destination's policy names the remote**: `[landing] push = "<remote>"` in `reef-policy.toml`, a git remote's name, never a URL. The remote branch is the destination's own name. Where landed code goes changes only through a policy change the owners sign off (S-146). The operator's `[publish] enabled` and `remotes` apply, and `[publish] landings = false` turns off only landing pushes.
2. **A push worker per (repository, destination), after the land operation.** Each landed prefix wakes it; it pushes the destination's head and loops while the head moves, so a batch is one push and landings during a push coalesce. It never holds the landing worker's claim, the lifecycle lock or the repository thread across the network. `reef land` waits, within its own wait, for the push that carries its landing and reports it; its exit code is the landing's.
3. **One push path.** The daemon's `publish` module's locked push-and-record (the per-branch lock, `[publish] timeout`, the `stuck` lock, R1.16's marking) runs both publishes and landing pushes. `reef_lib::jj::push::plan_landed_push` plans a landing push without #258's branch rules (a destination is trunk and immutable by definition), with the range `::H ~ ::(the remote's bookmarks | root())` capped at 1,000 commits and checked for conflicts only. Undescribed changes (`builtin:description` on a pushing destination) and a default `[identity]` (`IDENTITY_UNSET` at the gate) are stopped at the gate, where they can still be fixed.
4. **Lease, fetch, classify; never force.** A rejected lease, or a remote the view knows is ahead, makes the worker fetch the branch (through #259's fetch) and plan again: up to date (recorded), rewound (pushed again), remote ahead (followed fast forward by the fetch, unless the policy files differ: `REMOTE_POLICY_CHANGED`, S-201), or `diverged`, which pauses pushing while landings go on locally and gives a person both ways out.
5. **Every Reef fetch follows a pushing destination fast forward only.** `record_fetch` leaves it where it is rather than merge it into a conflict, tracked or not, checked out or not. The landing push's record keeps the remote bookmark's tracking state (untracked when new) and never touches the local bookmark.
6. **A pushing destination's batch fetches first** (at most 30 s; skipped while the remote is known to be unreachable and within 30 s of the last fetch or push; a failure is a log line), so a merge on the host is landed on rather than diverged from.
7. **Retries with backoff, no polling.** A failure that may pass retries at 30 s doubling to 15 minutes, and at 15 minutes for authentication or a missing git; a refusal or divergence waits for a landing, `reef landing push` or a start. The start sweeps every destination with a push row or an unpushed landing.
8. **The record.** Schema 20 (`0020_landing_pushes`): `destination_pushes` (one row per destination: state, last failure with redacted stderr, hint and next, the retry schedule, the last push), `landings.pushed_at` and `push_operation_id`, and `landing_batches.history` so a landing finished at start records its `[history]` (#297). The push is a `publish` operation (`reef: push landed <dest> to <remote>/<dest> (<n> commits)`), not undoable; GC maps its id.
9. **The surface.** API revision 34: `LandingService.ListPushes` (read, paged) and `PushLanded` (write, socket only), `PushState`, `DestinationPush`, `LandingPush` and `Landing.push`, and `GetDaemonStatusResponse.pushes_attention`. `reef landing push|pushes`; a push line in `reef land`, `reef landing show` and, when a person must act, `reef here`; a count in `reef daemon status`. One MCP tool, `push_landed`: an MCP agent has no landing tool and otherwise cannot tell whether its landed work reached the team's remote.
10. **Remote callers.** A landing asked through the remote listener pushes, since the signed policy, not the caller, decides where (S-200); `PushLanded` is socket only.

## Consequences

- A team on GitHub sees each landing on the destination's branch, as the projection wrote it, while evidence, provenance and the operation log stay local.
- A merge on the host is followed; a remote someone else rewrote is reported as `diverged` and never overwritten. Recovering from divergence is a person's step until re-landing exists (#304).
- A squash merge on the host is not recognised as a change's landing yet (#305); published review branches stay until deleted (#306). A host that requires signed commits refuses the push (`REMOTE_REJECTED`) until #296.
- A jj user's own `jj git fetch` on a tracked `main@origin` can still conflict the destination; `docs/landing.md` says to untrack it.
- R4.14 replaces the git subprocess under both the fetch and the push; the landing mode, the worker and the record stay. R4.16 exports the same destination.

## Alternatives

- **Pushing inside the landing, before moving the bookmark** (a merge queue whose truth is the remote): a push failure would block local landing, which the direction rules out.
- **A synchronous push in `LandChange`**: an offline laptop would wait the timeout on every landing.
- **Daemon configuration or `reef.toml`** for the remote: the first is per machine, not per destination; the second is repository content any change may edit without the owners.
- **A branch name override**: one more way to aim landed history at a branch someone else owns.
- **Re-landing onto a diverged remote**: the rebased trees were never judged; it needs R3.14's batch machinery against a new base (#304).
- **Blocking landings while diverged**: the person's fix is the same, and agents would stop on a problem they cannot fix.
- **`--force`**: would erase what someone else pushed.
- **Tracking the remote bookmark as #258 does**: jj users' fetches would then merge it into `main`.
- **One row per attempt, or no table**: the operation log already holds the history of pushes; state in memory would be lost at a restart.
- **No MCP tool** (the brief's recommendation): the user chose to give MCP agents `push_landed`; `tools/list` grew by 632 bytes.
