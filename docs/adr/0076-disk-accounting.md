# 0076. Disk accounting: one walk per workspace, a floor that reads statvfs first

- Status: accepted
- Date: 2026-11-13
- Ticket: [#272](https://github.com/reef-vcs/reef/issues/272) ([brief](../dev-tickets/phase-3/issue-272-disk-accounting.md)); closes [#146](https://github.com/reef-vcs/reef/issues/146)
- Amends: [0073](0073-laptop-resource-budget.md)'s decisions 7 and 11

## Context

ADR 0073 decided that the daemon keeps one disk figure per workspace from one capped walk, that a free-space floor refuses creates and store builds but never a snapshot, a release or a GC, that a soft budget warns, and that agents see the level only when it is not `ok`; and that #272 excludes the default workspace root from Time Machine and Spotlight. Experiments while briefing #272 (D1 to D7 in the brief) corrected several of those choices, and the API settled the rest. This ADR records them; [docs/disk.md](../disk.md) is the user's description.

## Decision

1. **One walk per workspace, told apart by the working copy.** A file the workspace's working copy tracks (its `tree_state`, as of the last snapshot) is tracked, and counts when its mtime or size differs from the base's; anything else is build output. Matching `.gitignore` again would cost more and could disagree with jj. `unshared_bytes` is APFS's private size (an unedited clone costs 0, D4; build output is 97% unshared, D2), an overlay's upper directory, or a copy's every file, and is unknown on reflink filesystems (a `FIEMAP` per file would be needed). The figure also lists the three largest untracked directories (`largest`) and #195's missing dependencies.
2. **On macOS the walk is `getattrlistbulk`**, which returns each entry's type, mtime, sizes and private size in one call per directory: 1.3 to 1.7× faster than a stat and a private-size call per file (D2, D3). Caps stay ADR 0073's: 2,000,000 entries or 30 s, then `incomplete`.
3. **Walks follow writes, not snapshots.** A build changes files without a snapshot, never without a watch event (verified on macOS and Linux, ignored paths included). Any event marks a workspace dirty; a dirty workspace is walked once `[disk] measure_interval` (5 minutes) has passed since its last walk, one background walk at a time machine-wide, on its own thread at lowered priority. A walk clears the mark when it starts, so a write during it is not lost; a request's walk waits for one of the same workspace and walks again, at most two request walks at once. A held workspace (a live presence entry) is also walked every three intervals, because `[snapshot] exclude` paths and Watchman's subscriptions send no events. `GetWorkspace` walks on request and keeps the figure. Replaces ADR 0073's "after a snapshot settles, and every 15 minutes".
4. **Figures persist** in `workspace_disk`, with #274's eviction columns from the start, so a restart costs no walks before `ws list` has figures. `ListWorkspaces` reads the table; a workspace never walked reads zero, measured at its create.
5. **Free space reads `statvfs` first.** On macOS the volume's important-usage capacity (free plus purgeable, through CoreFoundation) takes 9 to 43 ms against `statvfs`'s microseconds; usable free space is the larger of the two (the important-usage figure was larger on the dev host, D1, smaller on one CI runner, and 0 for a process without a login session, which counts as unknown), so above the floor `statvfs` decides alone and the other is read only for a volume under twice its floor, and always by `GetDaemonStatus`. On Linux `f_bavail` is the figure: it excludes ext4's root reserve (D7).
6. **What is refused.** Below the floor on the workspace root's or the state directory's volume, `CreateWorkspace` (checked before anything is made) and `BuildStore` (after a sealed store's answer) fail with `DISK_SPACE_LOW`, kind `Conflict`. Moves and `reef rebase` are not refused, although they may materialize a base: they act on a workspace an agent is using, and rebasing is how a stale one comes back. A move logs a warning. `next` names the largest idle workspace's release (active, no live presence entry, no snapshot and no watch event for 30 minutes), else `reef ws list`, not ADR 0073's `reef ws gc`, which frees little.
7. **The level travels with the lists.** `DiskStatus` is on `GetDaemonStatusResponse.disk` and also on `ListWorkspacesResponse.disk_status` and `GetWorkspaceResponse.disk_status`, so `reef here` and `ws list` need no second call. `ws list`'s JSON stays an array: the level goes to one stderr line, and each brief element gains `disk` only while the level is not `ok`.
8. **`evictable_bytes` is #274's.** Nothing declares a build directory before #273 and #274; a field that is always 0 would mislead. #274 adds it with its declarations.
9. **Time Machine excludes Reef's caches, once.** The sticky exclusion attribute (what `tmutil addexclusion` writes, no privilege, D5) goes on `~/.reef`, the base cache and the stores, each recorded in the database so a user's `tmutil removeexclusion` is respected; never on a configured root; `[disk] exclude_from_backups = false` turns it off.
10. **Spotlight: nothing under `~/.reef`.** A hidden directory is not indexed, and `.metadata_never_index`, which ADR 0073 planned, no longer works outside a volume's root on macOS 27 (D6). The base cache in the state directory is indexed; a `.noindex` suffix would stop it but changes the state directory's layout, so it is [#291](https://github.com/reef-vcs/reef/issues/291), with #272's `mds` measurement as its baseline.
11. **The fleet gate reads the daemon.** G1 counts creates refused with `DISK_SPACE_LOW` and requires the daemon's level never `low`, from `reef daemon status` every 10 s; G4 counts what Reef holds and reports shared tool caches' growth (`GOCACHE`) apart, since Reef cannot change what Go keys its cache by (the user's decision, 2026-11-13).

## Alternatives

- **Matching `.gitignore` in the walk**: exact between snapshots; slower, and a second ignore matcher that can disagree with jj.
- **`walkdir` plus a private-size call per file everywhere**: one code path; up to 2.7× slower on APFS.
- **Walks after snapshots and on a timer** (ADR 0073): misses build-only growth between snapshots, and walks idle workspaces.
- **Figures in memory only**: no migration, but every restart walks every workspace before `ws list` has a figure, and #274 would need its own table.
- **The purgeable figure on every check**: a CoreFoundation call per create for a number that only matters near the floor.
- **Refusing moves and rebases too**: protects against a base materialization near the floor, but stops agents mid-ticket.
- **`ws list` as an object with a `disk` key**: breaks every parser of the array, including the fleet harness and the onboarding judge.
- **`.metadata_never_index`** for Spotlight: no longer honoured outside a volume root.

## Consequences

- #146 closes: the web UI's Disk column reads the daemon's figure and walks nothing; "Measure again" asks the daemon to walk one workspace.
- An agent on a full machine learns it from `reef here` or a refused create, with the release that frees the most in `next`; nothing it has written is ever refused.
- `ReclaimRecord.freed_bytes` uses the kept figure (unshared bytes where known) when it is current.
- `Backend::disk_usage` stays for the backend conformance suite; the daemon's answers come from the walk.
- #274 evicts by `largest`, the budget and the level, and adds `evictable_bytes`; #273's clone stores count as build output.
