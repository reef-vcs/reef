# 0080. `reef here` walks the change's lifecycle

- Status: accepted
- Date: 2026-11-18
- Ticket: issue #262 and issue #327 ([brief](../dev-tickets/phase-3/issue-262-here-lifecycle.md))
- Amends: [0025](0025-agent-facing-cli-contract.md), [0031](0031-writes-over-jj-state.md)

## Context

`reef init` gives an agent one instruction: run `reef here` to learn where it is and what to do next. Until this ADR, `here` stopped at getting into a workspace. Its `next` was release, renew, rebase or a divergence listing. The guide named `describe`, `rebase` and `publish` but not `propose`, `land` or `conflict`. So an agent learned how to start work and not how to finish it, although proposals ([0063](0063-change-lifecycle.md)), stacks ([0078](0078-stacks.md)), the landing service ([0064](0064-landing-service-local.md)) and conflict objects all exist. Many agents reach Reef only through MCP or an SDK and never run `reef here`.

Separately, `DescribeChange` never snapshotted (#327). A file written just before `reef describe -m` was missing from the commit the describe named, until the watcher's next snapshot added it to a new commit of the same change.

## Decision

1. **The daemon names the step.** `Workspace.step` (`ChangeStep`) says what a workspace's change needs next to be finished. `Workspace.lifecycle` and `Workspace.conflicted_path_count` give what the step was read from, and `changed_file_count` is filled too. `GetWorkspace` sets them for an active workspace, read at head from the working-copy commit. They stay unset elsewhere and at an operation, as `behind_trunk` does. MCP's `get_workspace`, `reef ws inspect`, the SDKs and the web UI's workspace page all see the same value.
2. **The step table**, first match wins:

   | When | Step |
   | --- | --- |
   | the commit has conflicts | `resolve` |
   | queued | `wait` |
   | proposed, amended since | `propose` |
   | proposed, onto a destination whose policy lands automatically, while the daemon works toward landing it | `wait` |
   | proposed | `land` |
   | declined | `revise` |
   | landed, superseded, or immutable | `done` |
   | nothing changed | `work` |
   | a blank description | `describe` |
   | the repository lands through proposals (draft or withdrawn) | `propose` |
   | otherwise | `publish` |

3. **A repository lands through proposals** when `trunk()`'s tree has `reef-policy.toml` (nothing lands without one), when any change of it has a recorded lifecycle, or when the change's own lifecycle is recorded. Otherwise it is suggested `reef publish`, so a repository that uses Reef only for workspaces is not sent down a path it does not use.
4. **`sync_first`.** `GetWorkspaceRequest.sync_first` syncs through the status index ([0048](0048-status-from-the-watchers-journal.md)) before the read. If nothing changed since the last scan, it takes no snapshot. Otherwise it snapshots only the changed paths. `reef here` and MCP `get_workspace` set it, because a step read from a stale snapshot is wrong exactly when an agent has just written. This amends [0025](0025-agent-facing-cli-contract.md)'s rejection of a snapshotting `here`: the index costs about 2 ms when nothing changed, and records an operation only when there is a write to record.
5. **`reef here` renders.** Health comes first, in the order it had: lost lease, divergence, attention, grace, low disk. Then conflicts. Then being behind trunk, but only while a rebase fits the step (not proposed, queued, declined or done). Then missing dependencies and a warning disk. The step comes last. Its `hint` and `next` per step:
   - `describe`: `reef describe -m`, a prefix like `ws run --`, since the text is the agent's.
   - `propose`: `reef propose`.
   - `land`: `reef land`, which waits for any checks the daemon asked for, then lands it or reports what is missing.
   - `wait`: `reef op watch --kind transition --change <id> --count 1 --for 120`, then `reef here` again. The watch ends at the change's next transition or after two minutes (`reef land`'s default wait), so an agent that follows `next` rereads the step instead of waiting on a transition that never comes. A proposed change's hint says the daemon lands it and that `reef proposal show` says what it waits for.
   - `revise`: `reef proposal show`.
   - `resolve`: `reef conflict show`.
   - `publish`, `done` and `work`: release, or nothing inside `ws run` or a held workspace.
6. **No reviewer's or author's text in the step's advice** (S-216). The step's `hint` sentence and its `next` are built from Reef's own sentences, ids, counts, the round and bookmark names. A decline reason, a description or a comment never enters them. A declined change's `next` is `reef proposal show`, which marks the reason as untrusted. #263's overlap sentence, which may follow, names another change's owner and a shared path; S-216 accepts that.
7. **Brief views** ([0045](0045-brief-json-by-default.md)). `here`'s brief form keeps `id`, `path`, the change id, `changed_file_count`, `step`, `hint` and `next`, and what is set or present (state, flags, conflicted count, disk, push, overlaps). `json::Change` gains `subject`, its description's first line. `status`'s brief view keeps the trunk's and the parents' `subject` instead of their `description`, which a squash-merged pull request makes kilobytes long.
8. **The guide** names the finishing path in its Changes paragraph. Its short part says `reef here` gives the next step to finish. Its budgets rise to 1,900 and 1,850 bytes, from 1,800 and 1,750 ([0071](0071-rebasing-and-following-trunk.md)). The finishing path is what an agent set up by `reef init` most lacked, and `here` carries the per-state detail, so one paragraph is the whole cost. The MCP server's instructions gain one sentence naming `get_workspace`'s `step` and the finishing tools.
9. **A describe of `@` syncs first** (#327). `DescribeChange` snapshots through the status index when its revision uses `@`, as `TransitionChange` and `PublishChange` already did. The rewrite itself still takes no snapshot under its locks ([0031](0031-writes-over-jj-state.md)), since the sync before it covers every write made before the call.

## Consequences

- An agent on any surface can follow a change from the first write to landing by reading `step` (or `here`'s `next`) after each command.
- `GetWorkspace` does one more read job at head: a summary diff of the working copy against its parents, a conflicted-path scan, one tree lookup in `trunk()` and one indexed row check. It runs alongside `behind_trunk` and the overlaps.
- `reef here` may record a snapshot operation, which it never did before.
- The step is for the workspace's own change. `reef propose` and `reef land` take the stack below it themselves ([0078](0078-stacks.md)).
- When the destination's policy lands automatically (R3.45, [0081](0081-automatic-landing.md)), a proposed change reads `wait` while the daemon works toward landing it with no command: its automatic status is `pending`, `waiting` or `queued`. The step reads that status, which records the proposal's destination, commit and whether the destination lands automatically, so `GetWorkspace` evaluates no policy. The status counts only when its destination and commit are the lifecycle's, so one left from an earlier proposal of the change says nothing. Once the service gives up (`needs`, `returned` or `no_executor`: a failed check, evidence it does not ask for under `checks = "proposal"`, a conflict, a returned landing), nothing lands without someone acting, and the step reads `land`: `reef land` reports what is missing. Until the service's first pass over a new proposal (a moment after `reef propose`, or after a daemon start), the step also reads `land`, and `reef land` then waits for the same checks. A status is as fresh as the last pass, which every operation in the repository starts, so a policy that stops landing automatically can read `wait` until that pass ends; the watch's two-minute bound covers it.

## Alternatives considered

- **Compute the step in `reef here` only**, from `GetStatus` and two probes. Rejected by the user: agents on MCP and the SDKs would never learn how to finish.
- **Evaluate the policy for a proposed change** and list the missing evidence in the hint. Rejected: one policy evaluation per `here`, repeating what `reef land` reports.
- **Quote the decline reason in the hint** after "untrusted:". Rejected: `hint` is what an agent reads as advice (S-216).
- **`next` = `reef publish`** for a described change in a repository without proposals. Rejected: pushing to a team's remote is the operator's call, and `next` is something agents run.
- **A "your change landed" step from a landed parent.** Rejected: a workspace created on a trunk head that someone else landed through Reef has the same parent.
