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

