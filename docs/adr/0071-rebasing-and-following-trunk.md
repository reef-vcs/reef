# 0071. Rebasing, fetching, and following trunk

- Status: accepted
- Date: 2026-11-11
- Ticket: [#259](https://github.com/reef-vcs/reef/issues/259) and [#248](https://github.com/reef-vcs/reef/issues/248) ([brief](../dev-tickets/phase-3/issue-259-reef-rebase.md))

## Context

When `main` moved, an agent had no Reef command to bring its change onto it: agents ran `jj rebase` and `jj git fetch` at the root checkout, which a team without jj cannot do and which acts on the shared root rather than the agent's workspace (#259). Reef imported git's refs but never fetched them.

The other side of the same mechanism was a gate bypass (#248): when R3.09 recorded a change `LANDED` because it reached `trunk()`, a workspace whose working-copy commit was that commit stayed on it, and its next snapshot amended the landed commit; jj moved `main` with the rewrite, past every gate. R3.13 closed this for its own landings (S-121) and left observed landings, and any commit that reaches `trunk()` without a proposal, open.

`Manager::rewrite` ([0031](0031-writes-over-jj-state.md)) already locks, snapshots and follows every workspace a write moves, and carries edits made meanwhile ([0039](0039-carrying-edits-through-a-write.md)). #258's push module ([0070](0070-publishing-to-a-git-remote.md)) already runs the user's git off the repository thread.

## Decision

1. **A rebase is a `Manager::rewrite` write** (`Write::Rebase`) in one operation of kind `rebase`, undoable, keeping change ids. In **branch mode** (the default; `jj rebase -b`, `git rebase main`) it moves every commit of the revision's ancestry the destination lacks, from its roots, with their descendants; in **source mode** (`--source`; `jj rebase -s`) the revision's commit and its descendants. Descendants always move, with their workspaces' directories updated by jj's tree-diff check-out: only files that differ are written.
2. **The destination** is `onto` when given; otherwise `trunk()`, or its remote-tracking bookmark on the fetched remote (or `origin`) when that is a strict descendant of a local `trunk()`. Reef never moves the local bookmark to get there: a git user's local `main` is rarely fast-forwarded, and it may be git's checked-out branch.
3. **A rebase fetches first** unless `skip_fetch`: the destination's branch (trunk's, or a bookmark `onto` names), through jj-lib's `GitFetch` on a transaction started from the repository and **dropped**, off the repository thread, under a per-(repository, remote) lock and `[fetch] timeout`; then the import of the fetched refs is **replayed on the repository thread** with jj-lib's public calls (`import_head_commits`, `index_commits`, `add_heads`, `merge_local_bookmark`, `set_remote_bookmark`) in one operation of kind `fetch`. The fetch and the rebase are two operations: undoing the rebase is exact, and **a fetch is not undoable** (it records what the remote has). A test compares the replay with jj-lib's own import of the same fetch. Tags are not fetched, unreachable commits are not abandoned, and a branch the remote lacks is reported missing and left as it is. A tracked local bookmark is fast-forwarded, except git's checked-out branch in a colocated repository (S-183).
4. **A failed fetch fails the rebase**; the CLI's `next` is the same command with `--no-fetch`.
5. **Conflicts are written, not refused**: the rebase succeeds, jj records the conflict in the commit, the workspace checks out jj's markers, and the response lists each change that introduces one (its first paths; `ConflictService.GetConflict` reads all of it) with `next` `reef conflict show -r <change>`. R3.27 opens the assignment, owned by the rebasing session.
6. **No lifecycle transition.** A rebased `PROPOSED` change reads `amended` until proposed again; a `QUEUED` change is refused (`CHANGE_QUEUED`), because R3.13 lands exactly the queued commit and rebases it itself.
7. **Another agent's live work below the revision is refused** in branch mode (`INCLUDES_WORKING_COPY`), with `next` rebasing that workspace instead, which moves both; changes above always move, with their workspaces.
8. **A landed `@`** (the request's workspace's commit is immutable) starts a new empty change on the destination instead of failing.
9. **Following trunk (#248).** R3.09's observer now also follows active workspaces: when `trunk()`, or its remote bookmark when ahead, moves, every active Reef workspace whose working-copy commit has landed gets a new empty change on it (`Write::StartOnLanded`, no step-3 snapshot, a row update). For a recorded change it happens **in the operation that records the landing**; for any other, in an operation per workspace. A workspace's next snapshot then finds its working copy fresh against the new commit (jj's freshness check compares trees, and the new commit has the landed commit's tree), so edits made meanwhile go into the new change. **Landed** means in `immutable()`, or under `trunk()`'s remote bookmark on `origin` when that is strictly ahead of a local `trunk()`: a git user's local `main` is rarely fast-forwarded, and a change the host merged is landed once fetched.
10. **The new-child carry** (amends [0039](0039-carrying-edits-through-a-write.md)): a write that moves a workspace onto a new empty change on its change's rewritten commit (a landing that rebased it) hands the workspace a carry from its old commit, so edits made during the write go into the new change rather than diverging the landed one.
11. **The snapshot backstop (#248)**: a snapshot never rewrites a landed commit (as in decision 9). When the commit it would rewrite is one, at the scan or again at the commit, on the head it writes to, it records the scan as a new commit on it with a new change id, as the jj CLI does when a working-copy commit becomes immutable; the operation's description says so and `SnapshotWorkspaceResponse.started_on` names the commit. This closes the window before the observer acts, an observer failure, and tags, which do not move `trunk()`. It amends [0018](0018-snapshot-staleness-and-divergence.md)'s arms.
12. **Remote listener**: `RebaseChange` is served to `write` tokens without its fetch (`Rule::NoFetch`, with a note); `FetchGitRemote` is socket only.
13. **Discovery**: `reef rebase` joins the root help (in `ui`'s place), the agent guide's Changes paragraph names it (the guide's budgets rise to 1,800 and 1,750 bytes, amending [0025](0025-agent-facing-cli-contract.md)'s numbers), `reef here` suggests it when `Workspace.behind_trunk` is non-zero, `reef pull`, `sync` and `merge` are usage errors whose `next` is `reef rebase`, `reef rebase -o <not a format>` points at `--onto`, and `reef land`'s and `reef publish`'s advice name `reef rebase` and `reef fetch` instead of jj and git.

## Consequences

- API revision 30: `ChangeService.RebaseChange`, `GitService.FetchGitRemote`, `OPERATION_KIND_REBASE` and `FETCH`, `Workspace.behind_trunk`, `SnapshotWorkspaceResponse.started_on`. No migration. `reef rebase`, `reef fetch`, MCP's `rebase_change` (`tools/list`'s bound rises to 23,350 bytes), and `rebase()` on the SDKs' workspace handles.
- [docs/rebasing.md](../rebasing.md) documents the command; the threat model gains S-177 to S-184.
- A rebase writes only the files that differ in each moved workspace and grows its disk by about their bytes (`rebase_resource`, [benchmarks](../benchmarks.md)).
- The fetch's record depends on jj-lib's import semantics; a jj-lib upgrade that changes them fails `jj_fetch::record_matches_jj`.
- The observer looks at every active workspace when `trunk()` moves: one index ancestry test each, and one tag walk that stops at the commit's position.
- Follow-ups: #202 and #91 (an ancestor's snapshot leaving a descendant stale), a rebase preview, `reef merge`, re-basing a workspace's base cache entry after a large trunk jump, and onboarding scenario G.

## Alternatives considered

- **`jj rebase -s @` as the default**: drops an unlanded parent's edits from the agent's tree.
- **A merge commit instead of a rebase**: jj's and Reef's history are linear by change; a merge is a separate command.
- **Fetch and rebase in one operation**: undoing the rebase would undo the fetch, and the view would disagree with git's fetched refs.
- **Committing the fetch's own transaction off the repository thread**: simpler, but a concurrent operation the thread must merge, which [0012](0012-daemon-process-model-and-threading.md) avoids.
- **Fetching on the repository thread**: a slow remote would hold every snapshot of the repository.
- **Moving the local trunk bookmark on fetch**: moves a person's branch, possibly git's checked-out one.
- **A failed fetch as a note**: an agent would believe it is on the newest trunk when it is not.
- **Rebasing another workspace's change silently, as jj does**: changes another agent's files without its asking (S-177).
- **Re-proposing automatically after a clean rebase**: replaces the reviewers' view without a statement from the agent (S-179).
- **`EmptyBehavior::AbandonNewlyEmpty`**: abandons a change the agent may still be working in; empty changes are reported instead.
- **Only `immutable()` as landed**: with a checked-out local `main` that never moves, a change merged on the host would never count as landed, `reef rebase` would answer "up to date" while `reef here` said "behind", and the next edit would amend what landed.
- **The observer alone, or the backstop alone**: the observer leaves the window before it acts and tags; the backstop alone leaves the workspace on the landed commit, which the landing's operation should move.
- **Refusing the snapshot instead of starting a change**: loses the agent's edits from the record until it acts.
- **Full R3.25 conflict objects in the response**: `conflicts.proto` imports `changes.proto`, so the response could not embed them without a cycle; a summary per change and `next` cost fewer tokens anyway.
- **A flag on `move_workspace` instead of a tool**: about 300 bytes instead of 950, but a different shape from the RPC.
