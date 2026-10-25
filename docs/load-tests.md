# Load and crash tests

Phase 1's exit criteria are about behavior under failure and load, not about code: an agent killed mid-edit loses nothing and leaves no directory behind; two workspaces on one change do not interfere; two hundred workspaces with active watchers get a write into the store within five seconds; and the daemon survives being killed and the disk filling. R1.23 tests these end to end against the `reef-daemon` and `reef` binaries, on both platforms. The design's Scale section promised a load test per phase, with its targets "revised from the measurement"; this is Phase 1's.

There are three suites, all in `crates/reef-daemon/tests/`:

| Suite | What it asserts | Where it runs |
| --- | --- | --- |
| `integration.rs` | Thirteen invariants against a spawned daemon on the `small` preset: an agent SIGKILLed mid-edit (and inside the debounce window) leaves its edits in a final snapshot and no directory; leases expire, and a heartbeat inside the grace revives one; two workspaces on one change diverge without seeing each other's files; a daemon SIGKILLed mid-snapshot or mid-create recovers with no row lacking a directory and no directory lacking a row; mounts and watchers return after a restart, and a write made while the daemon was down is snapshotted within `watcher.fallback_interval`; one inotify instance (Linux); a store shared by twenty; a move keeps its lease and directory; the CLI's JSON has the same keys on both platforms. | `ci.yml`'s `check` job on both runners; the restart and store tests also as root in the `overlayfs` job |
| `disk_full.rs` | `ENOSPC` on the state directory's filesystem (a 256 MB sparse APFS image on macOS, a 256 MB tmpfs on Linux): the daemon stays up, snapshots fail and are logged at `error`, nothing is reclaimed without its final snapshot, the database passes `PRAGMA integrity_check`, and the edit made while full is snapshotted once space is freed. | `check` on macOS; as root in the `overlayfs` job on Linux (elsewhere it prints `skipped:`) |
| `load.rs` | The load profile below, gated on the steady-state write-to-snapshot p95. | `load.yml`, and by hand |

Every test drives the real binaries: the kill tests need a pid to kill. `tests/common/spawned.rs` has `SpawnedDaemon` (start the binary on a temporary state directory with a `config.toml` the test writes, SIGKILL it, start it again on the same state) and `reef_bin()`, which finds the `reef` binary beside `reef-daemon` or builds it, since `cargo nextest run -p reef-daemon` does not.

## Running the load test

```sh
REEF_LOAD=1 cargo nextest run --release -p reef-daemon --test load --run-ignored all --no-capture
```

It is `#[ignore]`d and does nothing without `REEF_LOAD=1`. Run it in release, the build agents get: a debug daemon's scans and creates are several times slower, which measures the build rather than the design. Knobs, all environment variables:

| Variable | Default | |
| --- | --- | --- |
| `REEF_LOAD_PRESET` | `medium` | the preset of the N workspaces: `medium` (10,014 files) or `large` (100,014) |
| `REEF_LOAD_WORKSPACES` | `200` | N |
| `REEF_LOAD_STEADY_SECS` | `300` | the steady state's length |
| `REEF_LOAD_WRITE_PERIOD_SECS` | `60` | how often each workspace writes in the steady state |
| `REEF_LOAD_WATCHER` | `notify` | the watcher backend: `notify` or `watchman` (R1.30); each workspace's `watcher` field in the report says which one it got |
| `REEF_LOAD_NARROWED_MAX_PATHS` | the daemon's (1,000) | `watcher.narrowed_max_paths`; `0` makes every notify snapshot a full scan, as before R1.30 |

The `medium` run takes about 17 minutes on an M2 Max (six of them releasing the 200 workspaces; see the findings) and 7 in a Linux VM on the same machine, and needs about 4 GB free; `large` with 200 workspaces needs about 9 GB, takes about an hour on the M2 Max, and on Linux needs more than 8 GB of memory (the test checks the disk before it starts and stops with the figure). On Linux, run it as root so the daemon picks kernel overlayfs (`sudo -E env "PATH=$PATH" cargo nextest run ...`): unprivileged without `fuse-overlayfs` it falls back to plain copies, and twenty copies of `large` fill a CI runner's disk. Two hundred `large` workspaces watch 1.67 million directories, above Linux's default `fs.inotify.max_user_watches` (at most 1,048,576), so raise it first: `sudo sysctl -w fs.inotify.max_user_watches=4194304`.

`.github/workflows/load.yml` runs it on `ubuntu-24.04` (as root) and `macos-15`, and fails on either: on Sundays at 05:23 UTC with `medium`, by hand with either preset (`gh workflow run load.yml -f preset=large`), and on a pull request that changes `load.rs` or the workflow. Each job uploads `target/load/` as `load-<os>-<preset>-<sha>-<run id>`, kept 90 days.

## The profile

One daemon with the default configuration except `watcher.backend = "notify"` (`watchman` with `REEF_LOAD_WATCHER`), two registered synthetic repositories (`large`, and the preset), and these phases:

1. **Generate and register** both repositories, timed.
2. **Twenty on `large`**, the creation criterion's repository. One create builds the base and is released; then twenty, one at a time, each timed by the client, with the free space before and after. On the copy-on-write tiers (APFS, kernel overlayfs) each must be under one second; on others the report names the backend and nothing is asserted. The test waits until none of their watches is still attaching (below), then releases all twenty.
3. **N on the preset**, eight creates in flight at once, after one that builds the base: each create's time, the wall time for all N, and the free space before and after. Then the test waits until none of their watches is still attaching.
4. **Steady state**, which starts once every watch is up. Each workspace writes `write-<seq>.txt`, containing the time of the write, once per period, at an offset of `i × period / N`, so the daemon sees an even 3.3 writes per second for 200 workspaces. One client heartbeats every workspace every 20 seconds.
5. **Burst.** All N write once within one second.
6. **Limits and teardown.** The limits below are read, every workspace is released (with its final snapshot), and no workspace directory may remain.

A sampler lists the preset's workspaces every 250 ms (one `List`, which carries what `Inspect(snapshot_first = false)` does for every workspace at once; `snapshot_first` would make the sampler the trigger). A write's latency is the `last_snapshot_at` of the first listing in which the workspace's commit differs from its commit at the write and `last_snapshot_at` is no earlier than the write; `last_snapshot_at` is when the snapshot's commit was recorded, so the 250 ms poll does not add to it. Writes change content, so jj's scan always sees them.

On Linux a created workspace's notify watch attaches in the background after the create returns, one workspace at a time ([ADR 0028](adr/0028-watch-attachment-off-the-create-path.md)), and is `attaching` meanwhile. The criterion is at steady state with active watchers, so the test polls `List` every 250 ms until no created workspace is `attaching`, and fails past `max(60 s, 2 s × count)`, naming those still attaching. It records how long that took as the attach drain. On macOS the watches attach inside the create, and the drain is 0.

**The gate**, which fails the test: the steady state's p95 is at most 5 s, and no write is still unsnapshotted, or was snapshotted, more than 30 s after it was made. The burst is recorded, not gated: it measures scan throughput over `watcher.max_concurrent_scans` cores rather than the design's target. A run on a CoW tier also fails if a `large` create takes a second or more.

## Reading the report

`target/load/<os>-<preset>-<UTC time>.json` holds the machine (`os`, `arch`, `cpus`, `model`, `memory_bytes`, `kernel`, and whether it ran in CI), the parameters, the daemon's `config.toml` as written, and per phase:

- `large_creates` and `creates`: `backend`, `base_build_ms`, every create's `ms`, the summary figures, and `free_bytes_per_workspace`.
- `steady` and `burst`: `writes`, `p50_ms`, `p95_ms`, `max_ms`, `unsnapshotted`, and `samples`, one `[workspace, seq, write_ms, snapshot_ms]` per write (`snapshot_ms` null if none came).
- `heartbeat_rpcs` and `list_rpcs` (the sampler's calls).
- `limits`: `max_concurrent_scans`, `peak_threads` and `peak_rss_bytes` of the daemon (sampled every 2 s from `/proc/<pid>/status` on Linux and `ps` on macOS) and, on Linux, `inotify_max_user_watches`, `inotify_max_user_instances`, and the daemon's own `inotify_instances` and `inotify_watches` (from `/proc/<pid>/fd` and `fdinfo`).
- `watcher_summaries`: the daemon's per-minute `watcher summary: scans=N queued_max=M wait_max_ms=K` lines, parsed, with `queued_max` and `wait_max_ms` over the run.
- `creates.steps`: the p50 and p95 of each step the daemon timed (R1.29's `CreateTimings`) over the N creates; `creates.watchers`: how many workspaces got each watcher.
- `large_creates.attach` and `creates.attach` (R1.32): `drain_ms`, from the last create's return to no workspace `attaching`; `count`, the daemon's `workspace watch attached` lines for those workspaces; and the p50 and max of their `queued_ms` (from the create queueing the attach to its start) and `attach_ms` (the attach itself). On macOS the sections are present, with zeros.
- `watcher_scans`: from the steady state on, the daemon's `watcher snapshot taken` lines: their `count`, how many were `narrowed`, and the p50, p95, and max of `scan_ms`, its parts `load_ms` (locking the working copy, which reloads jj's tree state, and for a narrowed scan loading it again) and `walk_ms` (walking and hashing), and `paths_count` (R1.30).
- `teardown` (the total and each release's p50, p95, and max), `directories_left`, and `pass` (`gate`, `creates_under_1s`).

The daemon's log is copied beside the report, into `<report>.daemon-log/`.

The test also prints the table below's column for the run.

## Baseline

`medium`, 200 workspaces, 300 s steady state, release build, 2026-10-23. The macOS column is an Apple M2 Max (12 cores, 32 GB, macOS 27.0.1, APFS) with nothing else running; the Linux column is Docker Desktop's VM on the same machine (arm64, 12 vCPUs, 7.7 GB, kernel 6.10.14-linuxkit), as root in a `--privileged` `ubuntu:24.04` container with the state directory on an ext4 volume, so kernel overlayfs. A VM on a laptop is not a Linux server. The runner columns are the first `load.yml` run, [39942894285](https://github.com/reef-vcs/reef/actions/runs/39942894285) on R1.23's pull request: `ubuntu-24.04` was a 4-vCPU AMD EPYC 7763 with 16 GB (x86_64, kernel 6.17, as root), `macos-15` a 3-vCPU M1 virtual machine with 7 GB (macOS 15, APFS).

| Measurement | M2 Max, macOS | Linux VM, same machine | `ubuntu-24.04` | `macos-15` |
| --- | ---: | ---: | ---: | ---: |
| backend used | apfs | overlayfs (kernel) | overlayfs (kernel) | apfs |
| 20 × `large` create, max / mean | 1,312 / 982 ms | 508 / 417 ms | 1,148 / 1,097 ms | 5,703 / 3,549 ms |
| 20 × `large` free-space delta per workspace | 32.1 MB | 6.0 MB | 6.0 MB | 34.2 MB |
| 200 × `medium` create, total (p95 each, 8 at once) | 41.7 s (5,336 ms) | 11.5 s (648 ms) | 27.8 s (1,283 ms) | 49.4 s (2,692 ms) |
| steady-state write-to-snapshot p50 / p95 / max | 760 / **779** / 1,143 ms | 647 / **726** / 1,004 ms | 665 / **671** / 1,064 ms | 30.2 / **148.9** / 150.7 s (505 of 1,000 unsnapshotted after 30 s) |
| burst p50 / p95 / max | 14.2 / 19.4 / 19.7 s | 1.1 / 2.0 / 2.4 s | 2.9 / 5.0 / 5.3 s | 14.7 / 28.8 / 30.1 s |
| scans queued max, wait max | 1, 0 ms | 1, 0 ms | 3, 843 ms | 197, 165,089 ms |
| inotify watches used / limit | n/a | 200,000 / 1,048,576 (1 instance) | 200,000 / 655,360 (1 instance) | n/a |
| daemon peak threads, RSS | 250, 554 MB | 223, 1,209 MB | 39, 997 MB | 49, 540 MB |
| 200 releases, total | 458 s | 7.3 s | 118 s | 202 s |

The M2 Max, the Linux VM, and the `ubuntu-24.04` runner pass the gate with room: the steady state's p95 is under 0.8 s against 5 s, over 1,000 writes each, none unsnapshotted. Almost all of it is `watcher.debounce` (500 ms) and one scan of 10,014 files. The `macos-15` runner fails it: with three scan permits, scans queued up to 197 deep, the same saturation as `large` on the M2 Max (below). The columns are from before R1.23's fallback change (below); after it, the M2 Max's steady state was the same (775 / **801** / 1,412 ms) and its 200 releases took 344 s.

### `large`

Before R1.30, `large` (100,014 files) failed the gate on both, by a wide margin; R1.30's runs follow. The first macOS column is 200 workspaces on the same M2 Max before R1.23's fallback change (below), the second after it. The Linux column is 50 in the same VM, before the change: with 200, and with 100, the kernel's OOM killer stopped the daemon while the workspaces were being created.

| Measurement | M2 Max, 200, before | M2 Max, 200, after | Linux VM, 50, before |
| --- | ---: | ---: | ---: |
| whole run | 4 h 9 min | 62 min | |
| backend used | apfs | apfs | overlayfs (kernel) |
| 20 × `large` create, max / mean | 1,995 / 1,652 ms | 1,218 / 988 ms | 519 / 411 ms |
| N × `large` create, total (p50 / p95 each, 8 at once) | 1,277.5 s (15.2 / 210.0 s) | 223.3 s (7.7 / 9.6 s) | 27.2 s (4.3 / 5.3 s) |
| slowest create step, p50 / p95 | materialize 12.8 / 202.2 s | materialize 4.3 / 7.6 s | watch 4.2 / 5.3 s |
| steady-state write-to-snapshot p50 / p95 / max | 87.4 / **194.1** / 214.5 s | 77.7 / **173.7** / 190.1 s | 11.3 / **23.8** / 39.3 s |
| writes unsnapshotted after 30 s | 840 of 1,000 | 831 of 1,000 | 4 of 250 |
| burst p50 / p95 / max | 13.3 / 25.3 / 25.3 s (170 of 200 unsnapshotted) | 13.8 / 28.8 / 29.0 s (166 of 200 unsnapshotted) | 14.8 / 24.9 / 25.3 s |
| scans queued max, wait max | 188, 202.8 s | 188, 181.0 s | 38, 21.2 s |
| scan queue while creating, max | 188 | 1 | |
| inotify watches used / limit | n/a | n/a | 415,150 / 4,194,304 (1 instance) |
| daemon peak threads, RSS | 338, 3,576 MB | 200, 2,681 MB | 181, 3,659 MB |
| N releases, total (p50 / p95 each) | 12,940 s (547 / 770 s) | 2,802 s (77.8 / 313.3 s) | 145 s (10.1 / 100.5 s) |

Before the change, the scan pool could not keep up with the `watcher.fallback_interval` snapshots. Every idle workspace was snapshotted once a minute, and each snapshot is a full scan of 100,000 files. On macOS the pool completed 50 to 60 scans a minute against the 200 the fallback asked for, so the queue grew to 188 (nearly every workspace) and a write waited behind it for minutes. In the Linux VM a scan through overlayfs took about 4.5 s, and 50 workspaces were already past the 12 permits. Creates and releases waited behind the same queue: the macOS creates' time was almost all in materializing (the clone, contending with scans on the same volume), and the Linux creates' in the watcher's first scan.

R1.23 changed that: a workspace whose watch is working is now snapshotted on the timer every `watcher.watched_fallback_interval` (10 minutes), and only workspaces without one, whose last automatic snapshot failed, or that have had none since a daemon restart, an aborted release, or a move (writes no watcher saw), every `watcher.fallback_interval`. A watch that loses part of its tree (inotify's limit hit on a new directory) counts as not working. The scan queue stayed empty through the creates (before, it reached 188 during them), the creates and releases no longer wait behind it, and the whole run took a quarter of the time. The steady state did not improve, because its scans are the writes' own. A notify snapshot scans every tracked file whatever path the event named (Watchman's are narrowed to the reported paths; notify's are not yet): a scan of 100,000 files took 11.4 s at the median and 14.2 s at p95 with the pool busy, so 12 permits complete about 63 a minute against the 200 writes a minute the profile makes. Passing notify's paths to the scan, as Watchman's are, is [R1.30](dev-tickets/phase-1/R1.30-incremental-notify-snapshots.md), which measures first whether that is where the time goes.

### R1.30: where a loaded scan's time goes, and narrowed scans

R1.30 split each scan into `load_ms` (locking the working copy, which makes jj re-read its whole tree state) and `walk_ms` (walking and hashing), and ran `large` four ways on the same M2 Max, release builds, nothing else running. The first three are the measurement, all with full notify scans or Watchman: 200 workspaces with notify, 200 with Watchman (`REEF_LOAD_WATCHER=watchman`), and 50 with notify (`REEF_LOAD_NARROWED_MAX_PATHS=0`, which is the code before R1.30 as far as scans go). The fourth is the same profile with notify snapshots narrowed to their reported paths ([ADR 0026](adr/0026-narrowed-notify-snapshots.md)).

| Measurement | notify, 200, full scans | Watchman, 200 | notify, 50, full scans | notify, 200, narrowed |
| --- | ---: | ---: | ---: | ---: |
| whole run | 61 min | 43 min | 19 min | 54 min |
| 20 × `large` create, max / mean | 1,239 / 1,006 ms | 12,832 / 1,790 ms | 1,210 / 991 ms | 1,274 / 1,041 ms |
| N × `large` create, total (p95 each, 8 at once) | 227.5 s (8,274 ms) | 450.3 s (17,690 ms) | 75.7 s (34,178 ms) | 226.0 s (8,243 ms) |
| steady-state write-to-snapshot p50 / p95 / max | 78.4 / **174.7** / 190.0 s | 723 / **809** / 4,937 ms | 1,281 / **1,403** / 1,762 ms | 688 / **794** / 867 ms |
| writes unsnapshotted after 30 s | 828 of 1,000 | 0 of 1,000 | 0 of 250 | 0 of 1,000 |
| burst p50 / p95 / max | 15.4 / 27.0 / 28.4 s | 4.4 / 7.7 / 8.0 s | 11.0 / 21.0 / 21.2 s | 4.4 / 7.5 / 8.0 s |
| watcher scans from the steady state on (narrowed by Reef) | 434 (0) | 1,200 (jj narrows them) | 275 (0) | 1,200 (1,200) |
| watcher scan p50 / p95 | 11,393 / 12,093 ms | 50 / 93 ms | 639 / 9,555 ms | 40 / 87 ms |
| of which load p50 / p95 | 47 / 152 ms | 15 / 23 ms | 13 / 23 ms | 23 / 30 ms |
| of which walk p50 / p95 | 11,283 / 11,890 ms | 25 / 50 ms | 619 / 9,476 ms | 8 / 40 ms |
| scans queued max, wait max | 188, 182.4 s | 6, 654 ms | 38, 19.2 s | 1, 0 ms |
| daemon peak threads, RSS | 206, 2,730 MB | 313, 3,394 MB | 195, 1,036 MB | 312, 3,688 MB |
| N releases, total (p50 / p95 each) | 2,759 s (78.2 / 346.3 s) | 1,512 s (62.7 / 77.5 s) | 393 s (76.5 / 82.2 s) | 2,377 s (77.4 / 348.0 s) |

- **The walk is the time.** A loaded full scan was 99% walk (11.3 s of 11.4 s); the tree-state reload that jj does on every lock was 47 ms. Watchman, whose scans are narrowed by jj, passed the gate. Both of the ticket's readings for "narrowing is the fix" held.
- **A full scan's cost grows with the load, not only with the tree.** The same 100,000-file scan takes 167 ms idle (R1.14), 639 ms at the median with 50 workspaces, and 11.4 s with 200: four times the workspaces made each scan 18 times slower. That is twelve scans contending for the disk and the kernel's metadata caches (200 `large` trees are 20 million files). A narrowed scan touches a few directories, so it removes both.
- **Narrowed, 200 `large` workspaces pass the gate** with a p95 of 794 ms, against 174.7 s. Every steady-state scan was narrowed, with one reported path each; the queue never held more than one. A narrowed scan's time is now mostly the two tree-state loads (jj's lock and the narrowed load), 23 ms at the median, against 8 ms of walking.
- **Watchman passes too, at twice the create time.** Its crawl of each new workspace ran while the next ones were cloned (R1.29), so the 200 creates took 450 s against 226 s, and one of the twenty timed creates took 12.8 s.
- **Memory.** Peak resident size with narrowed scans was 3.7 GB, against 2.7 GB with full scans in this ticket and 2.7 and 3.6 GB in R1.23's two runs. A narrowed scan holds a second copy of its workspace's tree state (about 18 MB for `large`) while it runs, about 0.2 GB at 12 scans at once; the rest of the difference is within the variation between runs. The per-workspace finding below stands.
- **Releases are unchanged** (2,377 s against 2,759 s): their time is the final snapshots, which stay full scans, and deleting 100,000-file trees, R1.23's teardown finding.

The macOS runs' twenty `large` creates before the change were also slower than in the `medium` run (1,652 ms mean against 982), with only the warm workspace before them. The volume was 97% full at the time; whether that is why was not established. After the change, with more room on the volume, they averaged 988 ms.

## Findings

- **The phase target holds with a wide margin on `medium`** on both platforms on a 12-core machine and on the 4-vCPU Linux runner, at 3.3 writes per second across 200 workspaces. With R1.30's narrowed scans the M2 Max's p95 is 729 ms (779 ms before), and its burst drains in 7.4 s (19.4 s before). It failed on the 3-vCPU `macos-15` runner until R1.30, since a full scan of 10,000 files on APFS took 0.8 s at the median and 3.1 s at p95 there (against 32 and 111 ms on `ubuntu-24.04`), so its 3 permits completed about 60 scans a minute: fewer than the 200 writes a minute the profile makes. With narrowed scans (11 ms at the median there) its p95 is 854 ms (from 148.9 s), and `ubuntu-24.04`'s is 644 ms, in R1.30's pull-request run; `load.yml` gates on both again.
- **It did not hold on `large` until R1.30, because every notify snapshot was a full scan.** Before R1.23's change, a full scan of every idle workspace every 60 s cost N × files a minute: 2 million file visits for 200 `medium` workspaces, which a 12-core pool absorbs and the `macos-15` runner's 3 permits do not, and 20 million for 200 `large`, which it does not. The change (a watched workspace's timer snapshot every 10 minutes) removed that load, and the creates and releases recovered, but the steady state's own writes still each scan 100,000 files (above). R1.30 narrowed a notify watcher snapshot to the paths its events named, as Watchman's already were, and 200 `large` workspaces now pass with a p95 of 794 ms (above).
- **The creation criterion is at the edge on APFS, as R1.29 said.** Through the daemon with 200 other workspaces' watchers running, twenty `large` creates averaged 982 ms (the first four 1.0 to 1.3 s, the rest 0.87 to 0.95 s) in one run and 974 ms in another; R1.29's bench measured 962 ms with nothing else running. Nine tenths of it is the APFS clone. Kernel overlayfs passes at 417 ms. The load test reports this and does not fail on it: the criterion is R1.12's bench's, and the runners are slower than the machine it was measured on.
- **A burst on macOS takes 14 to 19 s to drain; on Linux 2 s.** No scan waited, so the time is in committing: 200 snapshot transactions, serialized on the repository thread (R1.03), at 70 to 95 ms each on APFS against about 10 ms on ext4. Why a commit costs that much more on macOS (jj's file writes and syncs on APFS are the suspects) is not established here. Within the design's 5 s only for writes that do not all land in the same second; recorded, not gated.
- **Tearing down 200 `medium` workspaces on macOS takes 6 to 8 minutes; on Linux 7 seconds.** A release alone takes 0.8 s, but with 200 workspaces alive the median release took 15.7 s (8 in flight). A `sample` of a scratch daemon releasing them (installed release binaries, the same 200 workspaces) put almost all CPU in jj's working-copy scans (`fstatat` under `FileSnapshotter::visit_directory`) and in deleting released trees (`unlinkat` under `fs_util::remove_all`). Before R1.23's fallback change, its `watcher summary` lines showed 100 to 130 scans a minute, queued up to 153 deep and waiting up to 51 s for a permit: the 60 s fallback snapshot of every idle workspace running into the releases. After it, the queue stayed at one through the teardown, about 30 scans a minute (the releases' own final snapshots), and the teardown took 344 s instead of 458 (median release 13.9 s). The rest is the final snapshots and the deletions of 10,000-file cloned trees on the same APFS volume; on Linux a release removes an overlay's small upper directory instead. Deleting released trees at low priority, or in one background pass, is the follow-up.
- **Memory grows with the tree, per workspace.** Peak resident size was 554 MB with 200 `medium` workspaces on macOS and 1.2 GB on Linux. With `large`, it was 3.5 GB for 200 workspaces on macOS (about 18 MB each) and 3.6 GB for 50 on Linux (about 73 MB each; why Linux needs four times as much is not established). One hundred or 200 `large` workspaces did not fit the 7.7 GB Linux VM: the kernel's OOM killer stopped the daemon partway through creating them, at 3.6 to 3.7 GB resident, beside the kernel memory of more than a million inotify watches. Each loaded working copy keeps its `tree_state` (one entry per tracked file) in memory. A machine meant to hold hundreds of 100,000-file workspaces needs that much memory, or the daemon needs to unload idle working copies (a Phase 2 question for the Scale section).
- **Limits.** One inotify instance per daemon, as R1.13 designed; `medium` used 1,000 watches per workspace, so the default 1,048,576 holds 1,000 `medium` workspaces or 125 `large` ones. The scan pool defaults to one permit per CPU (12 here).
