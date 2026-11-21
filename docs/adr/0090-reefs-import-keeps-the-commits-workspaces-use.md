# 0090. Reef's import of git refs keeps the commits workspaces use

- Status: accepted
- Date: 2026-11-21
- Ticket: [#374](https://github.com/reef-vcs/reef/issues/374) ([brief](../dev-tickets/phase-3/issue-374-import-keeps-used-commits.md))
- Amends: [0089](0089-another-clients-undo-keeps-a-workspaces-files.md) (its amendment of 2026-11-21: Reef's import no longer abandons a workspace's commit, so the case it decides arises only when the jj CLI imports first)

## Context

Reef imports git refs before most writes (`refresh_for_resolve`), as the jj CLI does before each command in a colocated repository. The import uses jj's `git.abandon-unreachable-commits`, true by default: a commit git used to reach and no longer does is abandoned, and its descendants are rebased onto its parents. jj decides reachability from local bookmarks, local tags and untracked remote refs only. A working-copy commit is not protected; with the jj CLI 0.47.1, deleting the git branch on the default workspace's commit abandons it and removes its files from the root checkout.

For Reef that abandon is wrong. A git branch on an agent's snapshot is deleted after review, pruned by `git fetch --prune` after a squash merge, or reset with `git update-ref`, while the workspace is still working. [0089](0089-another-clients-undo-keeps-a-workspaces-files.md)'s amendment (#368) kept a single workspace's files by diverging. Three problems remained (#374):

- A workspace on a change above the abandoned one followed the import's rebase, which recorded a rewrite, and lost the lower change's files from its disk and its commit.
- The first write after the deletion runs the import itself. The import moved the workspace's `@` to the new empty commit jj makes for an abandoned working copy, and the write acted on that commit: a describe described it, a publish refused it as empty.
- The row said divergent while the change showed one visible commit beside a stray empty head, and `reef here` told the agent to abandon a commit.

The flag cannot simply be turned off. In jj-lib 0.47 it also decides whether the import records synthetic rewrites, and those are what let a workspace follow a git amend of its snapshot ([0089](0089-another-clients-undo-keeps-a-workspaces-files.md)'s amendment).

## Decision

1. **Every import Reef runs pins the working-copy commits.** `Repo::import_git_refs` sets a local tag at every working-copy commit in the view for the length of the import's transaction, named `reef:pin:<workspace>` (a name git cannot hold), and removes the tags before the transaction commits. jj's abandon keeps every commit a working copy is on or descends from and abandons the rest, as before. Every jj workspace counts, the root checkout's `default` included. The tags never reach git or a committed view. A user's `git.abandon-unreachable-commits = false` still turns the abandon off.
2. **jj's synthetic rewrites are unchanged.** A git amend of a snapshot is still followed and carried. A branch moved back from a snapshot to an earlier snapshot of the same change is still recorded as a rewrite into the earlier one; the workspace moves there and keeps its files by diverging, as [0089](0089-another-clients-undo-keeps-a-workspaces-files.md)'s amendment decides.
3. **A kept commit stays visible for good.** Release does not abandon it, like any released workspace's change today. Nothing records which commits an import kept.
4. **It is an open change.** The history index keeps it `OPEN`, and `reef here` says what it said before the branch went away.

## Consequences

- Deleting, pruning, squash-merging away or resetting the git branch on an agent's change no longer moves any workspace: not the agent's, not one stacked above it. The first write after it acts on the agent's change, and nothing diverges.
- After the workspace is released, the change stays in `reef log` until someone abandons it. A squash merge on a forge, fetched, lands a new change and leaves the agent's open, as [0044](0044-history-index-and-query-layer.md) already says for a fetch.
- Moving `main` back with git after it landed a workspace's change leaves that change open, where the import abandoned it.
- The jj CLI's own import still abandons with jj's rules. When it runs first, [0089](0089-another-clients-undo-keeps-a-workspaces-files.md) keeps a single workspace's files by diverging, and a workspace stacked above still follows the rebase.
- The import evaluates one more set of revisions, the working-copy commits' ancestors, inside the revset jj already evaluates. No new state, API, schema or threat-model row.

## Alternatives considered

- **Importing with `abandon_unreachable_commits: false` and abandoning in Reef**, as #374 proposed. jj-lib ties synthetic rewrites to the same flag, so a git amend would leave two visible commits of the change. Reef would have to copy jj's private `record_synthetic_predecessors` to keep following amends.
- **Importing with the flag on, and again with it off when a working copy would be abandoned.** The second import records no rewrite, so a git amend in the same import as a deletion would not be followed.
- **Abandoning kept commits at release.** It would match what jj would have done, at the cost of a table of kept commits keyed by change (snapshots rewrite them after the import), a GC sweep, and the risk of hiding work done after the branch went away. The user chose to keep them.
