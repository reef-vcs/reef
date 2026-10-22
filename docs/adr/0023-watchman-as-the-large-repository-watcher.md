# 0023. Watchman as the large-repository watcher

- Status: accepted
- Date: 2026-10-17
- Ticket: R1.14

## Context

R1.13's notify watcher only triggers snapshots. Every snapshot still scans every tracked file, because jj-lib has no public way to hand its working copy a set of changed paths except through Watchman ([0008](0008-jj-lib-embedding-findings.md), "Not exposed"). Two costs grow with repository size:

- **Scans are O(tracked files).** On the `large` preset (100,000 files) one scan is 146 ms in R1.13's benchmark, per snapshot, per workspace.
- **inotify is per directory.** On Linux, notify adds one watch per directory: 8,336 for `large`, so 200 workspaces need 1.7 million watches, against a typical `max_user_watches` of 65,536 to 655,360.

jj already integrates Watchman. With `fsmonitor.backend = "watchman"`, `LocalWorkingCopy::snapshot` asks Watchman which paths changed since a clock persisted in the working copy's `tree_state`, and scans only those (plus the parents of changed `.gitignore` files). A failed query is logged and falls back to a full scan. The settings are read into each working copy when it is loaded.

## Decision

We will use Watchman for large repositories, through jj-lib's own integration, with Reef supplying the trigger and the selection.

1. **jj-lib's `watchman` feature, nothing else.** `reef-lib` and `reef-daemon` have a `watchman` cargo feature that turns on `jj-lib/watchman`; the daemon (and `reef-vcs`) has it on by default. The trigger uses the `watchman_client` crate jj-lib already depends on, re-exported from `reef_lib::jj`, so there is one client crate at one version.
2. **The daemon is the trigger.** jj's own trigger (`fsmonitor.watchman.register-snapshot-trigger`, which runs `jj util snapshot`) is always off. `WatchmanWatcher` holds one subscription per workspace on one shared connection. Each batch becomes a `WatchEvent`, debounced by `watcher.debounce` on a tokio timer. The `[snapshot] exclude` directories and store mounts are left out by the subscription's expression; no `.watchmanconfig` is written. When a subscription is cancelled or its connection is lost, the workspace is marked `watch-failed` and lives on the fallback timer, and the task subscribes again after 1 s, 2 s, 4 s, and so on up to 30 s. Every subscription's first result asks for one snapshot at once, since nobody saw what changed before the subscription was up.
3. **Narrowing is jj's.** Reef passes no paths to the scan. The trigger and the narrowing are independent: a lost trigger costs latency until the fallback timer, never correctness.
4. **Selection is per repository; the result is recorded per workspace.** `watcher.backend = "auto"` (the default) picks Watchman when `watchman` is on `PATH` and the repository has at least `watcher.watchman_threshold_files` (50,000) tracked files; below that, and for `"notify"`, notify. `"watchman"` requires the binary: creating a workspace without it fails with `UNAVAILABLE` naming `watchman`. The count is the number of entries in the `default` workspace's working-copy tree, read from the store at registration (or at the first start after this ticket) and stored in `repositories.tracked_files`. A workspace's watcher is chosen at create and stored in `workspaces.watcher`, because jj fixes the `fsmonitor` settings when it loads the working copy; the daemon keeps one jj settings variant per watcher and loads each workspace with its own. Changing `watcher.backend` affects new workspaces only.
5. **`WATCHMAN_SOCK` at start.** jj connects to Watchman again for every snapshot, and without `WATCHMAN_SOCK` each connection first runs `watchman get-sockname` to find the socket. That costs about 135 ms of CLI start-up per snapshot on an M-series Mac, more than a full scan of `medium`. So when `watcher.backend` is not `notify` and `watchman` is on `PATH`, `reef-daemon` runs `get-sockname` once, before it starts any thread, and exports the answer. The daemon's own (re)connections still run discovery, with `WATCHMAN_SOCK` removed, so a Watchman server that stopped is started again.

## Consequences

- Scans after a change are narrowed. Measured in release on an M-series Mac, the median of 9 explicit snapshots after one changed file: `large` 167 ms with notify and 31 ms with Watchman (5.4×); `medium` 19 ms and 13 ms. Watchman's cost per query (connect, `watch-project`, removing jj's trigger, the query and its sync cookie) is about 10 ms, so it pays off only well above `medium`; the 50,000-file threshold stands.
- Watchman is a second, external, optional dependency. Reef does not ship or install it. With `"auto"` and `watchman` installed, the daemon starts the Watchman server at its own start (that is what `get-sockname` does), even if no repository is large enough to use it.
- The first snapshot of each Watchman workspace after the Watchman server restarts (or drops the watch) is a full scan. Watchman answers a clock from another server instance with "fresh instance", and jj then scans everything. A daemon restart alone keeps narrowed scans: the clock is in the working copy's state and the server still holds the watch.
- Every subscription's first result, and every later fresh-instance result, triggers one snapshot. So a workspace is snapshotted once when the daemon starts, after create, and after Watchman comes back, which covers edits made before its subscription was up; jj's clock keeps those scans narrowed when it can.
- An edit to `.reefignore` (or to the excludes) that un-ignores files is not picked up by a narrowed scan until those files change or a full scan runs. jj rescans parents of changed `.gitignore` files only, and Reef's ignore sources are not `.gitignore` files. Reef's own excludes are fixed at create (R1.19), so this is limited to `.reefignore` edits inside a workspace.
- The fsmonitor setting is baked into each working copy. A workspace keeps its watcher for life, and the database records it so a restart loads it the same way. A build without the `watchman` feature loads a Watchman workspace with full scans and no trigger: it is marked `watch-failed` and lives on the fallback timer.
- Watchman removes the per-workspace inotify instance but not the watches: it still holds one inotify watch per directory per project root, so 200 workspaces of `large` need as many watches inside Watchman as notify needed. R1.23 records the numbers. The fix at that scale is one Watchman project root shared by all workspaces, which needs the `.watchmanconfig` this decision does not write.
- The tracked file count is taken once per repository, so a repository that grows past the threshold stays on notify under `"auto"` until it is counted again. Recounting at each daemon start is cheap and left to a follow-up.
- `WATCHMAN_SOCK` is read once. If the Watchman server came back on a different socket, jj's queries would fail and fall back to full scans until the daemon restarts. Watchman's socket path is fixed per user and state directory, so this is not expected.

## Alternatives considered

- **A notify-based narrowing through an upstream jj-lib change**, letting a caller pass changed paths to `snapshot`. It is the better long-term design, one watcher for both jobs, and is upstream contribution 2 in [0008](0008-jj-lib-embedding-findings.md). It is not available in jj-lib 0.46.
- **Caching the Watchman client in Reef** instead of exporting `WATCHMAN_SOCK`. jj builds its own connector in `Fsmonitor::init` and offers no way to pass a client or a socket, so this would mean forking that code. If the per-query cost matters in R1.23, the fix belongs upstream.
- **FSEvents on macOS only.** It avoids the per-directory cost on macOS but not on Linux, where the watch limit bites.
- **Polling.** It is O(files) per tick, which is the cost this decision exists to avoid.

## Amendment, 2026-10-22 (R1.29): Watchman's crawl competes with APFS clones

R1.12 found `create_api/large/apfs/20` at 1.93 s per workspace against the one-second criterion, and R1.29 measured why with per-step create timings. With Watchman, attaching costs nothing on the create path, but the new workspace's subscription makes Watchman crawl its 100,000 files, and the subscription's first result asks for a full jj scan (a fresh instance has no changed-file list). Both run while the next workspace is cloned: on an M2 Max the clone took 1,842 ms of a 1,904 ms create in a run of twenty, against 875 ms for one.

- **A just-created workspace's subscription waits for creates to go quiet.** The daemon records when the latest create started. A Watchman task for a workspace that `Create` just tracked (`TrackReason::Created`; not daemon start, an aborted release, or a rewatch) subscribes only once no create has started for `watcher.watchman_attach_quiet` (default 2 s), or `watcher.watchman_attach_defer_max` (default 10 s) after the watch, whichever is first. `0s` for either turns the wait off. "No create in flight" would not do: one client's creates run back to back, so the count drops to zero between each pair.
- **Nothing is lost.** The first result of the subscription snapshots whatever was written during the wait, and the fallback timer still runs; what waits is how fresh the workspace's snapshots are. `attention` stays clear, and `Inspect` reports `watch_state = "waiting"` (`reef ws inspect`: `watchman (waiting for creates to finish)`).
- **Measured effect** (M2 Max, `create_api/large/apfs/20`): p50 0.98 s per workspace, from 1.90 s. Criterion's mean is 1.23 s: a run of twenty creates lasts about 20 s, longer than the 10 s cap, so the first workspaces subscribe while the run is still going. Subscriptions also queue behind each other's crawls (ten came up at the same instant in a scratch run), and their first full scans then compete with the clone: in twenty creates through the CLI (35 s), the creates before the first cap took 1.0 s each, one create during ten concurrent first scans took 10.8 s, and every subscription and first scan was done 8 s after the last create. The bench releases each iteration's workspaces, which cancels waiting subscriptions, so its number mostly measures creates with no Watchman work at all.

The cap is the trade-off: raising it keeps long bursts clean but leaves each workspace's first snapshots later. R1.29 records the numbers and does not tune it; R1.23 measures the phase at steady state, after the creates.
