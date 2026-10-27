# 0040. GC and the user's jj workspaces

- Status: accepted
- Date: 2026-10-27
- Ticket: issue #94

## Context

R1.17's GC abandons the operations older than the undo window by rewriting the newer ones onto the root operation (jj-lib's `reparent_range`), so every kept operation gets a new id. Reef moves its own active workspaces to the new head; R1.17 left the jj workspaces it does not own, the registered root's `default` and any `jj workspace add`, for `jj workspace update-stale`. Their `.jj/working_copy/checkout` still names the old id of their last operation. The operation store's GC then prunes unreachable operation files by mtime: the old operation survives when it is younger than the window, but its older ancestors do not. jj cannot load it (the index walks its ancestors), `update-stale` fails the same way, and jj's recovery for a *missing* operation never runs. A root that ran `jj` within the window, in a repository with older history, is broken by its first GC: the normal case for a colocated checkout, and what happened on this repository's first real `reef ws gc`.

Reproduced with the `jj` 0.46.0 CLI alone: `jj op abandon ..<op>` plus `jj util gc` from a second workspace leaves the first in the same state. jj's own `op abandon` moves only the workspace it runs in.

The issue offered two fixes: point each workspace at its operation's rewrite, or delete the old ids of rewritten operations so jj's recovery applies. The second is not a general fallback. A subscription behind the rewrite reads the old copies until it catches up ([0036](0036-operation-log-subscriptions.md) item 9), and so does a `jj` command that loaded the old head before the rewrite; deleting them would end the one and break the other.

## Decision

1. **Which workspaces.** GC moves every jj workspace in the head view not named `reef-*` (the user's), found at the `Repo`'s own root when it is that workspace and otherwise at the path jj's workspace store records. One is moved only when that directory has a `.jj/working_copy/checkout`, loads as a local working copy of this same repository, and its checkout names it (checked before locking too, so a directory another name took over is not mistaken for a busy one) with a valid operation id. Anything else is skipped with a warning and GC goes on; refusing GC for every directory the user deleted without `jj workspace forget` would block it forever.
2. **Under jj's lock, taken first.** GC takes each one's `working_copy.lock` with `try_lock` before loading the head it classifies at, polling for up to 2 seconds in all, and holds them until the op heads have moved and the checkouts are written. A workspace added between the first load and the locks is locked in another round. If a lock stays busy (a `jj` command running) and there is something to abandon, GC abandons nothing that run, still collects what is already unreachable, and reports `no operations were abandoned: jj workspace … is locked by another process`. Skipping only the busy workspace would strand it. Reef also gives the lock file the checkout's owner, so a root daemon leaves nothing the user's `jj` cannot open.
3. **Where each goes.** With X the operation its checkout names:
   - X was rewritten: it names X's rewrite, which has the same view, so jj answers exactly as before GC (fresh, or stale and asking for `update-stale`, which now works).
   - X is the root operation, or an operation GC keeps unchanged: nothing.
   - X was abandoned, by this GC or an earlier one (a 0.1 GC, a crash), and the workspace's recorded tree is its commit's at head: it names the head, which jj's staleness check would call fresh anyway.
   - The same, with a tree that is not its commit's: X's operation file is deleted, so jj's recovery for a missing operation applies (`update-stale` makes a recovery commit of the files as last recorded). Nothing reaches X any more, so no subscription or `jj` command can be reading it.
4. **Reef writes the checkout itself**, as jj-lib's `CheckoutState::save` does (a temporary file persisted over `checkout`), keeping the old file's owner and mode. It cannot use jj's `start_mutation`, which blocks on the lock Reef already holds. Only the operation pointer changes; the view, the commit, `tree_state` and the files do not. To learn each operation's rewrite, Reef runs its own copy of `reparent_range`, which returns the old-to-new map jj's drops; operation ids are content hashes, so it writes what jj's would.
5. **Checked again before collecting.** jj holds the lock while it snapshots and while it checks out, not while its transaction commits, and its check-out writes the operation it committed without looking at what the checkout names. A `jj` command that committed before GC loaded the head and was waiting on GC's lock therefore writes its operation's old id once GC lets go. After releasing the locks and running the callback, GC takes each lock again and moves a checkout that names a rewritten operation onto the rewrite. A lock still busy then fails GC before the collection, which keeps the old operations loadable.
6. **A failed write** still runs the caller's callback (Reef's own workspaces move) and then skips the collection, index rebuild and `git gc`, returning the error: the unpruned ancestors keep the old operation loadable, and the next GC repairs the workspace or hands it to jj.
7. **No schema change.** Moves are logged; the case the user must act on (a busy lock) is in `GcReport.jj_error`. `API_REVISION` stays 5.

## Consequences

- Amends [0031](0031-writes-over-jj-state.md)'s "the registered root's `default` workspace is never written by Reef" and R1.17's "the user's `default` workspace is not touched": GC writes the operation pointer of the user's jj workspaces, and nothing else of them. Reef's writes still never write `default`.
- A `jj` command in the root waits while GC holds its lock: the rewrite writes one file per kept operation. One that loaded the repository before the op heads moved then finds its checkout on an operation it does not know and fails with jj's sibling-operation error instead of writing; running it again works.
- Reef depends on jj's on-disk `checkout` format, its simple operation store's layout and a copy of `reparent_range`. A jj-lib upgrade re-checks all three; `reparent_mapped_matches_jj` catches a drift in the copy.
- A stale root whose operation GC abandons gets jj's recovery commit at its next `update-stale` instead of an ordinary stale update. It needs the root idle in jj longer than the undo window and its `@`'s tree changed meanwhile.
- Roots a 0.1 GC stranded are repaired by the next GC when their files match, else handed to jj's recovery.
- A busy lock defers the whole abandon to the next run, which matters for a GC timer (#95).
- The check before collecting narrows the race with a running `jj` command but does not close it: a command that committed before GC loaded the head and reaches its check-out only after the check still writes an old id, and the collection prunes that id's ancestors (#94's state again). The next GC repairs it (`ToHead` when the files match). A `jj` command that writes without the working-copy lock (`--ignore-working-copy`) races GC the same way it races `jj util gc`.

## Alternatives considered

- **Delete the old ids of rewritten operations as the general fix.** Breaks subscriptions catching up and a concurrent `jj` writer; used only for operations nothing reaches.
- **Skip only a busy workspace.** Abandons its operation's ancestors and strands it: the bug itself.
- **Block on jj's lock (`start_mutation`).** A stopped `jj` would wedge the repository thread.
- **Move every user workspace to the head.** Hides staleness: jj would call a stale root fresh and its next snapshot would overwrite the newer commit.
- **Refuse to abandon past a stale root, or past the root's operation at all.** Blocks GC forever for a root nobody runs `jj` in.
- **A `GcReport` field listing the moves.** A schema change for information needed only when something goes wrong; logs carry it.
