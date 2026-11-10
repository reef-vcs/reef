# 0070. Publishing to a git remote

- Status: accepted
- Date: 2026-11-09
- Ticket: [#258](https://github.com/reef-vcs/reef/issues/258) ([brief](../dev-tickets/phase-3/issue-258-reef-publish.md))

## Context

A team whose work reaches `main` as pull requests needs each agent's change on a branch of the git remote, and Reef had no way to put it there: agents left Reef for `git push` from the root checkout (#194, question 2). The landing service will push landed history (R3.17), and gitoxide will replace the git subprocess (R4.14), but neither is a review branch, and both are later.

jj-lib pushes through `push_refs`, which spawns the configured git, leases each ref on its expected position, and writes jj's private fetched-ref namespace inside the call. Reef's daemon runs every repository write on one thread per repository under a create lock, and holds no credentials. Its socket is one user's trust domain ([0027](0027-trust-boundaries-in-phase-1.md)); its remote listener serves scoped tokens ([0049](0049-remote-listener.md)). R1.16 marks every snapshot local until it is published.

## Decision

1. **The daemon pushes through jj-lib's `push_refs`**, on a transaction started from the planned repository and dropped, off the repository thread and without the create lock, under a per-(repository, remote, branch) lock. A plan job before it and a record job after it run on the repository thread under the create lock shared. The plan and the push load the repository at its operation over freshly opened stores, because jj-lib's git backend reads the git configuration once: a remote added while the daemon runs must count.
2. **Credentials are the user's git's, in the daemon's environment**, with every prompt disabled (`GIT_TERMINAL_PROMPT`, `GCM_INTERACTIVE`, `SSH_ASKPASS_REQUIRE`, OpenSSH batch mode and keep-alives), HTTPS low-speed limits, tracing off, and a request timeout (`[publish] timeout`). Failures are classified from git's output, the lines jj-lib passes to its callback included, as auth, unreachable, not found, too old or other; stderr is redacted and capped. A push that outlives its request keeps its branch's lock and is recorded if it succeeds.
3. **The branch is named from the change id** (`[publish] branch_prefix`, `reef/` and 12 letters), so an amendment publishes to the same branch, unless the caller names one. The record sets a local bookmark and a tracked remote bookmark at the pushed commit, as `jj git push --change` does, in one operation of kind `publish`.
4. **The range is what jj's CLI checks**: the change's commit and its ancestors that no remote bookmark of the remote reaches. Reef refuses an empty change, and any commit in the range that is undescribed, conflicted, divergent, without author or committer identity, authored by Reef's default identity, or another active workspace's working copy; and more than 1,000 commits.
5. **The lease always applies**, on the remote bookmark's position, "must not exist" when Reef has none. A non-fast-forward update is allowed without `force` only when the branch's commit's change id is the change id of the published commit or of a commit in the range (never "a change in `::C`", which includes trunk). `force` lifts only that rule and the local-bookmark rule; it is recorded (`FORCED`, " (forced)"). Trunk's branch, the remote's HEAD branch, a branch at an immutable commit, and git's checked-out branch are never targets.
6. **A publish is not undoable.** Undoing the bookmarks would leave the push in place.
7. **R1.16's flag is cleared by publish, and derived by the reindex**, by one rule: a snapshot has left the machine when a remote bookmark reaches its commit, or a commit of the same change with the same tree (a describe or sign-off rewrote it before the push); there is no publications table. The operation and its bookmark effects are the record.
8. **No lifecycle transition.** Review on the host is not Reef's review; R3.09's observed landing still records a change that reaches `trunk()`.
9. **One push path.** R3.17 calls the same plan, push and record with a landing mode (the destination's branch, fast-forward only); R4.14 replaces the subprocess inside the push.
10. **Socket only.** `PublishChange` is refused to every remote caller (`Rule::SocketOnly`).
11. **The pull-request command is an argv** from a host table (GitHub, GitLab, Bitbucket, Codeberg, and configured self-hosted GitHub and GitLab hosts), with a URL beside it; clients quote it for `next`. Reef opens no pull request.
12. **`signoff` on describe and publish** adds the daemon identity's `Signed-off-by` trailer once, only when asked, never with the default identity; publishing refuses the default identity altogether (`IDENTITY_UNSET`), and `reset_author` rewrites a commit made before it was set.
13. **No checks gate yet**: no hook runs (`--no-verify`), and a gate evaluated by the repository's policy is a follow-up.

## Consequences

- `GitService` is API revision 29: `PublishChange`, `DescribeChangeRequest.signoff` and `reset_author`, `OPERATION_KIND_PUBLISH`. `reef publish`, `reef describe --signoff --reset-author`, and MCP's `publish_change` (raising `tools/list`'s bound to 22,400 bytes).
- [docs/publishing.md](../publishing.md) documents the command, every refusal, the lease, credentials and the host table; the threat model gains S-166 to S-176.
- Each publish loads the repository again over new stores: a few milliseconds to tens on a large repository, which the push dominates.
- A hung git cannot be killed (jj-lib owns its process); it holds one blocking thread and its branch's lock until it exits.
- Follow-ups: a policy or checks gate, stacked pull requests, remote-listener tokens that may publish, forwarding the client's SSH agent.

## Alternatives considered

- **`push_updates` plus Reef writing jj's fetched-ref namespace**: duplicates jj-lib's private bookkeeping.
- **Pushing on the repository thread**: a slow remote would hold every write to the repository.
- **Running `git push` in the CLI**: the MCP server and the SDKs would each need their own, and the record would not be atomic with the push's view.
- **gitoxide now**: R4.14's work; jj-lib does not push through it yet.
- **Forwarding the client's SSH agent**: a follow-up, needed only for remote callers, which are refused.
- **jj's `push-` prefix**: `reef/` says which tool made the branch and is configurable.
- **An untracked remote bookmark only**: a tracked remote bookmark without a local one reads as deleted to jj, and `jj git push --deleted` would delete the branch.
- **Force-with-lease everywhere**: would let an agent replace any branch Reef has seen, including a person's or a reviewer's commits.
- **Fast-forward only**: every amendment, the normal way to answer review, would need `force`.
- **A `publications` table**: a migration for audit beyond GC's window, which no other write has.
- **Recording `PROPOSED`**: a pull request is the host's review, not Reef's.
- **Any `write` token may publish**: would hand a remote token the user's git credentials and the host's CI.
- **Opening the pull request through the host's API**: Reef would need host credentials.
- **A `--checks` flag**: a flag the agent chooses is not a gate.
- **An `Operation.publication` field**: the bookmark effects already say where it went.
