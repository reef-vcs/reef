# 0031. Writes over jj state

- Status: accepted
- Date: 2026-10-26
- Ticket: R2.03
- Amended by: [0039](0039-carrying-edits-through-a-write.md), [0040](0040-gc-and-the-users-jj-workspaces.md)

## Context

R2.01 ([0029](0029-daemon-api-v1.md)) declared the write side of `reef.api.v1` (`DescribeChange`, `AbandonChange`, `UndoOperation`) and R2.02 ([0030](0030-reads-over-jj-state.md)) built what they stand on: revisions resolved to one commit, `Change`s, effects, and a classifier that reads `reef.kind` first. Neither said how a write meets the live workspaces it rewrites.

Every describe, abandon or undo rewrites commits that workspaces may be sitting on while their agents keep editing and the watcher keeps snapshotting. A snapshot scans on the blocking pool and commits on the repository thread; if another write lands between the two, the snapshot rewrites a commit that is no longer the view's working-copy commit and the change diverges (ADR [0018](0018-snapshot-staleness-and-divergence.md)). Phase 1 met this once, in `MoveWorkspace`, which holds the one workspace it moves. Divergence itself is a state ADR 0018 left for this ticket to resolve. And jj's undo (Phase 1's `undo_last`, [0008](0008-jj-lib-embedding-findings.md)) restores the head's parent, which undoes only the latest operation and, because it snapshots first, often undoes that snapshot.

## Decision

1. **Every operation Reef records carries `reef.kind`**, and `reef.workspace`, `reef.agent` and (for a snapshot) `reef.snapshot-kind` where they apply, through one `OpMeta` that every `reef_lib::jj` write takes. jj-lib's own `initialize repo`, `add workspace 'default'` and `reconcile divergent operations` stay unattributed and classify as `EXTERNAL`; `import git head` at registration is `OTHER`. GC copies attributes with the operations it rewrites.
2. **`reef.agent` is the request's `reef-agent` metadata, else the workspace's owner.** Visible ASCII of at most 256 bytes, read by every RPC that may record an operation, `INVALID_AGENT` otherwise. It names who caused a write; it is not authentication (one uid is one trust domain, ADR [0027](0027-trust-boundaries-in-phase-1.md); R2.08 binds remote callers to tokens).
3. **Writes hold the workspaces they move still**, in five steps on the manager: plan (resolve and check on the repository thread, listing the jj workspaces whose working-copy commit the write moves); lock (the repository's create lock shared, then each active Reef workspace's automatic snapshots paused and its scan lock taken in id order, as GC does); snapshot (abandon and undo only, one workspace at a time, descendants first, so a commit is rewritten before an ancestor's snapshot rebases it under its scan); write (resolve and check again, then one operation); follow (each moved workspace takes its new commit: a row update when the tree is unchanged, else a snapshot that finds the working copy stale and clean and checks the tree out). A snapshot that fails in step 3 stops the write before anything is written. A revision naming a commit that step 3's snapshot rewrote follows it to the new commit, or, with `expected_commit_id`, is `EXPECTED_COMMIT_MISMATCH` naming the new commit. Snapshots record the request's agent, else their own workspace's owner; the write's own operation falls back to the named workspace's owner. Reads after the commit (the answer's `Change`, the working copies to follow) only degrade the answer when they fail: the write has happened.
4. **Describe** rewrites one commit's description, adding a trailing newline as jj's CLI does, and rebases descendants with unchanged trees. It takes no snapshot (it changes no tree) but still holds the locks: without them, a describe of `@` landing between a snapshot's scan and its commit diverges the change. Text already equal records nothing.
5. **Abandon of a divergent change resolves it.** When the abandoned commit's change has another visible commit that does not descend from it (the newest by committer time, then commit id, when there are several), the abandoned commit is recorded as rewritten into that one: descendants are rebased onto it, bookmarks move to it, working copies edit it, and the effects report the abandoned commit `HIDDEN`. Otherwise abandon is `jj abandon`: descendants onto the parents, local bookmarks deleted, working copies on one new empty commit. A divergent change id alone is `AMBIGUOUS_REVISION`; the caller names the commit to drop.
6. **Undo reverts exactly one operation, as `jj op revert` does**: a three-way merge of the views from the operation back to its parent onto the head, descendants rebased. It refuses what it cannot revert exactly. Never undoable (`OPERATION_NOT_UNDOABLE`): the root, a merge of concurrent operations, one that adds or forgets a jj workspace (Reef's rows would disagree with the view), a Reef move (it also swapped bases and stores), and one that changed git refs or git's `HEAD` (Reef never writes git's). `OPERATION_NOT_IN_HISTORY` for one the head never saw. `OPERATION_SUPERSEDED` when anything it changed has changed again: a commit it made visible is gone, one it hid is back, or a bookmark, tag or working copy it moved has moved since. That check runs after step 3, so a workspace's unsnapshotted edits supersede the operation instead of being overwritten.
7. **Rows' divergent marks follow the write**, read from the head after the follows, whose snapshots may themselves diverge. A moved workspace is marked divergent exactly when its change now is (with attention `divergent` if it had none); any other active row marked divergent whose change no longer is has the mark cleared. A resolving abandon clears both workspaces' marks; undoing it marks the one it moved back.
8. **Errors**: `INVALID_COMMIT_ID`, `INVALID_DESCRIPTION`, `EXPECTED_COMMIT_MISMATCH`, `IMMUTABLE_REVISION`, `HIDDEN_REVISION`, `OPERATION_NOT_UNDOABLE`, `OPERATION_NOT_IN_HISTORY`, `OPERATION_SUPERSEDED` and `INVALID_AGENT`, each with its metadata. `expected_commit_id` must be a full id: a prefix would weaken the check. No schema change; API revision 2.

## Consequences

- A write's latency includes a scan of each workspace it moves (abandon and undo), before and after. Abandoning an ancestor of many busy workspaces waits for all their scans; the answer is late, not wrong.
- Edits that reach the disk between a write's step 3 and its follow-up meet ADR 0018's fourth arm: the change diverges, visibly, and nothing is lost. For the first workspace step 3 snapshots, the window runs through the later workspaces' scans, the write, and the follows before its own, so it grows with the number of workspaces moved and their scan time.
- Undo is stricter than `jj op revert`: agents undo newest first, and an edit after a describe blocks undoing the describe until the edit's snapshot is undone. R3.26 replaces the refusal with conflict objects.
- Abandon deletes local bookmarks on a commit that is not divergent, as `jj abandon` does; undo restores them. In a colocated repository git keeps the ref until the `jj` CLI next exports.
- The registered root's `default` workspace is never written by Reef. A write that rewrites its working-copy commit leaves it stale for the `jj` CLI (`jj workspace update-stale`), as any other workspace's rewrite does.
- The successor rule leans on jj-lib's `update_wc_commits` taking the new commit for a rewritten working copy and on `update_heads` hiding every commit in the parent mapping; reef-lib's tests pin both.
- This settles ADR 0008's open points on undo (Reef no longer uses `undo_last`; it reverts one operation, keeps git refs and `HEAD`, and refuses operations it cannot revert) and ADR 0018's on resolving divergence.

## Alternatives considered

**jj's `undo` (restore the head's parent).** Only the latest operation can be undone, and with a snapshot first that is usually the snapshot.

**Plain abandon for a divergent change.** Strips the change from under its descendants and moves the losing workspace off the change, to a new empty one, instead of onto the surviving version.

**`jj op revert` without the supersession check.** Merges whatever changed since, which usually leaves the reverted change divergent: divergence as a side effect of undo.

**The create lock taken exclusively instead of scan locks.** Simpler, but every write would queue behind slow creates and moves of unrelated workspaces.

**Abandoning without snapshotting first.** A workspace's unsnapshotted edits would then meet a stale working copy with edits and resurrect the abandoned commit through ADR 0018's diverge arm.

**Snapshotting the moved workspaces concurrently.** Faster, but a workspace whose commit descends from another's is rebased under its own scan when the ancestor commits first, and diverges.
