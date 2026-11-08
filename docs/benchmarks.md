# Benchmarks

The design's first goal is that operations cost O(what changed), never O(repository size). The benchmarks track how far jj-lib, which Reef builds on, is from that today. They are trend data, not a gate: nothing fails when a number gets worse. Regression thresholds come with RX.06.

## Running them

```sh
just bench-quick                       # small preset, short measurements: about a minute
just bench                             # medium preset, criterion's full run: about 5 minutes
REEF_BENCH_PRESET=large just bench     # large preset; building the fixture alone takes minutes
just bench -- modify_50                # only benchmarks whose id contains modify_50
```

`just bench` is `cargo bench --workspace --bench '*'`, and anything after `--` goes to criterion (`--help` lists its options). Use `--bench '*'` when calling cargo directly with criterion arguments; without it cargo also hands them to the lib and bin targets' libtest harnesses, which reject them.

`REEF_BENCH_PRESET` picks the synthetic repository: `small` (1,000 text files), `medium` (10,000, the default), or `large` (100,000). All three are seed 42 of the `reef_testing::synth` presets, plus 3 binaries of 8 MiB, 10 symlinks, and ignored `target/` and `node_modules/` directories.

The fixture lives in `target/bench-fixtures/<preset>-42/` and is built on first use: the tree is generated, a jj repository with an internal git store is created around it, and the whole tree is snapshotted once so later benchmarks measure incremental cost. That first snapshot's time is printed to stderr; it is the O(repository size) number. Later runs reuse the fixture, which drifts as the benchmarks mutate it but keeps its file count. `target/bench-fixtures/<preset>-42.ready` marks a fixture as complete; delete it, or run `cargo clean`, to rebuild from scratch. A reused fixture whose working copy cannot be snapshotted (for example, stale after a run was killed mid-snapshot) is rebuilt automatically.

`cargo bench` builds with the `bench` profile, which inherits `release` and so keeps line-table debug info (`debug = 1`) for `perf` and Instruments.

## What each benchmark measures

All seven are `snapshot/<preset>/<name>`, in `crates/reef-lib/benches/snapshot.rs`.

| Benchmark | Setup (not timed) | Timed |
| --- | --- | --- |
| `modify_1`, `modify_50`, `modify_500` | `synth::mutate` modifies N text files | `Repo::snapshot` |
| `add_50` | deletes 50 text files and snapshots that, then adds 50 | `Repo::snapshot` |
| `op_log_100` | snapshots until the log holds 100 operations | `Repo::op_log(100)` |
| `open` | none | `Repo::open`: loads the workspace, op heads, view, and index |
| `noop` | one snapshot so the tree is clean | `Repo::snapshot` on the unchanged tree |

`noop` is the scan cost jj pays to find that nothing changed, and the number Phase 1's watcher-driven snapshots must beat. It also reports throughput in tracked files per second (criterion prints `elem/s`). The `modify` benchmarks create one operation per iteration and `add_50` two, so a reused fixture's operation log and git store grow with every run. `open` is the number that moves most: loading the index reads every commit added so far, so locally it creeps up as the fixture ages. CI builds a fresh fixture on every run.

Each benchmark takes 10 samples. The measurement time is 10 s for `small`, 30 s for `medium`, and 60 s for `large`; criterion's `--measurement-time` and `--sample-size` override both. Setup runs before every iteration (`iter_batched` with `BatchSize::PerIteration`) and is excluded from the reported time, but it counts toward the measurement time, so a benchmark with expensive setup gets fewer timed iterations rather than a longer run.

## CI

`.github/workflows/bench.yml` runs the benchmarks on `ubuntu-24.04` and `macos-15`:

- on every push to `main`, with the `medium` preset;
- on Mondays at 06:17 UTC, with the `large` preset;
- by hand: `gh workflow run bench.yml -f preset=medium` (or `small`, `large`).

Pull requests do not run them, but `just build` compiles them (`--all-targets`), so they cannot rot.

Each job uploads an artifact named `bench-<os>-<preset>-<commit sha>-<run id>`, kept for 90 days, holding `bench-results.txt` and criterion's `target/criterion/` directory. The run id is there because one commit can be benchmarked more than once with the same preset, for example by the push to `main` and a manual run. Download the artifacts for a commit with:

```sh
gh run download --pattern "bench-ubuntu-24.04-medium-$(git rev-parse HEAD)-*"
```

Each matching artifact lands in its own directory. Pass a run id (`gh run download <run id> --pattern ...`) to take just one run.

Open `target/criterion/report/index.html` from the artifact for criterion's HTML reports, one per benchmark under `snapshot/<preset>/<name>/report/`.

GitHub-hosted runners are shared, noisy machines. Read the artifacts for trends across many runs, not for precise numbers or small differences between two runs.

### Reading `bench-results.txt`

It is criterion's `--output-format bencher` output, one line per benchmark:

```text
test snapshot/medium/noop ... bench:  11,203,512 ns/iter (+/- 140,961)
```

The number is criterion's estimate of the median time per iteration in nanoseconds, and the `+/-` figure its estimate of the standard deviation. Both differ from criterion's default output, which prints the slope estimate (these benchmarks use linear sampling) and its confidence interval. Throughput is not included; divide the tracked file count, which the benchmark prints to stderr as `bench fixture: ... N tracked files`, by the `noop` time for files per second. The workflow's "Fixture summary" step prints the manifest's text file, directory, and byte counts; the tracked count adds the binaries, symlinks, and `.gitignore`.

## Baseline

Medium preset (10,014 tracked files, 59 MB), median time per iteration from `bench-results.txt`. The CI columns are run [38938632308](https://github.com/reef-vcs/reef/actions/runs/38938632308) at commit `3352127`, run from the R0.06 branch because `workflow_dispatch` only works once the workflow is on `main`. Runner hardware is GitHub's published spec for public repositories. The local column is a run of the same code for comparison.

| Benchmark | `ubuntu-24.04` (x64, 4 vCPU, 16 GB) | `macos-15` (arm64 M1, 3 vCPU, 7 GB) | Apple M2 Max, 12 cores, 32 GB, macOS 27.0.1, APFS |
| --- | ---: | ---: | ---: |
| `noop` | 18.1 ms | 24.4 ms | 15.7 ms |
| `modify_1` | 27.3 ms | 58.9 ms | 53.7 ms |
| `modify_50` | 50.9 ms | 129.4 ms | 95.1 ms |
| `modify_500` | 201.2 ms | 610.6 ms | 304.8 ms |
| `add_50` | 54.0 ms | 135.7 ms | 106.7 ms |
| `op_log_100` | 0.85 ms | 1.66 ms | 1.68 ms |
| `open` | 0.42 ms | 0.48 ms | 0.49 ms |
| First snapshot of the fixture (stderr, once) | 2.2 s | 5.2 s | 2.6 s |

The runners are as noisy as warned above. An earlier run of nearly the same code ([38936744617](https://github.com/reef-vcs/reef/actions/runs/38936744617), which still timed settings construction inside `open`) measured `ubuntu-24.04` `modify_1` at 47.3 ms and `macos-15` `modify_500` at 526 ms.

Every benchmark scales with the tree as well as with the change. A no-op snapshot scans all 10,014 files (about 640,000 files per second on the M2 Max), and even a one-file change costs 27 to 59 ms, mostly in committing the operation and saving the working-copy state, and ADR 0008 found that saving the state is O(tracked files). On the `large` preset (100,014 files) on the M2 Max, `noop` takes 146 ms, `modify_1` 194 ms, and the first snapshot 24.9 s. Those are the costs Phase 1's watcher-driven snapshots have to remove.

## Watchman-narrowed scans (R1.14)

`crates/reef-daemon/tests/watchman.rs` has a manual measurement, `scan_is_narrowed`, outside criterion:

```sh
cargo test --release -p reef-daemon --test watchman -- --ignored scan_is_narrowed --nocapture
```

It runs the `reef-daemon` binary with automatic snapshots held off, creates one workspace with `watcher.backend` set to `notify` and then to `watchman`, and reports the `scan_ms` of nine explicit snapshots, each after one changed file. `scan_ms` is the time jj spends locking the working copy, checking freshness, and scanning; it does not include committing or saving the working-copy state. Medians on the M2 Max, release build, Watchman 2026.07.06.00:

| Preset | notify (full scan) | Watchman (narrowed) | Ratio |
| --- | --- | --- | --- |
| `medium` (10,014 files) | 19 ms | 13 ms | 1.5× |
| `large` (100,014 files) | 167 ms | 31 ms | 5.4× |

A Watchman query costs about 10 ms whatever the tree's size, so narrowing only pays off well above `medium`, which is why `watcher.backend = "auto"` switches at 50,000 files. These numbers need `WATCHMAN_SOCK`, which the binary sets at start (ADR 0023). Without it, jj runs `watchman get-sockname` for every snapshot, and the `medium` Watchman scan measured 151 ms. The test asserts the 3× bound on `large` only.

## Garbage collection (R1.17)

`crates/reef-daemon/tests/gc.rs` has a manual measurement, `medium_thousand_snapshots_then_gc`, and its control, `medium_thousand_snapshots_then_gc_control`:

```sh
cargo test --release -p reef-daemon --test gc -- --ignored medium --nocapture --test-threads 1
```

Each registers the `medium` preset, creates one workspace, appends a line to one file three directories deep and takes an explicit snapshot, 1,000 times (watcher snapshots write the same operations), then runs `Maintenance.Gc`. The measurement uses `gc.undo_window = "0s"`, which abandons every operation but the head. The control uses a one-hour window, which abandons nothing, so `git gc` packs the same objects without pruning any. On the M2 Max, release build, git 2.50.1:

| | `undo_window = "0s"` | Control, `"1h"` |
| --- | ---: | ---: |
| Git objects after the first snapshot | 74.5 MiB | 74.5 MiB |
| Git objects after 1,000 snapshots | 101.8 MiB | 101.8 MiB |
| Git objects after GC | 39.8 MiB | 40.5 MiB |
| Operation store and git objects (the report's `jj_bytes_*`) | 109.6 → 39.8 MiB | 109.6 → 48.4 MiB |
| Operations after GC | 2 | 1,004 |
| Index rebuild (`reindex_ms`) | 21 ms | 109 ms |
| GC in all | 3.8 s | 4.9 s |

The 1,000 snapshots took about 55 s. The acceptance criterion (git objects within 10% of their size after the first snapshot) holds in both columns, because `git gc` packs the loose objects and deltas the 1,000 versions of one file against each other. Pruning the hidden snapshots saved only 0.7 MiB of git objects. Abandoning the old operations freed the rest of the difference: the operation store's 8.6 MiB of operations and views, which `git gc` cannot touch. The index rebuild walks every surviving operation, so its cost follows the operations newer than the undo window: 109 ms for 1,004 here. R1.23 should measure it on a long log before GC runs more often or automatically.

## Workspace creation (R1.12)

Phase 1's exit criterion is quantitative: twenty workspaces on the 100k-file synthetic repo, each created in under one second on APFS and overlayfs, with disk usage per workspace proportional to modified files. `crates/reef-daemon/benches/workspace_create.rs` measures it, and how every other backend compares.

### Running them

```sh
REEF_BENCH_PRESET=small just bench -- create       # every create group on small: about 6 minutes
just bench-ws                                      # the large preset, the exit-criterion run: see below
REEF_BENCH_PRESET=large REEF_BENCH_WS_MAX=20 just bench -- 'create/large'
REEF_BENCH_PRESET=medium just bench -- create_api  # only the daemon group
REEF_BENCH_WATCHER=notify REEF_BENCH_PRESET=large cargo bench -p reef-daemon --bench workspace_create -- 'create_api/large'
```

`just bench` runs them with the snapshot benchmarks; `just bench-ws [preset]` runs only these, with `--sample-size 10`. The filter is a regular expression over benchmark ids, so `create` also selects `create_api`. `REEF_BENCH_WS_MAX` caps the workspace counts: by default 20 on `large` and 100 on `small` and `medium`, since tearing down a hundred `large` workspaces alone takes minutes. `REEF_BENCH_PRIVILEGED=1`, run as root on Linux, benchmarks kernel overlayfs and nothing else (see CI below). `REEF_BENCH_WATCHER` (`notify` or `watchman`; unset is `auto`) sets `watcher.backend` on `create_api`'s daemon (R1.29). The filtered `create_api/large` run above takes about ten minutes.

### Where the benchmarks live

R0.06 decided that "benches live in `crates/reef-lib/benches/`". R1.12 amends that: benches live in the crate that owns the code they measure, and fixtures are shared through `target/bench-fixtures/`. The backends are daemon code, and `reef-lib` cannot depend on `reef-daemon`, so these benchmarks are in `crates/reef-daemon/benches/`. Their fixture builder (`benches/common/mod.rs`) is a copy of `reef-lib`'s with the same seed, `FIXTURE_VERSION`, and `.ready` stamp, so whichever crate's benchmarks run first build `target/bench-fixtures/<preset>-42/` and the other reuses it. Keep the two builders in step.

### What each group measures

The first backend benchmark to run builds a base cache entry for the fixture's working-copy commit in a temporary state directory under `target/bench-state/` (removed at the end; a killed run leaves it, and on Linux possibly overlay mounts under it). Its build time is printed (`bench base: materialized ...`) and not benchmarked. Every available backend is probed for that directory, and the unavailable ones are printed with the reason.

| Benchmark | Timed | Not timed |
| --- | --- | --- |
| `create/<preset>/<kind>/<n>` | `Backend::materialize` of `n` workspaces, one after another | tearing them down, on a few threads, deleting inline |
| `tear_down/<preset>/<kind>` | `Backend::tear_down` of one workspace, deletion included (no tokio runtime, so the backend deletes inline) | materializing it |
| `create_api/<preset>/<kind>/<n>` | `n` `Workspaces.Create` calls to an in-process daemon (`TestDaemon`) on the `auto` backend, which resolves to `<kind>`: resolving the revision, the base cache lookup, materializing, `jj` workspace init, seeding `tree_state`, the lease row, starting the workspace's watcher, and the RPC | releasing them without a final snapshot, and waiting for the daemon's background deletions |

`n` is 1, 20, and 100, up to `REEF_BENCH_WS_MAX`. The copy backend runs only `n = 1` above `small`, in `create` and in `create_api` when `auto` resolves to it: a copy costs the same whatever `n` is, and twenty of them on `large` would take minutes per iteration. `create_api` runs `n` = 1 and 20, with one daemon for the group, started when its first benchmark runs; its first create builds the daemon's own base and is printed (`bench base: the daemon's first create took ...`), not timed. The daemon registers the shared fixture, so the fixture's operation log gains each workspace's creation and release.

Criterion's times are per workspace: each iteration's total divided by `n`. Every benchmark takes 10 samples with flat sampling (the same iteration count in each), and the measurement time is 30 s for `small`, 60 s for `medium`, and 120 s for `large`. The untimed teardown still counts toward it, so the slow benchmarks run their minimum of 11 iterations (one warm-up, ten samples) and take longer than the measurement time; criterion warns that it could not complete 10 samples in time, which is expected.

### The disk rows

After each `create` and `create_api` benchmark a disk pass creates `n` more workspaces and prints one Markdown row to stderr:

```text
| group | preset | kind | n | p50 create ms/ws | disk MiB/ws | disk MiB/ws after modify 50 | tear_down ms/ws |
```

- **p50 create ms/ws**: the median over every workspace the benchmark created, warm-up included. This, not criterion's mean, is what the phase target is judged on.
- **disk MiB/ws**: free space on the workspace directory's volume (`statvfs`, after a `sync`, the median of three samples) before and after creating the `n` workspaces, divided by `n`. Measured, not estimated with `Backend::disk_usage`.
- **disk MiB/ws after modify 50**: the same plus the free space lost when `synth::mutate` then modifies 50 text files in each workspace. The generator's manifest, which `mutate` needs, is copied into each workspace before that measurement and is not counted.
- **tear_down ms/ws**: the median of tearing the `n` workspaces down one at a time. For `create_api` it is the whole `Release` call: the daemon walks the workspace to report what it frees, forgets the jj workspace, renames the root away, and removes the working-copy state before answering; only deleting the renamed root runs in the background. On `large` that took 1.0 to 2.3 s per workspace on the M2 Max, against 4.3 s for the backend's inline teardown.

A `proportional` line follows each row. For copy-on-write backends it checks that the modified column is within 2× of `disk MiB/ws` plus the 50 modified files' allocated size (printed as `modified`). For `copy` it prints the base tree's allocated size beside the modified column instead, which should be about equal. On `small` both checks are within free-space noise (hundredths of a MiB), so a MISS there means little. `n = 20` rows (and copy's `n = 1` row when that is all it runs) add a target line, on every preset; only `large`'s is the phase exit criterion:

```text
target (create_api/large/apfs/20): 20 workspaces under 1 s each: PASS (0.512 s)
```

It never fails the run; RX.06 decides the threshold and the gate. Each row is also appended as a JSON line to `target/bench-ws.json`, with the OS, whether the run was privileged, the target result, and for `create_api` the median `materialize_ms` the daemon reported, the median of each step it timed (`timings`, R1.29), and the watcher it started. A `breakdown` line prints the steps:

```text
breakdown (create_api/large/apfs/20): p50 per create 961.8 ms (daemon 961 ms): resolve 0, base 0, materialize 895, mount 0, jj 52, seed 2, insert 0, watch 9 ms; watcher notify
```

Each step's median is taken on its own, so they need not add up to the daemon's total. Free space is shared with everything else on the volume (criterion's reports, other processes, APFS's own housekeeping), so read the disk columns from a quiet machine, and trust the modified column's difference more than either absolute value.

### CI

`bench.yml` runs these with the snapshot benchmarks on both runners, with `fuse-overlayfs` installed on `ubuntu-24.04`. Then the Linux job runs this bench a second time as root with `REEF_BENCH_PRIVILEGED=1`, for kernel overlayfs, the way `ci.yml`'s overlayfs job runs its tests (`sudo -E env "PATH=$PATH" ...`), and gives `target/` back to the runner user. Its bencher output is `bench-results-privileged.txt`, and both runs append to `target/bench-ws.json`; the artifact holds both files. The workflow does not install Watchman, so `create_api` on the runners uses the `notify` watcher whatever the preset, unlike a `large` run on a machine with Watchman; the JSON's `watcher` field says which ran.

### Baseline

Apple M2 Max, 12 cores, 32 GB, macOS 27.0.1, APFS, `cargo bench` (release with debug info), Watchman 2026.07.06.00 on `PATH`. `large` is 100,014 tracked files and 362 MB; the bench base took 31.7 s to materialize. The `large` run took 53 minutes and the `medium` run 17.

| Benchmark | p50 create ms/ws | disk MiB/ws | after modify 50 | tear_down ms/ws |
| --- | ---: | ---: | ---: | ---: |
| `create/large/apfs/1` | 909 | 29.7 | 35.2 | 4,257 |
| `create/large/apfs/20` | **961** | **29.0** | 34.8 | 4,322 |
| `create/large/copy/1` | 17,830 | 78.7 | 86.1 | 4,589 |
| `create_api/large/apfs/1` | 2,312 | 34.5 | 40.0 | 1,601 (`Release`) |
| `create_api/large/apfs/20` | **1,933** | **34.1** | 39.0 | 2,346 (`Release`) |
| `create/medium/apfs/1` | 156 | 3.0 | 3.7 | 516 |
| `create/medium/apfs/20` | 132 | 3.0 | 3.8 | 446 |
| `create/medium/apfs/100` | 82 | 3.1 | 3.9 | 432 |
| `create/medium/copy/1` | 1,778 | 4.6 | 5.3 | 445 |
| `create_api/medium/apfs/1` | 316 | 3.8 | 4.5 | 91 (`Release`) |
| `create_api/medium/apfs/20` | 245 | 3.6 | 4.6 | 284 (`Release`) |

`tear_down/large/apfs` took 4.32 s and `tear_down/large/copy` 4.47 s; on `medium`, 0.41 s and 0.40 s.

Against the exit criterion:

- **`create_api/large/20` misses: 1.93 s per workspace, not under 1 s.** The backend alone makes it (`create/large/apfs/20`, 0.96 s), and so does one workspace through the daemon when nothing else is going on: a second run of `create_api/large` measured 0.95 s at `n = 1`, 0.89 s of it the clone. Twenty in a row are slower because each new workspace gets a Watchman watch (`auto` picks Watchman above 50,000 files), and Watchman's crawl of its 100,000 files runs while the next workspace is being cloned. That second run's `breakdown` line put the clone at 1.83 s of a 1.89 s create, against 0.96 s for the same clones with no daemon. With `watcher.backend = "notify"` the clone stays at 0.95 s, but starting the watcher (notify's debouncer walks the tree to build its file-id cache) adds 1.1 to 1.3 s inside the create, so both watchers cost about a second per `large` workspace. The full run's `create_api/large/apfs/1` measured 2.3 s, close to what a `notify` watcher costs, but that run predates the `breakdown` line that names the watcher, so why it differs from the second run's 0.95 s is unconfirmed. Making the target needs the watcher's start-up kept off the create path and out of the next clone's way: see [Workspace creation off the watcher's path (R1.29)](#workspace-creation-off-the-watchers-path-r129), which does both and passes.
- **Disk passes**: 34 MiB per workspace through the daemon (29 MiB of directory clone metadata, plus the working-copy state, chiefly its `tree_state`), under the 50 MiB bound. Modifying 50 files adds 5 to 6 MiB per workspace, while the files themselves allocate 0.25 MiB: APFS copies the cloned directories' metadata on the first write under them. That is within 2× of `disk MiB/ws` plus the files on every copy-on-write row, as the `proportional` lines check.
- **`copy` is not a full copy on APFS.** The copy backend uses `std::fs::copy`, which clones each file (`fclonefileat`) on APFS, so `create/large/copy/1` costs 79 MiB per workspace against the tree's 576 MiB allocated, and 17.8 s (a clone per file) instead of the 26 s and 632 MiB a byte copy measured while R1.07 was written. On ext4 it is a real copy (see the Linux rows below, 608 MiB and 1.4 s). R1.07 documents this and accepts it for the fallback.

#### Linux

Docker Desktop's Linux VM on the same M2 Max: arm64, kernel 6.10.14-linuxkit, 12 vCPUs, 7.7 GB, the state and target directories on an ext4 volume, in a `--privileged` `ubuntu:24.04` container with `fuse-overlayfs` 1.13 and no Watchman, so `create_api` used `notify`. The privileged run (root, `REEF_BENCH_PRIVILEGED=1`) took 11 minutes and the unprivileged one 20. A VM on a laptop is not a Linux server. The CI rows below are `medium`; the runners' `large` numbers for the criterion itself are in [Linux workspace creation off the inotify watch path (R1.32)](#linux-workspace-creation-off-the-inotify-watch-path-r132).

| Benchmark | p50 create ms/ws | disk MiB/ws | after modify 50 | tear_down ms/ws |
| --- | ---: | ---: | ---: | ---: |
| `create/large/overlayfs/1` | 0.2 | 0.02 | 0.97 | 9.2 |
| `create/large/overlayfs/20` | **0.2** | 0.02 | 0.99 | 3.7 |
| `create_api/large/overlayfs/1` | 356 | 5.9 | 6.9 | 106 (`Release`) |
| `create_api/large/overlayfs/20` | **467** | **5.9** | 7.7 | 131 (`Release`) |
| `create/large/fuse-overlayfs/1` | 6.9 | 0.02 | 0.98 | 7.2 |
| `create/large/fuse-overlayfs/20` | 7.3 | 0.02 | 1.02 | 3.9 |
| `create/large/copy/1` | 1,360 | 608 | 608 | 615 |
| `create_api/large/fuse-overlayfs/1` | 1,429 | 5.9 | 6.9 | 106 (`Release`) |
| `create_api/large/fuse-overlayfs/20` | 1,556 | 5.9 | 7.0 | 119 (`Release`) |

`tear_down/large/overlayfs` took 0.90 ms, `tear_down/large/fuse-overlayfs` 0.91 ms, and `tear_down/large/copy` 441 ms.

- **The overlayfs half of the exit criterion holds**: `create_api/large/overlayfs/20` is 0.47 s per workspace and `create/large/overlayfs/20` 0.2 ms, under the brief's 1 s and 0.1 s. The daemon's own cost is most of the 0.47 s: its `breakdown` line puts the mount under a millisecond, and the rest is the jj workspace, seeding the 5.9 MiB of working-copy state, and starting `notify`. R1.29's timings put almost all of it in starting `notify`: 405 ms of a 430 ms create, inotify adding a watch per directory.
- **`fuse-overlayfs` through the daemon misses (1.56 s)** although its mount takes 7 ms. Most of the difference is `notify` starting up: inotify adds a watch per directory, and on a FUSE mount every lookup goes through the `fuse-overlayfs` process. (The debouncer's file ID cache, which walked every file on macOS, was never used on Linux.)
- **The overlay rows fail the `proportional` check, by under a megabyte.** An overlay workspace costs nothing until it is written (0.02 MiB, its empty upper and work directories), so the bound is twice the modified files alone, 0.5 to 0.6 MiB, and modifying 50 files cost about 1 MiB. Copy-up also creates each modified file's parent directories in the upper layer, a block apiece on ext4, which the check does not count. The cost is still in proportion to the modified files, about a thousandth of the 608 MiB tree. RX.06 should allow for the directories if it gates on this check.
- **`copy` on ext4 is a real copy**: 608 MiB per workspace, the tree's allocated size, and 1.4 s.

#### Reflink (R1.10)

The reflink backend is not in the rows above: it needs the state directory on btrfs or XFS, and the bench's is under `target/`. These are its `materialize` alone on the `large` preset (100,000 files) in the same container, on 4 GiB loop-mounted images (`mkfs.btrfs` defaults, `mkfs.xfs -m reflink=1`), from `REEF_TEST_REFLINK_DIR=<mount> cargo test -p reef-daemon --release --lib reflink::tests::timing_large -- --ignored --nocapture`: three runs each, free space read after a `syncfs`.

| Filesystem | materialize, runs 1 to 3 | free space per workspace |
| --- | --- | ---: |
| btrfs | 1.63 s, 1.48 s, 1.84 s | 122 MiB of metadata (245 MiB of disk: metadata is `DUP`) |
| XFS | 1.60 s, 1.69 s, 1.71 s | 55 MiB |

- **Both miss the one-second creation target**, as the R1.10 brief expected: Linux has no directory clone, so each of the 100,000 files costs two opens, a `FICLONE`, two mode changes, and a `utimensat`. The exit criterion names APFS and overlayfs only; reflink is for Linux machines that can mount neither overlay.
- **Neither is proportional to modified files.** XFS spends 55 MiB per workspace on inodes, directories, and extent records. btrfs spends more because it stores files under 2 KiB inline in its metadata, and cloning an inline extent copies it, so the `large` tree's small files are duplicated in every workspace. The test's free-space delta is coarse on btrfs (512 MiB on the first run, when a metadata chunk was allocated, then 0), so the btrfs figure is `btrfs filesystem df`'s metadata `used` across ten `cp -a --reflink=always` copies of the same tree, which make the same `FICLONE` per file. A btrfs subvolume snapshot per base would make creation O(1) and share the metadata too; it is a follow-up listed in R1.10.

#### CI runners

Run [39660853557](https://github.com/reef-vcs/reef/actions/runs/39660853557), dispatched on the R1.12 branch at commit `5cb9ba1` with the `medium` preset (10,014 files). Hardware is GitHub's published spec for public repositories. The `ubuntu-24.04` job took 28 minutes, the root run included; `macos-15` took 35. Neither runner has Watchman, so `create_api` used `notify` on both.

| Benchmark | p50 create ms/ws | disk MiB/ws | after modify 50 | tear_down ms/ws |
| --- | ---: | ---: | ---: | ---: |
| **`macos-15`** (arm64 M1, 3 vCPU, 7 GB) | | | | |
| `create/medium/apfs/1` | 128 | 3.0 | 3.8 | 863 |
| `create/medium/apfs/20` | 109 | 3.0 | 3.8 | 529 |
| `create/medium/apfs/100` | 119 | 2.9 | 3.6 | 714 |
| `create/medium/copy/1` | 2,150 | 3.5 | 4.3 | 465 |
| `create_api/medium/apfs/1` | 312 | 3.5 | 5.5 | 72 (`Release`) |
| `create_api/medium/apfs/20` | 289 | 3.5 | 4.5 | 478 (`Release`) |
| **`ubuntu-24.04`** (x64, 4 vCPU, 16 GB, ext4) | | | | |
| `create/medium/fuse-overlayfs/1` | 5 | 0.02 | 0.77 | 13 |
| `create/medium/fuse-overlayfs/20` | 5 | 0.02 | 0.76 | 11 |
| `create/medium/fuse-overlayfs/100` | 6 | 0.02 | 0.77 | 12 |
| `create/medium/copy/1` | 398 | 82.7 | 82.7 | 1,230 |
| `create_api/medium/fuse-overlayfs/1` | 270 | 0.57 | 1.3 | 58 (`Release`) |
| `create_api/medium/fuse-overlayfs/20` | 268 | 0.56 | 1.5 | 49 (`Release`) |
| `create/medium/overlayfs/1` (root) | 0.2 | 0.02 | 0.83 | 12 |
| `create/medium/overlayfs/20` (root) | 0.3 | 0.02 | 0.77 | 11 |
| `create/medium/overlayfs/100` (root) | 0.4 | 0.02 | 0.76 | 12 |
| `create_api/medium/overlayfs/1` (root) | 104 | 0.56 | 1.4 | 28 (`Release`) |
| `create_api/medium/overlayfs/20` (root) | 101 | 0.57 | 1.3 | 35 (`Release`) |

`tear_down/medium` took 525 ms for `apfs` and 522 ms for `copy` on `macos-15`; 0.69 ms for `overlayfs`, 1.2 ms for `fuse-overlayfs`, and 153 ms for `copy` on `ubuntu-24.04`.

The runners agree with the local runs in shape. APFS clones cost about 3 MiB per `medium` workspace, overlays nothing until written, and `copy` the whole tree on ext4 but not on APFS. Every `n = 20` target passes on `medium`; only `large` is the criterion. On `medium` the `macos-15` runner clones about as fast as the M2 Max (109 against 132 ms at `n = 20`). The overlay rows miss the `proportional` check by the margin explained above.

## Workspace creation off the watcher's path (R1.29)

R1.12's `create_api/large/apfs/20` missed the one-second criterion, all of the excess in the file watcher. R1.29 dropped notify's file ID cache (its debouncer walked and stat'ed every file of a new root, symlinked store mounts included, under the lock that delivers every workspace's events), attached watches on the blocking pool, added per-step create timings, and deferred Watchman subscriptions during a burst of creates.

Apple M2 Max, macOS 27.0.1, APFS, Watchman on `PATH`, `cargo bench -p reef-daemon --bench workspace_create -- 'create_api/large'`, nothing else running, 2026-10-22. "Before" is the R1.29 branch with notify's `RecommendedCache` put back, so the timings are there to read; Watchman's path had no change before the deferral.

| Benchmark | watcher | p50 create ms/ws | `materialize` | `jj` | `watch` | target |
| --- | --- | ---: | ---: | ---: | ---: | --- |
| `create_api/large/apfs/1`, before | notify | 1,781 | 878 | 54 | 848 | |
| `create_api/large/apfs/20`, before | notify | 2,125 | 954 | 54 | 1,109 | MISS |
| `create_api/large/apfs/1`, after | notify | 970 | 900 | 53 | 9 | |
| `create_api/large/apfs/20`, after | notify | **962** | 895 | 52 | **9** | **PASS** |
| `create_api/large/apfs/1`, no deferral | watchman | 932 | 875 | 53 | 0 | |
| `create_api/large/apfs/20`, no deferral | watchman | 1,904 | **1,842** | 56 | 0 | MISS |
| `create_api/large/apfs/1`, deferred | watchman | 938 | 886 | 52 | 0 | |
| `create_api/large/apfs/20`, deferred | watchman | **977** (mean 1,234) | 913 | 54 | 0 | **PASS** |

The other steps (`resolve`, `base` with the base cached, `mount`, `seed`, `insert`) are 0 to 2 ms each. Disk per workspace is unchanged (34 to 38 MiB). The `workspace created` log line carries the same fields; its `materialize_ms` is now the backend's materialization alone, as in `CreateTimings`, while the response's top-level `materialize_ms` keeps R1.05's meaning (base plus backend).

- **notify passes, with 38 ms to spare.** Attaching is now one `FSEvents` stream restart, 9 ms in the median. What is left is the clone, 0.9 s of every create, and about 60 ms of the daemon's own steps (jj's workspace, 52 ms, most of it). As the brief's risks said, this is at the target, not under it with room; a slower Mac would miss on the clone alone.
- **Watchman needed the deferral.** Without it, the clone doubles in a run of twenty (1,842 ms against 875 ms for one) because the previous workspace's crawl and full first scan run beside it. With `watchman_attach_quiet = 2s` and `watchman_attach_defer_max = 10s` (the defaults) the p50 passes, but the bench flatters it: each iteration's releases cancel the subscriptions still waiting, so most creates run with no Watchman work at all. Criterion's mean for the deferred `n = 20` is 1.23 s (1.14 to 1.38 s): a run of twenty lasts about 20 s, longer than the cap, so the first workspaces subscribe mid-run.
- **The end of a burst.** Twenty creates through the CLI on a scratch release daemon (35 s, since each `reef ws create` adds its own process start-up), then 15 s idle: the first ten creates took 1.0 s each with nothing else running. At the 10 s cap the first workspace subscribed, and the next ten queued behind its crawl and came up at the same instant; their ten full first scans took 10.5 s each, and the create that ran beside them took 10.8 s (its clone 10.7 s). The rest of the burst's creates took 1.0 to 1.2 s (one 1.9 s). Every subscription was up 5 s after the last create, and every first scan done 8 s after it; a create 15 s later took 1.03 s. ADR 0023's amendment records the trade-off: a longer cap keeps longer bursts clean but delays each workspace's first snapshots further. The scan cost itself is jj's on a fresh instance (no changed-file list), which a Watchman workspace pays once.
- **Linux, unchanged.** In the same Docker container as R1.12's Linux rows, as root: `create_api/large/overlayfs/1` 310 ms and `create_api/large/overlayfs/20` **430 ms** (PASS), against R1.12's 356 and 467 ms. The timings now show where it goes: `watch` 291 and 405 ms, inotify adding a watch for each of the 8,336 directories (notify on Linux never had the file ID cache); `jj` 5 to 7 ms; the overlay mount under a millisecond. Making that watch cheaper is out of R1.29's scope (one watch per directory is inotify's model, ADR 0023). R1.32 takes it off the create path: see [Linux workspace creation off the inotify watch path (R1.32)](#linux-workspace-creation-off-the-inotify-watch-path-r132).
- **`FSEvents` registration depends on where the executable is.** `FSEventStreamStart` waits on fseventsd, which does work in proportion to the entries in the directory holding the running executable: about 2 µs per entry on this machine, so 0.7 s per watch for a test binary in a `target/debug/deps` of 370,000 files, against about a millisecond for `~/.cargo/bin` or a scratch directory (the same binary, copied). Every notify `watch` and `unwatch` restarts the stream and pays it. The release bench's `target/release/deps` held 5,700 entries, so its `watch` column carries about 10 ms of this. Debug tests that bound attach time measure an empty root first and bound the difference; `create_reports_timings` reports `watch_ms` near 740 ms on this machine for the same reason. An installed daemon is not affected.

## Linux workspace creation off the inotify watch path (R1.32)

The first `large` run on the CI runners (`bench.yml`, main at `4c966c7`, run [40143472665](https://github.com/reef-vcs/reef/actions/runs/40143472665), `ubuntu-24.04`) missed the creation criterion on kernel overlayfs by 6 ms, and 98% of each create was the `watch` step. That step is inotify adding a watch for each of the `large` preset's 8,336 directories, inside `Create`. R1.32 attaches a created workspace's notify watch on Linux after `Create` returns, one workspace at a time, and takes one full scan when it is up ([ADR 0028](adr/0028-watch-attachment-off-the-create-path.md)). `watch_state` is `attaching` until then. `create_api` now waits, untimed, for an iteration's attaches before its releases, and reports the wait as `attach drain` on the `breakdown` line and as `attach_drain_ms` in `bench-ws.json`. On macOS a watch still attaches inside the create, and the drain is 0.

| Benchmark | where | p50 create ms/ws | `watch` | attach drain p50 | target |
| --- | --- | ---: | ---: | ---: | --- |
| `create_api/large/overlayfs/1`, before | runner (root) | 965 | 945 | | |
| `create_api/large/overlayfs/20`, before | runner (root) | 1,006 | 985 | | MISS |
| `create_api/large/fuse-overlayfs/20`, before | runner | 2,408 | 2,375 | | MISS |
| `create_api/large/overlayfs/1`, before (R1.29) | Docker VM (root) | 310 | 291 | | |
| `create_api/large/overlayfs/20`, before (R1.29) | Docker VM (root) | 430 | 405 | | PASS |
| `create_api/large/overlayfs/1`, after | Docker VM (root) | **26.0** | 0 | 430 ms | |
| `create_api/large/overlayfs/20`, after | Docker VM (root) | **29.4** | 0 | 7.6 s | **PASS** |
| `create_api/large/overlayfs/1`, after | runner (root) | **21.0** | 0 | 1.1 s | |
| `create_api/large/overlayfs/20`, after | runner (root) | **19.5** | 0 | 21.7 s | **PASS** |
| `create_api/large/fuse-overlayfs/1`, after | runner | **25.1** | 0 | 3.3 s | |
| `create_api/large/fuse-overlayfs/20`, after | runner | **22.3** | 0 | 80.1 s | **PASS** |

The Docker VM is the one in [Linux](#linux) above: Docker Desktop on the M2 Max, as root, with `REEF_BENCH_WATCHER=notify`. Without it, `auto` picks Watchman for `large` where Watchman is installed. A create there is now the daemon's other steps (`jj` 5 to 7 ms, `seed` 1 to 2 ms) and the RPC. The drain is the twenty attaches in a row, from the last create's return. The runner rows after the change are `bench.yml` with `large`, run [40170090452](https://github.com/reef-vcs/reef/actions/runs/40170090452) on this ticket's branch at `e8d94a6`, `ubuntu-24.04` (no Watchman, so `notify`): the creation criterion passes on kernel overlayfs at 19.5 ms per create, against 1,006 ms before. The `create` rows' `proportional` MISS lines and `copy/1`'s target MISS are the same as before the change.

- **Through FUSE the attaches drain slowly.** Twenty in a row took 80 s, about 4 s each, against 21.7 s (about 1.1 s each) on kernel overlayfs; before the change a FUSE attach was 2.4 s inside the create. The bench does not break the drain down, so why an attach takes longer behind the queue is not established. Until its watch is up, a workspace is snapshotted only by the 60 s fallback, with a full scan. A create no longer waits for it.

- **macOS is unchanged.** On the M2 Max with nothing else running, `REEF_BENCH_WATCHER=notify`: `create_api/large/apfs/1` 940 ms and `create_api/large/apfs/20` **969 ms** (PASS), against R1.29's 970 and 962 ms. `watch` is 12 ms, and the attach drain 0 to 3 ms (one `List` call): watches still attach inside the create there.
- **Queueing must not wait for an attach.** The first version kept the queue under the debouncer's lock, which the attach thread holds across an attach, so each create of a run of twenty waited for the previous workspace's attach: `watch` 456 ms of a 479 ms create at `n = 20`, against 0 at `n = 1`. The queue has its own lock now.
- **The end of a burst, by hand.** A release daemon as root in the same VM, `large` registered, then twenty `reef ws create` in a row. Each took 25 to 39 ms through the CLI and answered `attaching`. Their attaches took 397 ms at the median (483 ms at most, 8.1 s in all). The last one waited 6.8 s in the queue, and the backlog was gone 7.3 s after the last create. Each workspace then got one full scan.
- **The overflow loop.** Before the fix in ADR 0028 (decision 9), the same burst overflowed the kernel's inotify queue 32 times in 12 s. Each overflow makes every notify workspace `everything`, and the workspaces' full scans overflowed it again: about 30 full scans of each workspace during the drain, and it went on until the workspaces were released. The trigger is not new. On main at `10cba6e`, twenty `large` workspaces, a daemon restart, then one write in each started the same loop: 33 overflows in 45 s. With reads dropped before the debouncer and one inotify turn for attaches and full scans, both cases show no overflow and one scan per workspace. Each change alone left one of the two overflowing (the turn alone: 15 overflows in 45 s in the restart case; reads dropped alone: 23 in the burst).


## Reads (R2.02)

`crates/reef-lib/benches/reads.rs` measures the jj side of the read RPCs on the same fixture, as `reads/<preset>/<name>`: `cargo bench -p reef-lib --bench reads` (or `just bench -- reads/`). They use criterion's default sampling; the fixture is mutated only to set up each state.

| Benchmark | Timed |
| --- | --- |
| `status_diff` | describe `@`, list `heads(mutable())`, and diff a one-file edit (summary): a workspace status |
| `log_default` | parse `trunk() \| working_copies() \| mutable()`, the first page of the log, each entry described |
| `change_describe_100` | 100 `Change`s described |
| `diff_summary_full` | the whole tree against `root()`, first page of 100 files plus `total_files` (the O(tree) case) |
| `effects_snapshot` | a snapshot operation's effects, uncached |
| `diff_hunks_50` | 50 modified files with hunks |
| `op_page_100` | 100 operations walked from the heads and classified, no effects |
| `op_range_1` | the one operation after a position one behind the head, walked and classified: what each subscription read per operation before R2.22's broker; since it, what the broker's ingest reads once per operation per followed repository |
| `op_range_1000` | the 1,000 operations after a position, walked and classified: a subscription catching up |

Apple M2 Max, 12 cores, 32 GB, macOS 27.0, APFS, median per iteration:

| Benchmark | `medium` (10,014 files) | `large` (100,014 files) |
| --- | ---: | ---: |
| `status_diff` | 0.47 ms | 0.50 ms |
| `log_default` | 0.09 ms | 0.09 ms |
| `change_describe_100` | 0.39 ms | 0.12 ms |
| `diff_summary_full` | 7.6 ms | 477 ms |
| `effects_snapshot` | 0.05 ms | 0.05 ms |
| `diff_hunks_50` | 9.0 ms | 11.7 ms |
| `op_page_100` | 1.6 ms | 1.6 ms |
| `op_range_1` | 0.048 ms | |
| `op_range_1000` | 17.2 ms | |

Everything but `diff_summary_full` scales with what changed, not with the tree. `diff_summary_full` walks every path, and a page of a large diff pays that walk again to count `total_files`. Before R2.02 skipped rename and copy detection past 1,000 differing files (git's default `diff.renameLimit`), the git backend's detection took 764 ms of its 770 ms on `medium` and grew faster than the file count; the tree diff alone is 6 ms there.

## Replay (R2.12)

Two more groups in `crates/reef-lib/benches/reads.rs`, run after the operation benches on the same fixture: `cargo bench -p reef-lib --bench reads -- 'load_at|locate'`.

| Benchmark | Timed |
| --- | --- |
| `load_at_shared` | after GC's `reinit` (only the head's index kept), the repository at an operation 500 back, borrowing the head's index, uncached: what every read at an operation pays to load it |
| `load_at_rebuilt` | the same operation through jj's own load, which builds and saves an index for it; timed once and printed to stderr, since a second load finds the index it saved |
| `locate_1000` | `GetOperation` by time to the operation 1,000 back, walked from the op heads |

Apple M2 Max, 12 cores, 32 GB, macOS 27.0, APFS, median per iteration, `medium` (10,014 files, about 1,100 operations):

| Benchmark | `medium` |
| --- | ---: |
| `load_at_shared` | 0.017 ms |
| `load_at_rebuilt` | 42 ms |
| `locate_1000` | 16.9 ms |

A read at an old operation after GC cost a full reindex of that operation (42 ms here, 0.16 to 0.44 s measured on Reef's own repository through the CLI while writing the brief) and wrote an index segment; it now costs reading the view. A lookup by time costs about 17 µs per operation after the time, so the 100,000-operation bound is about 1.7 s. `large` was not run: the shared load does not depend on the tree, and the rebuild is the full reindex, which grows with the commits indexed.

## Broker (R2.22)

`crates/reef-broker/benches/broker.rs` measures the subscription broker's in-memory cost per operation ([ADR 0038](adr/0038-subscription-broker.md)), on a ring at its default bound of 4,096 operations of about 1.5 KiB: `cargo bench -p reef-broker --bench broker`. `bench.yml`'s `cargo bench --workspace` runs it.

| Benchmark | Timed | M2 Max, median |
| --- | --- | ---: |
| `broker/append_1` | one operation appended to a full ring: an eviction and two index updates | 271 ns |
| `broker/read_1` | the one entry after a position one behind the head | 48 ns |
| `broker/fanout_500` | 500 readers each read the newest operation, filter it as the load test's classes do, and clone the message they keep: the in-memory fan-out of one operation at Phase 2's target | 41 µs |

Before the broker each of 500 subscribers ran `op_range_1` (0.048 ms) plus two SQLite queries and a hop to the blocking pool per operation; now the ingest runs it once and the subscribers share `fanout_500`. The end-to-end figures, before and after, are in [`load-tests.md`, Subscriptions](load-tests.md#subscriptions-r222).

## Writes (R2.03)

`crates/reef-lib/benches/writes.rs` measures the jj side of the write RPCs on the same fixture, as `writes/<preset>/<name>`: `cargo bench -p reef-lib --bench writes` (or `just bench -- writes/`). Each iteration's setup commits what it writes on, outside the measurement; the fixture's operation log grows with every run.

| Benchmark | Timed |
| --- | --- |
| `describe_wc` | describe the working-copy commit, which has one descendant: rewrite, rebase, commit and publish |
| `abandon_leaf` | abandon a commit with no descendants |
| `abandon_divergent` | abandon one commit of a divergent change whose other commit takes over its child |
| `revert_snapshot` | revert the snapshot just taken, supersession check included |

Apple M2 Max, 12 cores, 32 GB, macOS 27.0, APFS, median per iteration:

| Benchmark | `medium` (10,014 files) | `large` (100,014 files) |
| --- | ---: | ---: |
| `describe_wc` | 35.3 ms | 35.8 ms |
| `abandon_leaf` | 16.5 ms | 17.2 ms |
| `abandon_divergent` | 29.1 ms | 30.9 ms |
| `revert_snapshot` | 17.0 ms | 19.6 ms |

Nothing here walks a tree: `medium` and `large` agree. An operation that writes no commit (`abandon_leaf`, `revert_snapshot`) costs 16 to 17 ms, which is jj-lib committing and publishing an operation (operation, view, index segment, op heads) on APFS; each commit written adds about 9 ms (`describe_wc` writes two, the working-copy commit and its rebased descendant; `abandon_divergent` one). The brief's targets (under 10 to 15 ms) were set before measuring and assumed a cheaper operation commit; the cost is in jj-lib's store writes, not Reef's code, and is paid once per write RPC.

End to end through the daemon, `DescribeChange` of a workspace's `@` (test `describe_change`, 20 calls) has a median of 30.4 ms in the test (debug) build, writing one commit: the transaction above, the follows' row updates, and `written_operation`. `AbandonChange` and `UndoOperation` add a scan of each workspace they move before the write and one after (a scan plus a check-out of the files that differ); see "Baseline" for scan costs.

## Files (R2.04)

`crates/reef-lib/benches/files.rs` measures `reef_lib::jj::files` on the same fixture, as `files/<preset>/<name>`: `cargo bench -p reef-lib --bench files` (or `just bench -- files/`). Setup writes a 16 MiB file and a leaf to edit, each as a new change on the working-copy commit, which never moves; the fixture's operation log grows with every run.

| Benchmark | Timed |
| --- | --- |
| `read_small` | read a small text file at the working-copy commit |
| `read_paged_16mib` | read a 16 MiB file in 1 MiB pages, all 16 and the empty one past the end |
| `list_root` | list the root directory, not recursive |
| `list_recursive_first` | the first page of 100 of a recursive listing of the root |
| `list_recursive_last` | the last page of 100 of the same listing, by cursor |
| `edit_one` | rewrite a leaf commit with one text replacement: overlay, blob, tree, commit and publish |
| `new_on_hundred` | a new change on the working-copy commit writing 100 small files |

Apple M2 Max, 12 cores, 32 GB, macOS 27.0, APFS, median per iteration:

| Benchmark | `medium` (10,014 files) | `large` (100,014 files) | Brief's target |
| --- | ---: | ---: | ---: |
| `read_small` | 21.6 µs | 21.2 µs | < 1 ms |
| `read_paged_16mib` | 75.5 ms | 73.0 ms | < 400 ms |
| `list_root` | 2.6 µs | 4.4 µs | < 2 ms |
| `list_recursive_first` | 23.3 µs | 27.3 µs | < 5 ms |
| `list_recursive_last` | 34.4 µs | 38.0 µs | < 10 to 15 ms |
| `edit_one` | 30.9 ms | 31.4 ms | < 35 ms |
| `new_on_hundred` | 30.1 ms | 31.4 ms | < 60 ms |

`list_recursive_last` close to `list_recursive_first` on `large` is what the cursor matcher buys: the last page skips every subtree before its cursor instead of walking 100,000 entries. `read_paged_16mib` is the known quadratic case (each page reads the whole blob), about 4.6 ms a page here. The edits cost what R2.03's writes do: one commit written plus committing the operation; 100 small blobs add little.

End to end through the daemon (test build), `EditFiles` with no workspace (test `edit_files_without_a_workspace`, 20 calls) has a median of about 30 ms; with one workspace on the edited commit and one on its child, which are snapshotted first and follow afterwards (test `edit_files_moves_workspaces`), one call takes about 230 ms, mostly the scans and the check-out.

## Symbols (R2.14)

Two benches measure the symbol index ([ADR 0042](adr/0042-symbol-index.md)). Both are release builds on the M2 Max, run on 2026-10-28.

**`crates/reef-symbols/benches/extract.rs`** times tag extraction of about 2,000 lines per language: `cargo bench -p reef-symbols --bench extract`. Python, TypeScript and Go repeat the crate's test fixtures; Rust is Reef's own `reef-lib/src/jj/files.rs`. The brief's target was 5 MB/s per core.

| Benchmark | M2 Max, median | Throughput |
| --- | ---: | ---: |
| `extract/python` | 5.28 ms | 6.6 MiB/s |
| `extract/typescript` | 4.51 ms | 6.8 MiB/s |
| `extract/go` | 4.23 ms | 5.7 MiB/s |
| `extract/rust` | 11.7 ms | 9.0 MiB/s |

**`crates/reef-daemon/benches/symbols.rs`** times the index through the daemon on a synthetic repository: `REEF_BENCH_PRESET=medium cargo bench -p reef-daemon --features testing --bench symbols`.
- **The first index** is timed once and printed, since a second run would find every entry.
- **The other three** are criterion benches.
- **About 70% of the synthetic files** have a supported extension, filled with pseudo-source the grammars parse by error recovery. This makes the benches a test of cost, not of what gets extracted.

| Benchmark | `medium` (10,014 files) | `large` (100,014 files) | Target |
| --- | ---: | ---: | --- |
| `full_index`: first index, 4 workers | 6,880 files in 8.4 s | 70,062 files in 92 s | < 20 s, < 200 s |
| `symbols.sqlite3` after it | 12.7 MiB | 87.8 MiB | — |
| `incremental_one_file`: a child commit changing one file | 3.9 ms | 4.2 ms | < 25 ms |
| `list_file`: `ListSymbols` of one file | 1.09 ms | 0.39 ms | < 2 ms |
| `list_name`: `ListSymbols` by name, every page | 2.3 ms (529 occurrences; 2.9 ms before R2.16) | 177 to 187 ms (4,973 occurrences; 387 ms before R2.16) | < 20 ms for 50; < 200 ms on `large` |
| `list_name_deep`: one page of 10 of that name, 40 pages in (R2.16) | 0.68 ms (1.33 ms before) | 16 to 17 ms (118 ms before) | — |

The `list_name` rows were measured again on 2026-10-30, before and after R2.16 moved the filters and the cursor of a name or symbol query into SQL. The store now reads the name's rows in jj's path order (the `reef_path` collation), starting after the cursor, with the path range, roles, kinds, the current extractor and SCIP's precedence over tags applied in the statement. Each row is then checked against the tree, and the read stops after the page's last row. Before, every page read and checked every stored hit, then sorted them.

On `large`, the listing is five pages of 1,000, and each costs two things:
- **One statement, 9 to 16 ms.** It sorts the name's 4,973 rows in a temporary B-tree, fewer as the cursor advances. Its cost is reading each row's columns and the check for an upload, not the sort.
- **A tree lookup per distinct path the page meets, about 25 µs each.** That is about 800 a page, and 4,064 in all. A path is looked up once per page, not once per hit.

A later page costs no more than the first, and `list_name_deep` is the statement and 11 lookups. The listing still costs more than the brief's estimate of 20 ms, because the lookups alone add up to about 100 ms. Making them cheaper is a matter for the tree walk in `reef-lib`, not the store.

**Binary sizes** come from the `dist` profile on the same Mac, before (`main` at `3a14316`) and after:

| Binary | Before | After |
| --- | ---: | ---: |
| `reef-daemon` | 20.1 MB | 25.5 MB (+5.5 MB) |
| `reef` | 7.67 MB | 7.90 MB (+0.23 MB) |

`reef`'s growth is the `reef symbols` commands and their JSON. It links neither tree-sitter nor `reef-symbols`, which `crate_graph` checks.

## History (R2.16)

`crates/reef-daemon/benches/history.rs` measures the history index ([ADR 0044](adr/0044-history-index-and-query-layer.md)) through the daemon. Release build on the M2 Max, run on 2026-10-30, on the `small` synthetic repository with 1,000 `EditFiles` changes by 50 agents, each adding a Python file of ten functions under one of 20 directories (10,000 symbol touches). The brief planned a generated fixture of 20,000 changes and 400,000 touches written straight into the store; the bench drives the real follower and worker instead, so its fixture is smaller.

| Benchmark | Result | Target |
| --- | ---: | --- |
| `follow`: 1,000 edits (33.6 s), then the last edit to its row | 8.8 ms | < 1 s for 1,000 operations |
| the same, to every change's touches computed | 10.0 ms | — |
| `query_symbol`: one page by a name 5% of changes define | 0.36 ms | < 20 ms |
| `query_path_dir`: one page by one of the 20 directories | 0.99 ms | < 20 ms |
| `query_agent_week`: one page by one agent within seven days | 1.29 ms | < 20 ms |
| `group_agent_outcome`: counts by agent and outcome over every change | 0.50 ms | < 100 ms |

The worker kept up with the edits, one change at a time, so nothing was pending a few milliseconds after the last one. The queries read indexes only: test `history::query::tests::query_plans_use_indexes` checks each plan.

## Symbol-level diff (R2.15)

`crates/reef-daemon/benches/symbols.rs` gained the symbol diffs of `GetDiff` ([ADR 0043](adr/0043-symbol-level-diff.md)). These are release builds on the M2 Max, run on 2026-10-30, on the synthetic repositories above. The diffs rewrite the trees' own Python files with 20 definitions each, and every definition calls the bench's common name. Call sites are listed up to 10 per change.

| Benchmark | `medium` | `large` | Target |
| --- | ---: | ---: | --- |
| `diff_one_file_plain`: one rewritten file, no symbols | 0.28 ms | 0.36 ms | — |
| `diff_one_file`: the same with symbols, warm | 6.7 ms | 7.3 ms | < 15 ms above plain |
| `diff_100_files`: 100 rewritten files, warm | 72 ms | — | < 250 ms |
| `diff_100_files_cold`: the same, the new side's 100 files without entries and its tree not indexed (extraction and the wait for its pass included), timed once | 147 ms | 634 ms | < 1.5 s |
| `diff_paged`: one page of 20 of those 100 | 37 ms | — | < 300 ms |
| `call_sites_common`: one changed function named as `list_name`'s name | 9.3 ms | 25 ms | < 100 ms on `large` |
| `list_name` (R2.14's bench, after `find_rows`) | 2.7 ms (was 19 ms) | 402 ms (was 1.29 s) | < 200 ms on `large` |

A page re-matches the whole diff's definitions, and that costs less than the 200 ms over a plain page that would call for a cache between pages (the brief's open question 3), so there is none.

`list_name` on `large` missed its target. Reading only the matching rows took it from 1.29 s to 402 ms. What remains is the tree check: each of the 4,973 hits' paths is looked up in the tree one at a time. Call-site lookups filter by kind and language family before that check, so they make far fewer lookups. R2.16 then moved the filters and the cursor into SQL, which took it to 177 to 187 ms (see "Symbols (R2.14)").

## Status index (R2.23)

`crates/reef-daemon/benches/status.rs` measures a workspace's `GetStatus` through the daemon, from the incremental status index and from a walk ([ADR 0048](adr/0048-status-from-the-watchers-journal.md)), one notify workspace on the synthetic preset. Release build on the M2 Max, run on 2026-10-31. The `walk` rows mark the journal `events-lost` before each call, so the read's snapshot scans every tracked file; an `index_*` call that answers other than `index` panics. Before R2.23 every status walked: 182 ms on `large` through `reef status -o json`, process start included.

| `status/<preset>/…` | `medium` (10,014 files) | `large` (100,014 files) | Target, `large` |
| --- | ---: | ---: | --- |
| `walk`: unchanged tree | 18.4 ms | 161.4 ms | recorded |
| `walk_one_edit`: after one file appended | 55.7 ms | 206.9 ms | recorded |
| `index_clean`: unchanged, no snapshot | 12.2 ms | **12.2 ms** | < 15 ms, ≥ 10× `walk` (13.2×) |
| `index_one_edit`: one file appended just before the call | 60.9 ms | **87.5 ms** | < 100 ms, ≥ 2× `walk_one_edit` (2.4×) |
| `index_100_edits`: 100 files appended before the call | 73.6 ms | **91.9 ms** | < 250 ms |
| `skip_snapshot`: the floor | 5.9 ms | 5.3 ms | recorded |

- **Clean costs the same at both sizes**: the sync's cookie round trip and the read, about 7 ms over the floor, whatever the tree.
- **An edit costs its snapshot**: recording the commit, the operation and the working-copy state. On `medium` a walk is cheap enough that the index answer with one edit is no faster; on `large` it is 2.4 times faster, and the gap grows with the tree and with load (below).
- Under the load test's 200 `large` workspaces, every status after a write answered from the index, at p95 446 ms ([load tests](load-tests.md), R2.23), where a loaded full scan took 11.3 s (R1.30).

```sh
REEF_BENCH_PRESET=large cargo bench -p reef-daemon --features testing --bench status
```


## Evidence (R3.01)

`crates/reef-evidence/benches/evidence.rs` measures the evidence format ([ADR 0055](adr/0055-evidence-object-format.md)): `cargo bench -p reef-evidence --bench evidence`. Release build on the M2 Max, run on 2026-11-05.

| Benchmark | Timed | M2 Max, median | Target |
| --- | --- | ---: | --- |
| `evidence/sign` | build a claim with three artifacts, write its payload, check it as `Verified::check` would, and sign it into a canonical envelope | 21.5 µs | < 100 µs |
| `evidence/check` | `Verified::check` of that envelope: the duplicate-key and depth walks, base64, the key checks, the signature, the statement's checks, the canonical form and its SHA-256 | 44.2 µs | < 150 µs |
| `evidence/check_64k` | `Verified::check` of an envelope just under 64 KiB, most of it in `annotations` | 182 µs | < 2 ms |

Through the daemon, `tests/evidence.rs` prints medians as trend data, not gates (release build, the same machine, two tests running at once): `AddEvidence` 0.47 ms and `GetEvidence` 0.21 ms over 50 calls (`add_and_get`; expected under 3 ms and 1 ms), and a `ListEvidence` page of 50 with every signature checked again 2.6 ms (`list_filters_and_pages`; expected under 10 ms). A debug build, as `cargo nextest` runs them by default, is about ten times slower: 3.7 ms, 0.93 ms and 23 ms. Nothing changes on an existing path: one migration at start, and a repository's forget deletes two more tables' rows.

```sh
cargo nextest run --release -p reef-daemon --test evidence --success-output immediate -E 'test(add_and_get) | test(list_filters_and_pages)'
```

## Artifacts (R3.02)

`crates/reef-daemon/tests/artifacts.rs` holds two ignored tests that time the artifact store ([ADR 0056](adr/0056-evidence-artifact-store-and-retention.md)) through a test daemon over its socket. Release build on the M2 Max, run on 2026-11-06, three runs each, one test at a time; the table gives the median run. They are trend data, not gates.

| Measurement | Timed | M2 Max, median | Expected |
| --- | --- | ---: | --- |
| upload | a 256 MiB file through `Client::upload_artifact_file`: hash, probe, 256 chunks of 1 MiB, and the daemon's hash at commit | 488 ms (525 MiB/s) | > 200 MiB/s |
| dedup | the same file again: the client's hash, then one probe that answers "stored" | 124 ms, of which the probe 0.45 ms | hashing time + < 2 ms |
| read | `ReadArtifact` of the whole file, every message drained, re-hashed by the daemon as it sends | 178 ms (1,441 MiB/s) | > 300 MiB/s |
| gc step | GC's seventh step (`artifacts::retention::run`) over 100,000 evidence objects, 20,000 artifacts and 2,004 live commits: 65,000 objects deleted, 5,000 artifacts collected | 4.62 s, of which `live_set` 60 ms | < 3 s, of which `live_set` < 1 s |

- **The GC step is 1.5 times its expectation, and its commits are why.** A profile of the step (`sample`) put about 1.6 s in committing: 65 transactions of 1,000 objects each (decision 5), and each commit's WAL grows past SQLite's auto-checkpoint, which then copies and syncs the pages (`synchronous = NORMAL` syncs only at checkpoints). The per-object deletes, four indexed statements each, take about 0.6 s. Preparing those four once per transaction rather than once per object took the median from 5.02 s to 4.62 s. Larger transactions would cut the checkpoints but hold the database's write lock longer; the 1,000 stays. The fixture's 65,000 deletions are a worst case: a timed GC deletes only what aged out since the last run.
- **`live_set` is not the cost**: 2,004 commits evaluated in 60 ms.
- **Dedup costs the hash**: the probe is under a millisecond, so a repeated upload costs reading the file once.

The protocol tests print medians too, as trend data (release, the same machine): `UploadArtifact` 1.0 ms per 1 MiB chunk (`upload_protocol`), `ReadArtifact` of 5 MiB in 4.6 ms (`read_stream`), and a GC dry run with the new step in 16 ms on the small fixture (`retention_pins`). A debug build, as `cargo nextest` runs them by default, takes 3.7 ms and 16 ms for the first two. Nothing changes on an existing path but one migration and its backfill at start, and one more table in `AddEvidence`'s insert.

```sh
cargo nextest run --release -p reef-daemon --test artifacts --run-ignored only --success-output immediate -E 'test(=bench_upload_dedup_read)'
cargo nextest run --release -p reef-daemon --test artifacts --run-ignored only --success-output immediate -E 'test(=bench_gc_step)'
```

## Identity (R3.03)

`crates/reef-evidence/benches/evidence.rs` measures session-key certificates ([ADR 0057](adr/0057-operator-identities-and-session-keys.md)): `cargo bench -p reef-evidence --bench evidence -- session`. Release build on the M2 Max, run on 2026-11-06. Trend data, not gates.

| Measurement | Timed | M2 Max, median | Expected |
| --- | --- | ---: | --- |
| `session/certify` | build a binding's statement, check it as `check_certificate` would, and sign it into a canonical envelope | 19.9 µs | < 100 µs |
| `session/check` | `check_certificate` of that envelope: the envelope's walks, the authority by `keyid`, the signature, the statement and predicate checks, the canonical form | 42.8 µs | < 100 µs |
| issue | `Client::issue_session_key` over the socket: key generation, `ListAuthorities` for the proof's audience, the proof, the RPC (the proof's check, the rules, signing, one transaction with the cap's count), and the read back with its certificate checked | 0.50 ms | < 3 ms |
| `evidence add` by session | the CLI end to end against a release daemon, median of 25 runs, three runs: a session key issued, signing, and the add, against R3.01's `--key` path | 12.2 ms against 12.2 ms (+0.04 ms) | < 5 ms more |
| list page | a `ListEvidence` page of 50 objects, each by its own session key, every signature and certificate checked again | 5.2 ms | < 15 ms (R3.01's, unbound, 2.6 ms) |

- **A page costs one certificate check per key**: 50 checks at about 43 µs are the 2.5 ms between R3.01's page and this one; the key lookup is one `IN` query.
- **The proof's audience costs one READ call.** Since the review, `issue` reads the current authority (`ListAuthorities`) before it signs the proof: 0.46 ms became 0.50 ms.
- **`evidence add` by session costs no more than a key file.** The extra RPC and the key generation, about half a millisecond, are lost in the command's process start, connection and snapshot; reading and checking a key file costs about as much.
- A debug build, as `cargo nextest` runs the daemon tests by default, is about eight times slower: 3.9 ms per issue and 45 ms per page. Nothing changes on an existing path but one migration and the authority check at start, the key lookup per evidence page, and the issuing RPC per `evidence add`.

```sh
cargo bench -p reef-evidence --bench evidence -- session
cargo nextest run --release -p reef-daemon --test identity --success-output immediate -E 'test(issue_over_the_socket) | test(evidence_reports_identity)'
```

## Provenance (R3.04)

`crates/reef-evidence/benches/evidence.rs` measures a provenance record's format ([ADR 0058](adr/0058-provenance-records.md)), and `crates/reef-daemon/tests/provenance.rs`'s `timings` the daemon's paths against two in-process daemons, one with `[provenance] enabled = false`. Release build on the M2 Max, run on 2026-11-06.

| Measurement | Timed | M2 Max, median | Expected |
| --- | --- | ---: | --- |
| `provenance/sign` | build a record's statement, check it as `check` would, and sign it into a canonical envelope | 19.4 µs | < 50 µs |
| `provenance/check` | `provenance::check` of that envelope | 42.1 µs | < 100 µs |
| record (gate) | `Provenance::record` as a snapshot's blocking half calls it: the session read, the held key's check, signing, and the insert with its key row, 200 times | 0.12 ms; p99 1.75 ms | < 0.5 ms; p99 < 2 ms |
| snapshot of a one-file edit | an explicit snapshot, 60 each with provenance and without, interleaved | 42.0 ms against 39.3 ms (a second run: 39.9 against 41.9) | within noise |
| `DeclareProvenance` | end to end, nothing to snapshot | 2.6 ms | < 5 ms |
| list page | `ListProvenance` page of 50, every record and certificate checked again | 2.8 ms | < 10 ms |
| storage | `reef.sqlite3`'s growth over 200 records | about 5.7 KB per record | about 1.3 KiB |
| load (gate) | `load.rs` `medium` × 200, release, every snapshot recording | passed: steady write-to-snapshot p50 / p95 770 / 878 ms, no write unsnapshotted after 30 s | the existing 5 s gate (R1.30: p95 729 ms) |

- **The snapshot gate is the record's own cost.** An explicit snapshot of the synthetic repository scans it whole and takes about 40 ms, so two runs put provenance 2.7 ms slower and 2 ms faster: noise. The record path alone is 0.12 ms at the median; its p99 is the renewal, which certifies a new key.
- **Storage is four times the estimate.** A record's envelope is about 1 KiB of base64, and the table has five indexes, one over the envelope's row. At one record per committing watcher snapshot, a workspace that commits every 10 s for 8 hours writes about 16 MB; compaction after `[provenance] compact_after` keeps two records per run. The load test (`load.rs` medium × 200) is the gate on fleet-wide cost.
- A debug build, as `cargo nextest` runs the daemon tests by default, is about eight times slower.

```sh
cargo bench -p reef-evidence --bench evidence -- provenance
cargo nextest run --release -p reef-daemon --test provenance --success-output immediate -E 'test(timings)'
```

## Executor protocol (R3.05)

`crates/reef-daemon/tests/executor.rs`'s `timings` (ignored by default) measures the job queue's paths against an in-process daemon, and `claims_match_and_order` prints a long-polling claim's latency after a submit ([ADR 0059](adr/0059-executor-protocol.md)). Release build on the M2 Max, run on 2026-11-07. All trend data; none is a gate.

| Measurement | Timed | M2 Max, median | Expected |
| --- | --- | ---: | --- |
| submit | `SubmitJob` by `tree_id`, end to end, 1,000 jobs | 1.07 ms | < 5 ms |
| claim latency | a waiting `ClaimJob`'s return after a `SubmitJob` starts, including the submit | 14.7 ms | < 50 ms |
| claim selection | `ClaimJob` with `wait` 0 over 1,000 open jobs, none matching | 1.61 ms | < 5 ms |
| heartbeat | `UpdateJob` with no evidence | 1.88 ms | < 3 ms |
| update with evidence | `UpdateJob` with 50 envelopes, each verified and stored | 6.2 ms | < 50 ms |
| `GetJob` | a job with 500 accepted objects and one coverage entry, re-verified | 27.1 ms | < 150 ms (1,000 entries) |

- **No existing path changed** but `AddEvidence`'s split (its tests and benchmarks unchanged), presence's added kind, and GC's added step.
- The claim latency is mostly the submit's own round trip and transaction: a waiting claim wakes on the submit's `Notify` and claims in one more transaction.

```sh
cargo nextest run --release -p reef-daemon --test executor --run-ignored ignored-only --no-capture -E 'test(timings)'
cargo nextest run --release -p reef-daemon --test executor --no-capture -E 'test(claims_match)'
```

## Reference executor (R3.06)

`crates/reef-executor/tests/executor.rs`'s `timings` (ignored by default) measures the executor against an in-process daemon ([ADR 0060](adr/0060-reference-executor.md), [docs/executor.md](executor.md)). Release build on the M2 Max, run on 2026-11-08, Seatbelt. All trend data; none is a gate.

| Measurement | Timed | M2 Max, median | Expected |
| --- | --- | ---: | --- |
| sandbox start | `true` under a generated Seatbelt profile, less `true` alone (30 runs) | 7.1 ms | < 30 ms |
| job overhead | claim to completion for one pair running `true`, warm cache, socket (7 jobs) | 297 ms | < 300 ms |
| materialize, `small` | 1,014 entries, 28 MB: cold / warm / one changed file, socket, disk check on | 0.95 s / 0.19 s / 0.20 s | < 3 s / < 300 ms |
| materialize, `medium` | 10,014 entries, 59 MB: cold / warm / one changed file | 4.1 s / 1.8 s / 1.8 s | < 30 s / < 2 s |
| disk check | `verify = "disk"` over `listing`, warm page cache: `small` / `medium` | 36 ms / 191 ms | < 1 s (`medium`) |
| remote materialize | `small`, cold, over the remote listener on loopback with a `ci` token | 0.95 s | < 10 s |

- **No existing path changed.** The executor calls only RPCs that exist.
- **A warm tree is mostly the listing and the layout:** `ListFiles` of 10,000 entries and 10,000 files copied out of the blob cache. Reef's own tree (about 1,100 files) materializes cold in about a second from the installed daemon, as the dogfood run in the PR showed.
- **The job's overhead** is two round trips for the session key (its authority, then the issue), the listing and layout of the warm `small` tree, the check, the log's upload, and the completion.

```sh
cargo nextest run --release -p reef-executor --test executor --run-ignored ignored-only --no-capture -E 'test(timings)'
```

## Policy evaluation (R3.07)

`crates/reef-policy/benches/policy.rs` measures loading and evaluating policies ([ADR 0061](adr/0061-repository-policy-file.md), [docs/policy.md](policy.md)): `cargo bench -p reef-policy --bench policy`. Release build on the M2 Max, run on 2026-11-08. Every load and evaluation includes starting its 64 MiB-stack thread. All trend data; none is a gate.

| Benchmark | Timed | M2 Max, median | Expected |
| --- | --- | ---: | --- |
| `policy/load_starter` | parse, check and compile `reef policy init`'s starter | 94 µs | < 2 ms |
| `policy/load_200_rules` | the same for 200 `[[rule]]`s, each with a `when` and a macro over `change.paths` | 6.3 ms | < 50 ms |
| `policy/evaluate_small` | the starter over 50 paths, 20 evidence objects and 10 provenance records | 0.84 ms | < 1 ms |
| `policy/large/evaluate_100k_paths` | 20 rules (8 `[[require]]` with globs, 6 `[[forbid]]`, `[limits]`, 5 `[[rule]]`s each running `any_glob` and an `all` over every file) over 100,000 paths and files and 1,000 evidence objects | 590 ms | < 300 ms |
| `policy/large/evaluate_200_rules_100k_paths` | 200 cheap rules over the same input: the cost bound stops them at the evaluation's 5×10^7 | 0.80 s | none |

- **The large evaluation is 2.0× its expectation**, and most of it is the benchmark's five `[[rule]]`s, each walking 100,000 files in cel-rust's tree-walking interpreter (about 0.14 µs a node). Binding the input as CEL values and as JSON for the cost bound costs about 100 ms at 100,000 files. The declared tables (globs, limits, required evidence) are a few milliseconds of it. Path rules belong in the tables or `any_glob`, as [docs/policy.md](policy.md) says.
- **The 200-rule case shows the bound at work**: the rules that fit under 5×10^7 run, and the rest report `error` without running. The cost bound charges values by their size (the review's first finding), which refuses more of them early than the first measurement's 1.47 s did.
- **Through the daemon**, `tests/policy.rs`'s `evaluate` prints the median of ten `EvaluatePolicy` calls for a one-file change over the socket with a warm cache: 5.9 ms in a debug build (expected under 50 ms in release). It includes resolving both revisions, the diff, reading the tree's evidence with every signature checked again, and the change's provenance records.
- **No existing path changed**, apart from the diff's new `sizes` option, which only `EvaluatePolicy` sets.

```sh
cargo bench -p reef-policy --bench policy
cargo nextest run -p reef-daemon --test policy --success-output immediate -E 'test(evaluate)'
```
