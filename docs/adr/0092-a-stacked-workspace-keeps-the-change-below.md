# 0092. A stacked workspace keeps the change below

- Status: accepted
- Date: 2026-11-21
- Tickets: [#380](https://github.com/reef-vcs/reef/issues/380), [#381](https://github.com/reef-vcs/reef/issues/381) ([brief](../dev-tickets/phase-3/issue-380-381-stacked-import.md))
- Amends: [0089](0089-another-clients-undo-keeps-a-workspaces-files.md) (decision 1, for a rebase of a workspace stacked above another change), [0090](0090-reefs-import-keeps-the-commits-workspaces-use.md) (its Consequences on a branch moved back and on the jj CLI's import)

## Context

[0089](0089-another-clients-undo-keeps-a-workspaces-files.md) decision 1 lets a clean workspace follow another jj client's move, or Reef's import of git refs, when the operation recorded the new commit as a rewrite of the old one. [0090](0090-reefs-import-keeps-the-commits-workspaces-use.md) keeps Reef's import from abandoning a commit a workspace uses. Both left a workspace stacked above another change, W2 above W1, exposed:

- **A branch moved back (#380).** When git moves a branch from W1's last snapshot back to an earlier snapshot of the same change, jj's import records the last snapshot as rewritten into the earlier one, and rebases W2's commit onto it. W1 keeps its files by diverging (0089's amendment). W2 followed the rebase and lost the last snapshot's files, from its disk and its commit, with its row not divergent. A pin stops an abandon, not a rewrite.
- **The jj CLI's import (#381).** Any `jj` command at the root first imports git refs with jj's own rules, as its own operation (`import git refs`). After the branch on W1's commit is deleted or pruned, it abandons the commit and rebases W2's commit off it, and W2 followed and lost W1's files.
- **Another client's abandon or revert of the change below.** The same rebase, the same loss; 0089's Consequences accepted following "a rebase that a revert gives a workspace stacked above".

In each, jj recorded W2's rebased commit as made from W2's commit, as every rebase is, so decision 1 counted it. The record says the commit was moved, not that anyone decided anything about it: jj rebases every descendant of a commit that is rewritten or abandoned.

Keeping real jj tags at Reef workspaces' commits, so the jj CLI's import would not abandon them, does not work. jj's default `immutable_heads()` includes `tags()`, so the jj CLI makes a tagged working-copy commit immutable and starts a new commit on it, and it warns on every command that the tag cannot be exported to git (a name git can hold would be exported). It would not stop #380's rewrite either.

## Decision

1. **A rebase counts only when what moved the commit below counts.** Another jj client's move, or Reef's import's, from `was` to `now` counts toward a workspace's adopt and carry when the operation recorded `now` as made from `was`, and each parent of `was` that `now` no longer has was left by a decision. Either the parent is still visible after the operation, so the operation moved `was` off it (`jj rebase -r` of the workspace's own change), or the operation rewrote the parent into a parent of `now`, and that move counts by this same rule (a describe, a `jj restore` or a git amend of the change below). `reef_lib::jj::snapshot::rewrote` holds the rule; `came_from` and the carry's reading of an import use it.
2. **For every other client's move.** The rule reads the operation's recorded predecessors and its view, not its description. It covers the jj CLI's import and `jj git fetch`, and a person's `jj abandon`, `jj op revert` or `jj rebase -r` of the change below. The user chose this over limiting it to imports.
3. **Otherwise the stacked workspace diverges.** ADR [0018](0018-snapshot-staleness-and-divergence.md)'s fourth arm, as 0089 decision 4 says: the snapshot rewrites the workspace's last snapshot, the change is marked divergent, and the directory stays as it is.
4. **Unchanged.** Reef's own moves count as before: `reef abandon`, `reef undo`, `reef move` and a landing are requests made through Reef. A merge of operation heads keeps its rule ([0087](0087-adopting-only-what-came-from-the-last-snapshot.md) decision 2): its rebase follows rewrites recorded in the heads it merged, not in the merge itself, so this rule would refuse every merge's rebase.

## Consequences

- A workspace stacked above another change keeps that change's files when git moves the branch on it back to an earlier snapshot, when the jj CLI's import abandons it, and when another client abandons, reverts or rebases away the change below. Its change is marked divergent with attention `divergent`, beside the rebased commit jj made for it. The agent resolves it as for a single workspace: abandon the commit to drop.
- Following is unchanged for a rewrite of the change below: another client's describe or `jj restore` of it, and a git amend of it, imported by Reef or by the jj CLI. The stacked workspace checks out the rebase, also when the rewrite dropped files.
- A person's `jj rebase -r` of the change below, which takes it out from under the stacked workspace, now diverges the stacked workspace where it followed and removed the change below's files. A `jj rebase` of the stacked workspace's own change, or of the change below with it (`-s`, `-b`), is still followed.
- A merge of operation heads that rebased a stacked workspace over an abandon in one of its heads is still followed (decision 4).
- The rule reads the operation's view and the commits' parents only for a move whose parents changed, and goes down the stack only as far as rebases go. No new state, API, schema or threat-model row.

## Alternatives considered

- **Imports only, recognised by the jj CLI's `import git refs` description.** It would match jj's operation texts, which 0089 decision 3 stopped reading for undo, and miss `jj git fetch` and a person's abandon or revert of the change below.
- **Pins the jj CLI respects** (the issue's first option): real local tags at Reef workspaces' commits. See the Context: the jj CLI would make Reef's commits immutable and warn on every command, and pins do not stop #380.
- **Re-pinning after another client's operation** (the issue's second option): the abandon and the rebase are in the view by then, and undoing them would write over another client's operation.
- **Keeping descendants in place in Reef's import of a branch moved back**, by recording the rewrite as divergent so jj does not rebase them. It covers only Reef's import, and the jj CLI's import still needs this rule.
