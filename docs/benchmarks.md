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

Each job uploads an artifact named `bench-<os>-<preset>-<commit sha>`, kept for 90 days, holding `bench-results.txt` and criterion's `target/criterion/` directory. Download one with:

```sh
gh run download --name "bench-ubuntu-24.04-medium-$(git rev-parse HEAD)"
```

Open `target/criterion/report/index.html` from the artifact for criterion's HTML reports, one per benchmark under `snapshot/<preset>/<name>/report/`.

GitHub-hosted runners are shared, noisy machines. Read the artifacts for trends across many runs, not for precise numbers or small differences between two runs.

### Reading `bench-results.txt`

It is criterion's `--output-format bencher` output, one line per benchmark:

```text
test snapshot/medium/noop ... bench:  11,203,512 ns/iter (+/- 140,961)
```

The number is criterion's estimate of the median time per iteration in nanoseconds, and the `+/-` figure its estimate of the standard deviation. Both differ from criterion's default output, which prints the slope estimate (these benchmarks use linear sampling) and its confidence interval. Throughput is not included; divide the tracked file count, which the benchmark prints to stderr as `bench fixture: ... N tracked files`, by the `noop` time for files per second. The workflow's "Fixture summary" step prints the manifest's text file, directory, and byte counts; the tracked count adds the binaries, symlinks, and `.gitignore`.

## Baseline

Medium preset (10,014 tracked files, 59 MB), median time per iteration from `bench-results.txt`. The CI columns are the first `bench.yml` run, [38936744617](https://github.com/reef-vcs/reef/actions/runs/38936744617) at commit `1cde75e`, run from the R0.06 branch because `workflow_dispatch` only works once the workflow is on `main`. Runner hardware is GitHub's published spec for public repositories. The local column is a run of the same code for comparison.

| Benchmark | `ubuntu-24.04` (x64, 4 vCPU, 16 GB) | `macos-15` (arm64 M1, 3 vCPU, 7 GB) | Apple M2 Max, 12 cores, 32 GB, macOS 27.0.1, APFS |
| --- | ---: | ---: | ---: |
| `noop` | 16.1 ms | 22.8 ms | 15.9 ms |
| `modify_1` | 47.3 ms | 49.2 ms | 54.2 ms |
| `modify_50` | 65.4 ms | 113.0 ms | 97.4 ms |
| `modify_500` | 210.1 ms | 526.1 ms | 302.8 ms |
| `add_50` | 67.5 ms | 124.1 ms | 104.9 ms |
| `op_log_100` | 0.77 ms | 1.30 ms | 1.71 ms |
| `open` | 0.36 ms | 0.43 ms | 0.52 ms |
| First snapshot of the fixture (stderr, once) | 1.9 s | 5.2 s | 2.8 s |

Every benchmark scales with the tree as well as with the change. A no-op snapshot scans all 10,014 files (about 630,000 files per second on the M2 Max), and even a one-file change costs about 50 ms, mostly in committing the operation and saving the working-copy state, and ADR 0008 found that saving the state is O(tracked files). On the `large` preset (100,014 files) on the M2 Max, `noop` takes 146 ms, `modify_1` 194 ms, and the first snapshot 24.9 s. Those are the costs Phase 1's watcher-driven snapshots have to remove.
