# 0014. Workspace model on jj

- Status: accepted
- Date: 2026-10-16
- Ticket: R1.01

## Context

A Reef workspace is a directory leased to an agent ([Workspaces](../design.md#workspaces)). Reef needs many of them per machine, often several on one change, each cheap to create (under a second on a 100,000-file tree), cheap to reclaim, and, to the agent, nothing but a directory: the agent edits files and Reef snapshots them. The daemon owns the inventory ([0003](0003-sqlite-for-daemon-metadata.md)) and builds on jj-lib ([0002](0002-build-on-jj-lib.md), [0008](0008-jj-lib-embedding-findings.md)).

jj already has workspaces: several working copies of one repository, each with its own working-copy commit, named in the view. `jj workspace add` creates one by writing `<root>/.jj` (a pointer to the repository and the working-copy state under `.jj/working_copy/`), checking out a new commit on the root commit, and registering the root in jj's workspace store. jj-lib 0.46.0 also exposes the pieces separately, verified for R1.01: `LocalWorkingCopy::init` and `load` take any directory for the working-copy state, `Workspace::new` builds a workspace from a root, a repository path, a working copy, and the repository's loader, `MutableRepo::edit`, `check_out`, and `remove_workspace` move the working-copy pointer, and `WorkspaceStore::add` and `forget` register a root, which need not contain `.jj`.

## Decision

We will make every Reef workspace a jj workspace with its own working-copy commit, assembled from those pieces in `reef_lib::jj::workspace` rather than through `jj workspace add`.

- **Names.** The jj workspace is named `reef-<workspace id>`. The user's own workspace at the registered root stays `default` and is never leased, watched, reclaimed, or forgotten by Reef.
- **State outside the tree.** The working-copy state (`checkout`, `tree_state`, and jj's `working_copy.lock`) lives in the daemon's state directory at `<state>/repos/<repo-id>/wc/<ws-id>/`. The workspace directory holds only the tree: no `.jj`.
- **Starting points.** A workspace either edits an existing commit (`MutableRepo::edit`, so the workspace is on that change) or starts a new empty commit on one (`MutableRepo::check_out`). Creating it is one jj operation. Resolving user input to a commit is `Repo::resolve_revision`: a change id prefix (hex or jj's reverse hex), then a commit id prefix, then a local bookmark name; no revsets.
- **Registration.** The workspace root, canonicalized, is registered in jj's workspace store, and release forgets it there and removes the working-copy pointer (`MutableRepo::remove_workspace`, which abandons an empty working-copy commit and keeps a non-empty one visible), as `jj workspace forget` does.
- **Seeding.** A new working copy records the empty tree. The caller seeds its `tree_state` from the base it materialized (R1.02, R1.05) or calls `Repo::reset_to`, which marks every file unknown so the next snapshot rehashes the tree and writes nothing if it matches.

## Consequences

- `jj workspace list` in the user's checkout shows Reef workspaces next to `default`; `tests/jj_workspace.rs` checks this with the jj 0.46.0 CLI when it is on `PATH`.
- A Reef workspace directory is not a jj repository: `jj status` inside it fails. That is by design until R2.05 gives agents the API; agents use Reef, not jj, from inside a workspace.
- Backends clone, copy, or overlay only the tree, and an agent cannot delete the working-copy state by cleaning its directory.
- Two workspaces editing one change both rewrite its commit, and jj then sees a divergent change. ADR 0018 (R1.13) says how Reef handles it; R1.01 only observes it.
- jj's working-copy lock file lives in the state directory, so two daemons cannot share one state directory, which R1.03's single-instance lock already prevents.
- A Reef workspace's working copy is fresh only relative to the operation it last recorded. Reopening it after other workspaces wrote operations gives a stale working copy, which R1.13's staleness policy resolves.
- If jj-lib changes `LocalWorkingCopy::init` or `Workspace::new`, only `reef_lib::jj` changes ([0008](0008-jj-lib-embedding-findings.md)'s seam).
- The records that describe Reef workspaces (registered repositories, workspace and lease rows, the reclaim log, and their ids) are the daemon's inventory, not part of the version-control data model that [0007](0007-crate-layout.md) places in `reef-lib`. They live in `reef_daemon::model`, next to the SQLite store that holds them.

## Alternatives considered

**`.jj` inside the workspace (`jj workspace add`).** The supported path, and `jj` would work inside the directory. Rejected: every backend would have to create or clone `.jj` per workspace, adding to the one-second budget, and an agent that cleans its directory could delete it and orphan the workspace.

**One jj workspace per change with a Reef-side overlay.** Fewer jj workspaces, with Reef tracking each directory's differences from a shared working copy. Rejected: it reimplements jj's working copy (scanning, file state, ignores) in Reef.

**A child commit per workspace.** Every workspace starts a new commit on top of the change it was asked for, so workspaces never share a commit and never diverge. Rejected: it changes what "a workspace on a change" means; the agent's edits land in a different change, and combining them is left to the user.
