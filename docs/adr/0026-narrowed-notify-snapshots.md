# 0026. Narrowed notify snapshots

- Status: accepted
- Date: 2026-10-23
- Ticket: R1.30
- Amended by: [0048](0048-status-from-the-watchers-journal.md)

## Context

R1.23 ran 200 `large` workspaces (100,014 files each) with notify watchers on one M2 Max. The steady-state write-to-snapshot p95 was 174 s against a 5 s target, with 831 of 1,000 writes not snapshotted 30 s after they were made. Every notify snapshot scanned every tracked file, whatever path its events named. Watchman workspaces already scanned only the reported paths, through jj's own integration ([0023](0023-watchman-as-the-large-repository-watcher.md)), but nobody had measured them at that scale. R1.30 measured before building anything: was the time in the walk, which narrowing removes, or in the tree-state reload that jj performs on every lock, which it does not?

The scan was split into `load_ms` (`start_working_copy_mutation`: the lock and jj's re-read of the whole tree state) and `walk_ms` (`snapshot`: walking and hashing). Three runs on the M2 Max, release builds, nothing else running (`docs/load-tests.md`):

| 200 × `large` unless stated | steady p50 / p95 | unsnapshotted after 30 s | watcher scan p50 / p95 | load p50 / p95 | walk p50 / p95 |
| --- | ---: | ---: | ---: | ---: | ---: |
| notify | 78.4 / 174.7 s | 828 of 1,000 | 11,393 / 12,093 ms | 47 / 152 ms | 11,283 / 11,890 ms |
| Watchman | 723 / 809 ms | 0 | 50 / 93 ms | 15 / 23 ms | 25 / 50 ms |
| notify, 50 workspaces | 1,281 / 1,403 ms | 0 of 250 | 639 / 9,555 ms | 13 / 23 ms | 619 / 9,476 ms |

The walk is 99% of a loaded notify scan, and the reload is 0.4%. Watchman, whose scans are narrowed, passes the gate. Both of the ticket's readings for "narrowing is the fix" hold. The 50-workspace run shows the per-scan cost growing with load as well: a full scan that takes 167 ms idle takes 639 ms with 50 workspaces and 11.4 s with 200. That is contention and kernel cache pressure on top of the walk itself, and narrowing removes the walk, so it removes both.

Two facts about jj-lib 0.46, the latest release (7 October), shape how the paths can reach the scan:

- A working copy fixes its `fsmonitor` settings when it is loaded, and the only changed-path input is `FsmonitorSettings::Test { changed_files }`. jj turns that into a `FilesMatcher` of exactly those files, unioned with a `PrefixMatcher` of the directory of each changed `.gitignore` (`make_fsmonitor_matcher`). `from_settings` refuses `test` from configuration.
- `TreeState::load`, `snapshot`, `save` and the fields of `TreeStateSettings` are public. `LockedLocalWorkingCopy::finish` saves its tree state only if its own snapshot dirtied it, and asserts that its tree is otherwise unchanged.

## Decision

We will narrow a notify workspace's watcher snapshots to the paths its events reported, through jj-lib's public `TreeState` API inside Reef's own lock (option 1 of the ticket), with no patched jj-lib.

1. **The scheduler keeps the paths.** Each scheduler entry keeps its workspace's canonical root and the set of distinct paths reported since its last scan began, relative to that root; a path outside the root is dropped. The set is taken once the snapshot holds its scan permit, just before the scan, and the workspace's dirty mark is cleared again then. Events that arrive while a snapshot waits for a permit join its set; events during the scan go to the next one.
2. **"Everything" is explicit.** `WatchEvent::everything` replaces the empty-path-list convention. notify sets it for every workspace when events were lost: inotify's queue overflow, or an FSEvents rescan. It also sets it on a workspace's next event after one of its events was dropped because the scheduler's queue was full: before narrowing, that next scan was full anyway. Watchman sets it on a fresh instance and on a subscription's first result. The set also becomes *everything* when it would grow past `watcher.narrowed_max_paths` (default 1,000; `0` turns narrowing off), when `.reefignore` changed (jj rescans under a changed `.gitignore` itself, not under Reef's own ignore file), and when the root itself was reported.
3. **Every reported path is a prefix.** A path covers itself and, if it is or was a directory, everything under it, on disk and in the recorded tree. For each path Reef gives jj's test monitor the path and `<path>/.gitignore`. The first matches the path as a file. The second makes `make_fsmonitor_matcher` add a prefix matcher on the path. jj's own walk then covers new directories (inotify names a new directory, not the files written into it before its watch exists), directories moved in or out, and both halves of a rename, and finds deletions under them. Reef does not expand prefixes itself, so there is no walk of its own and no bound on the files under a prefix. Ignore rules and symlinks are jj's as in any scan, so a store mount is never followed. A real changed `.gitignore` keeps jj's parent rescan, since it is one of the paths.
4. **The narrowed scan is a second load of the tree state.** `Repo::begin_snapshot` takes `Option<&[PathBuf]>`. It locks the working copy (jj's reload), checks freshness, and decides as before. If the working copy is fresh and the view has its working-copy commit, it loads the tree state again with the repository's own settings and `fsmonitor_settings = Test`, and snapshots that. `PendingSnapshot::finish` saves that tree state, then calls jj's `finish`, which saves only the operation, since the locked working copy was neither snapshotted nor dirtied. The order is jj's own: tree state, then operation. Nothing is saved before the operation commits, so a failed commit leaves the working copy as it was.
5. **The in-memory working copy is not trusted after a narrowed scan.** It keeps the tree it was locked with. The only reader of that tree, `Repo::working_copy_matches_view` (the GC's check), now reads the recorded tree from the state on disk, which is right after any snapshot. That costs one tree-state load per workspace per GC (about 18 MB read for `large`), where the in-memory tree was free; the GC runs rarely, and a flag set by a narrowed `finish` could skip the load for the rest if it ever matters.
6. **On macOS a path is resolved to its spelling on disk** (`fcntl(F_GETPATH)` on the longest existing part of it, the rest as given), because jj compares paths byte for byte and a case-insensitive volume reports the spelling a writer used. Both spellings are passed. A path whose existing part cannot be opened (a permission, a hard link to outside the root) scans everything.
7. **What stays a full scan:**
   - explicit snapshots (the `Snapshot` RPC, `Inspect` with `snapshot_first`, a release's final snapshot, a move) and shutdown snapshots;
   - fallback-timer snapshots;
   - a workspace whose watch is not working (`watched` false) or that is `behind` (its last automatic snapshot failed, or it was tracked again after a time unwatched);
   - a set that became *everything*, or a path jj cannot take (`..`, not UTF-8);
   - a working copy that is stale or has no working-copy commit in the view: the adopt, divergent and recover paths;
   - Watchman workspaces, which keep jj's own narrowing.

   The 10-minute watched fallback (R1.23) is the backstop for any change a narrowed scan missed.

## Consequences

- **The gate passes on `large`.** 200 × `large` with notify on the M2 Max: steady-state p95 794 ms (from 174.7 s), nothing unsnapshotted after 30 s, all 1,200 watcher scans narrowed at 40 ms p50 (23 ms of it the two tree-state loads, 8 ms walking), the scan queue never past one. 200 × `medium`: p95 729 ms (R1.23: 779 ms), and the burst drains in 7.4 s instead of 19.4 s. Creates and releases are unchanged: the releases' final snapshots stay full.
- **Each narrowed snapshot reloads the tree state twice**, once for jj's lock and once for the narrowed load. Under load a reload took 47 ms at p50 (152 ms at p95) against a full walk of 11.3 s. It is the cost that remains per snapshot, Watchman's included, and the ticket's option 2 (copying the tree state instead of reloading it, jj's TODO in `start_mutation`) would remove one or both. That is an upstream change ([0008](0008-jj-lib-embedding-findings.md), contribution 2), not needed for the target.
- **A narrowed scan can miss a change, and nothing says so.** A full scan cannot. The known cases are tested (`reef-daemon/tests/narrowed.rs`): directories moved in and out, files written into a new directory at once, `.reefignore` and `.gitignore` edits, overflows, too many paths, case-folded names, unwatched workspaces, and events while a snapshot waits or scans. A change that no event names waits for the watched fallback (10 minutes) or lands in the release's final snapshot: a tool writing through a path the watcher does not see, or on APFS a deletion through another spelling, which has nothing left on disk to resolve. `watcher.narrowed_max_paths = 0` turns the feature off without a rebuild.
- **Every FSEvents rescan is *everything* for every notify workspace.** The ticket asked that a rescan's directory narrow it to the one workspace it names. notify-debouncer-full keeps only the latest rescan event of a debounce window and drops the earlier ones, which can name other workspaces, so a narrowed rescan would trust a path that is not the whole story. None of R1.23's or R1.30's kept daemon logs has a rescan, so this costs nothing measured. Narrowing rescans would need the events before the debouncer drops them: a wrapper around notify's watcher.
- **Reef depends on three jj-lib behaviours**, any of which a jj-lib bump could change: the `.gitignore` parent rescan in `make_fsmonitor_matcher` (decision 3), `finish` not saving a tree state it did not dirty (decision 4), and `start_mutation` reloading the tree state rather than sharing it (decision 5). reef-lib's `jj_narrowed` tests pin all three: a reported directory covers its subtree, a full scan after a narrowed one finds nothing to record, and `working_copy_matches_view` holds after a narrowed snapshot.
- In debug builds jj checks every scan's whole tree against its file states, which no narrowing removes, so a debug narrowed scan is no cheaper than a full one by more than the walk. Benchmarks and the load test run in release.

## Alternatives considered

- **Option 2: through jj-lib.** Pass changed prefixes to `snapshot` (a field on `SnapshotOptions`), and copy the tree state at lock time instead of re-reading it. It is the better long-term design: one load, no reliance on `Test` or on `finish`'s internals. It needs an upstream change. Carried as a `[patch.crates-io]` fork until a release had it, it would also block `cargo publish` of `reef-lib` and `reef-daemon`. The measured reload (0.4% of a loaded scan) does not justify that now. It stays upstream contribution 2 in [0008](0008-jj-lib-embedding-findings.md).
- **Expanding prefixes in Reef**, as the ticket proposed: walk each reported directory, bounded by `narrowed_max_paths`, and add the tracked paths under it from `FileStates::prefixed` for deletions. jj's prefix matcher, reached through the `.gitignore` path, does the same with jj's own walk, ignore rules and symlink handling, at no extra cost and without a bound that turns a large new directory into a full scan.
- **Watchman for every workspace.** It passes the gate, but it is an external dependency Reef does not ship, its per-query cost (about 10 ms) dominates scans of small repositories, its crawl of 200 new workspaces doubled the creates' total (450 s against 228 s), and it still holds one inotify watch per directory on Linux ([0023](0023-watchman-as-the-large-repository-watcher.md)).
- **`force_tracking_matcher`.** It is the one matcher `SnapshotOptions` takes. But it overrides ignore rules, so narrowing through it would snapshot ignored files under every reported path.
