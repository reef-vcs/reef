# 0079. Overlaps from open changes' paths

- Status: accepted
- Date: 2026-11-18
- Ticket: issue #263 ([brief](../dev-tickets/phase-3/issue-263-overlaps.md))

## Context

Two agents editing the same file learn about it when a rebase or a landing conflicts. Reef knows earlier: each workspace's change is a jj commit recorded at every snapshot, and presence ([0037](0037-presence-channel.md)) lists the paths each snapshot edited. But an agent has to ask (`reef presence watch`, which has no MCP tool), and the commands it already runs, `reef status`, `reef here` and MCP `get_status`, said nothing.

The issue fixed three inputs. The signal is computed from persisted state, the changed paths of each open change, not from presence, which is ephemeral and coalesced under load. Status stays answered from the status index ([0048](0048-status-from-the-watchers-journal.md)) with no tree walk. And it decides whether released workspaces' unlanded changes count. Its acceptance: two workspaces change one file and each one's status names the other within one snapshot interval; `status/large/index_*` stay inside their gates; with no overlap, brief output ([0045](0045-brief-json-by-default.md)) gains no bytes.

## Decision

1. **`Workspace.overlaps` and `overlap_count` (API revision 35), and an `Overlap` message**: the other change's id and commit, a Reef workspace on it (an active one, else the newest released one) and its owner, or the commit author's email when no Reef workspace has been on it, whether it is active, up to 10 shared paths in path order with their count, and its committer time. At most 10 changes, newest written first. On `Workspace`, as `behind_trunk` is, so status, `here` (which flattens the workspace), `ws inspect`, MCP `get_status` and `get_workspace` carry it through one field. Set by `GetStatus` at head and by `GetWorkspace` for an active workspace; empty in `ListWorkspaces` and at an operation.
2. **Computed on each read, from the view the read holds.** Inside `GetStatus`'s existing read job, after the workspace's diff; in a job of its own for `GetWorkspace`, beside `behind_trunk`. Best effort: a failure is logged and leaves the list empty.
3. **The candidates are `mutable() ~ (::@ | @::)`, less `::main@origin` when the local trunk lags it.** Ancestors are the workspace's base and descendants are work stacked on it, so neither is another line of work; commits on `main@origin` past a lagging local `main` have landed, though Reef's `mutable()` holds them. Commits on the workspace's own change are dropped (a shared change, a divergent copy), and each other change counts once, by its newest commit. At most 1,000 commits are read, working copies first, so a fetch of many commits never pushes an active workspace's change past the cap. A workspace whose change is empty reads no candidate.
4. **Which count.** Active Reef workspaces' changes (a `reef-<id>` working copy on the change in the view, whose workspace the database says is active) always; any other open change, a released workspace's, the root checkout's or a fetched one, only if its committer time is within `[overlaps] max_age` (7 days; `0s` for active workspaces only). Hidden and landed changes are not `mutable()`.
5. **A commit's changed paths come from a cache keyed by its id.** A commit id hashes its tree and parents, so its diff against its parents merged never changes and the cache needs no invalidation, only bounds: 10,000 paths per commit (the rest is cut, and a path past a cut list is asked of that commit's tree), 250,000 in all, after which it is cleared whole. A miss is one tree diff with no copy detection and no content read. Two reads missing one commit at once both compute it; the lock is never held across a diff. The workspace's own paths come from the same cache, not from status's diff, which detects copies and so leaves out a rename's source.
6. **Paths are matched in jj's order** (by component: `a/b` before `a.rs`), the smaller list looked up in the larger.
7. **A path whose content in the other change equals trunk's does not count** (`trunk()`, or `main@origin` when it is ahead): that change adds nothing there over trunk, so rebasing either cannot conflict on it. It covers a squash-merged change in the days before trunk edits the file again; older ones are dropped by the age bound.
8. **Nothing is computed with no candidate.** A status in a repository whose only open commit is its own does one revset and no diff.
9. **The CLI.** `reef status` and `ws inspect` text print up to three lines, `Also changing <path> (+N more): workspace <id> (<owner>); reef diff -r <change>`, then `... and N more`. `reef here` adds one `overlaps` line and one `hint` sentence naming the newest overlap; its `next` is unchanged. The brief view of `Workspace` keeps `overlaps` (each with `change_id_reverse_hex`, `workspace_id`, `owner`, `active`, `paths`, `path_count`) and `overlap_count`, both left out when empty, so a workspace with no overlap prints the same bytes as before.

## Consequences

- An agent learns of an overlap at its next `status` or `here`, within one snapshot interval of the other's write, before a rebase or a landing conflicts. Conflicts still do not block; the hint says to read the other change or carry on.
- A status with nothing to find costs one revset over the mutable commits. With open work, one cache lookup per candidate and a tree diff per commit not seen before; after a daemon start the first read diffs every candidate once.
- A rebase rewrites committer times, so rebasing an old stack brings its changes back inside `max_age`.
- Reef's `immutable_heads()` is `trunk() | tags()`, so untracked remote bookmarks are mutable: a bot's open branch (a dependency update touching `Cargo.lock`) counts within `max_age`, named by its author, and so does a merged branch's remote ref until a fetch prunes it. Commits on `main@origin` past a lagging local `main` are landed, and are left out (decision 7's trunk is that remote bookmark then).
- A squash-merged change whose file trunk later edited again counts until `max_age` passes.
- When both changes are cut (more than 10,000 paths each), paths past both cuts are not compared.
- The cache holds at most 250,000 paths, about 20 MB, shared across repositories.

## Alternatives considered

- **The history index's `change_paths`** ([0044](0044-history-index-and-query-layer.md)): persisted and indexed by path, but its worker computes a busy change's paths about once a minute, which misses one snapshot interval. An eager path half would change its worker for a second consumer.
- **Presence's editing entries**: ephemeral, lost at a restart, coalesced under load, and they list what each snapshot edited, not what the change changes.
- **A SQLite table of paths per open change**, or an index kept by a follower of the operation log: a second copy of what jj's commits say, kept equal at every rewrite (rebase, abandon, land, move, undo, GC). A follower moves work off the read path but runs after every operation whether anyone reads; it stays the next step if the read-time cost shows up under load.
- **The workspace's paths from status's diff**: copy detection leaves out a rename's source, so status and `GetWorkspace` would disagree.
- **Symbol-level matching and an event when an overlap begins**: left to follow-up issues. A symbol diff of both changes costs too much on the status path, and an event moves the computation onto every snapshot.
