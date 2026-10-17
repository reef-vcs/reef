# 0018. Snapshot staleness and divergence

- Status: accepted
- Date: 2026-10-17
- Ticket: R1.13

## Context

The daemon snapshots every workspace on every API call and on a short timer driven by filesystem notifications (`docs/design.md`, Workspaces, "Always snapshotted"). Before each snapshot it has to decide what to do when the working copy and the loaded repository disagree. [0008](0008-jj-lib-embedding-findings.md) left that open: R0.04's `Repo::snapshot` returned `Stale` whenever the working copy's operation differed from the loaded one.

jj-lib classifies the disagreement with `WorkingCopyFreshness::check_stale`:

- `Fresh`: the operations match, or the repository moved on without rewriting the working-copy commit's tree.
- `Updated`: the working copy is newer than the loaded repository (another process snapshotted).
- `WorkingCopyStale`: an operation the working copy never saw rewrote its working-copy commit.
- `SiblingOperation`: the two operations diverged from a common ancestor.

jj-cli reloads at the working copy's operation on `Updated`. On `WorkingCopyStale` it refuses and asks the user to run `jj workspace update-stale`, which checks the rewritten commit out, or, when the commit is gone, makes a recovery commit (`create_and_check_out_recovery_commit`).

Three facts about Reef change the answer.

- **One writer per repository.** Every transaction the daemon writes runs on the repository's thread, which reloads at head after each one ([0012](0012-daemon-process-model-and-threading.md)). `Updated` and `SiblingOperation` therefore only arise when the `jj` CLI runs on the registered root.
- **Workspaces share changes.** Several agents may work on one change (`Create { change }`; [0014](0014-workspace-model-on-jj.md)). When workspace A snapshots, `MutableRepo::rebase_descendants` ends in `update_wc_commits`, which moves every workspace pointing at the rewritten commit to A's new commit ("We arbitrarily pick a new working-copy commit among the candidates"). Workspace B's next snapshot then sees `WorkingCopyStale`.
- **Nobody is at the keyboard.** An agent cannot run `update-stale`, and a workspace that refuses to snapshot leaves its edits unprotected until a human notices.

## Decision

We will apply four arms after `check_stale`, in `reef_lib::jj::Repo::begin_snapshot`:

1. **`Fresh` → scan** and plan a rewrite of the view's working-copy commit.
2. **`Updated` or `SiblingOperation` → `SnapshotError::RepoBehind`.** The daemon reloads the repository thread at head (which merges divergent operation heads) and retries once. A second `RepoBehind` is logged at `warn` and retried on the next trigger.
3. **`WorkingCopyStale` with the scanned tree equal to the working copy's old tree** (no edits since its last update) → **adopt**: check the view's working-copy commit out on disk and save the working-copy state at the head operation. No operation is written. Nothing of the workspace's own is lost, so following the rewrite is correct.
4. **`WorkingCopyStale` with local edits → diverge**: plan a rewrite of the commit the workspace last snapshotted (`workspaces.commit_id`) instead of the current pointer. The change becomes divergent. The daemon sets `divergent = 1` and `attention = 'divergent'`. The workspace never adopts another agent's tree over its own edits, and nothing blocks.

A fifth case is not a freshness answer. If the view has no working-copy commit for the workspace at all (someone ran `jj workspace forget`), the daemon **recovers**: it creates a new commit from the scanned tree on the parents of the commit the workspace last snapshotted, with a new change id (jj forgot the old one, and reusing it would diverge silently). It points the workspace at that commit and sets `attention = 'stale-recovered'`.

Divergence is also detected at commit time. If another workspace rewrites the commit between this workspace's scan and its transaction, the transaction still rewrites the commit it scanned against, and the snapshot is reported as divergent (`SnapshotCommit::divergent`).

*(addition)* Before each snapshot the repository thread reloads at head if the operation heads have moved, so operations written outside the daemon are seen. That costs one listing of the operation heads.

## Consequences

- Divergent changes are a normal, visible state in Phase 1: `Inspect` and `List` show `divergent` and `attention`, and the `jj` CLI shows them with `??`. R2.03 provides the operations to resolve them.
- A snapshot never fails because of what another workspace did. Its own edits always reach the store.
- The policy depends on `MutableRepo::rewrite_commit` accepting a hidden predecessor. A jj-lib bump that rejects it breaks the daemon's `stale_diverges_when_dirty` test, which is there to catch that.
- `Repo::snapshot` (one process, no daemon) follows the same policy. With no recorded commit, it takes the commit the workspace last snapshotted from the view of the working copy's own operation.

## Alternatives considered

**jj's recovery commit** (`create_and_check_out_recovery_commit`). It moves the workspace onto another agent's tree and turns the workspace's own edits into a child commit nobody asked for. An agent that edited `src/lib.rs` would find its change rebased under someone else's work without being told. Rejected.

**Refuse to snapshot a stale workspace** (jj-cli's default). An agent's edits would be unprotected until a human ran `update-stale`, and an expired lease would delete them. That breaks "deleting a workspace loses nothing". Rejected.

**Child-commit semantics for every workspace.** Each workspace would always work on a new child of the change it was created on, so two workspaces never share a commit and never diverge. Then "workspace on change C" would mean "on a child of C", which neither the CLI nor the design says, and every finished workspace would need a squash. Rejected for Phase 1. It is the fallback if divergence proves unmanageable in practice.
