# Fleet benchmark

Reef's users run agent fleets on their own machines. The goal ([#264](dev-tickets/phase-3/issue-264-laptop-resource-budget.md)) is that a standard laptop works on five tickets at once, in a typical repository, without running out of disk and without becoming unusable. The tracked tree costs almost nothing, because workspaces are copy-on-write; what costs is everything a build adds after the create. This benchmark measures that cost, so that the follow-on tickets ([#271](https://github.com/reef-vcs/reef/issues/271) build limit, [#272](https://github.com/reef-vcs/reef/issues/272) disk accounting and floor, [#273](https://github.com/reef-vcs/reef/issues/273) clone stores, [#274](https://github.com/reef-vcs/reef/issues/274) idle eviction) each have a baseline to beat. The decisions are in [ADR 0073](adr/0073-laptop-resource-budget.md).

## The standard machine

8 cores (4 performance, 4 efficiency: a base M1 to M3), 16 GiB of memory, a 512 GB SSD (about 460 GiB usable), with 150 GiB free when the run starts.

The dev machine (an M2 Max: 12 cores, 32 GiB, 926 GiB) overstates all three, so its runs decide nothing. Each run names its profile:

| Profile | Where | Judged |
| --- | --- | --- |
| `standard` | a macOS VM on the dev machine with 8 vCPUs, 16 GiB and a 512 GB sparse APFS disk ([Lume](https://github.com/trycua/cua/tree/main/libs/lume), Apple's Virtualization framework) | yes |
| `linux` | the `ubuntu-24.04` hosted runner (4 vCPUs, 16 GiB), in [`fleet.yml`](../.github/workflows/fleet.yml), with release builds of `reef` and `reef-daemon`; Reef's own row only when dispatched by name | yes |
| `dev` | any other host | no: shown with the verdict `dev` |

Gates are bytes, ratios and pressure states, never raw wall time: the VM's vCPUs run mostly on performance cores and its disk is a file on a faster SSD, so its times are optimistic.

## Repositories

One per stack, pinned by commit in [`repos.toml`](../crates/reef-testing/fleet/repos.toml):

| Name | Stack | Pin | Timed build and tests | Cold, dev host |
| --- | --- | --- | --- | --- |
| `tokio` | Rust | tokio-1.54.1 (with a pinned `Cargo.lock` the harness commits) | `cargo test --workspace --exclude tests-build --features full,test-util` | 86 s, 3.9 GB `target/` |
| `cli` | Go | cli/cli v2.103.3 | `go vet`, `go build`, `go test -race` (as its CI), in the run's own `GOCACHE`, emptied first | 77–90 s, 1.6 GB `GOCACHE` |
| `excalidraw` | TypeScript | v0.18.1 | install from the lockfile, typecheck, lint, Vitest, the app and package builds | 80 s, about 1.3 GB |
| `fastapi` | Python | 0.162.5 | `uv sync` from the lockfile, mypy, pytest with coverage | 162 s, 373 MB |
| `reef` | Rust (reported, not gated) | the run's own commit | `cargo nextest run --workspace` | see the baseline |

Each has five tickets, `patches/<name>/<n>.patch`, each a small change and its test in a different package or module, and a review round per ticket, `<n>b.patch`, applied on top in the same workspace. Every patch applies with `git apply` in a tree with no `.git`, and every ticket's tests pass. Reef's own tickets add a test file to five crates, so they apply at any commit.

`setup` runs once in a fresh clone, untimed: it downloads dependencies and warms the package managers' caches, because downloads are not what is measured. `clean` runs before each scenario and empties shared build caches (`GOCACHE`), which are build output. Shared tool caches (`~/.cargo/registry`, uv's and yarn's) stay shared: Reef never gives a workspace its own `HOME`. `GOCACHE` is shared by the five workspaces too, but is the run's own (`{caches}` in `repos.toml`, `<work-root>/caches`), so that `clean` never empties the user's.

`--repo synthetic` generates a small Cargo workspace (five crates and a probe crate) and its patches, offline. `reef-vcs`'s [`tests/fleet_smoke.rs`](../crates/reef-vcs/tests/fleet_smoke.rs) runs it in `just ci` in about 20 seconds.

## The scenario

For each repository, through the `reef` CLI as an agent uses it, against a daemon with the run's own state directory:

1. **Solo.** One `reef ws run` per ticket, one at a time: create, apply the patch, build and test, release. Ticket 1's run is the "one workspace" every ratio divides by. Each one's test outcomes are the reference for G9.
2. **Five at once.** Five `reef ws run --keep` start within a second (`--stagger-ms`), each with its patch. Each then runs its review round in the same workspace (`reef ws run --workspace`). After each round the harness walks the workspace: allocated and APFS-unshared bytes, build output (`build_dirs`) apart from the rest.
3. **Idle** for 5 minutes (`--idle-secs`), workspaces kept.
4. **Release** each workspace, then `reef ws gc`, then the final free-space reading.

The daemon deletes a released workspace in the background, so before the five start and before the final reading the harness waits until no `.trash-` entry is left in the state directory and free space holds steady. A run asked for `standard` or `linux` on a host that is not that machine is judged as `dev`, and `summary.md` says why.

The harness sets no Reef variable and drops its own build settings (`CARGO_TARGET_DIR`, `CARGO_INCREMENTAL`, `RUSTFLAGS`, `CARGO_PROFILE_*`) so each repository builds with its own defaults plus its `env` in `repos.toml`.

Variants are flags: `--seed off|sibling|sibling-edited|clone` (the build-output spike), `--stores on` (a repository's `reef.toml` stores), and `--limit off|on|both` (the build limit, #271; default `on`). `--limit off` passes `--no-build-limit` to every `ws run`, which reproduces the #264 baseline. `--limit both` runs each repository's scenario twice against one daemon, without the limit and then with it: the report judges the second and compares the two. The first skips its `ws gc`, so the second starts from the same objects. `--solo-runs <n>` runs solo ticket 1 `n` times, and the report takes the median: use 3 for a gated `--limit both` run, and warm the caches with `--setup-only` first so neither half pays the downloads. A sampler reads `reef daemon status`'s `build` every 5 s: the tokens free and the builders.

## What is measured

Sampled every second into `report.json`; `summary.md` renders it.

| Metric | How |
| --- | --- |
| Free space | `statvfs` on the volume holding the workspaces (and the clone's, if different). macOS's "available for important usage" figure, which counts purgeable space such as Time Machine's local snapshots, every 5 seconds |
| Disk per workspace | after each round, the daemon's kept figure from `reef ws inspect` (#272: tracked, build output, unshared), beside the harness's own walk (allocated and unshared bytes, build output apart from the rest); `summary.md` shows both |
| The daemon's disk | `reef daemon status -o json`'s `disk` every 10 seconds (#272): its level, the lowest usable free space and that volume's floor, and what Reef holds (`reef_bytes`) |
| Load | the 1-minute load average; CPU busy share from `/proc/stat` on Linux and summed `ps %cpu` on macOS |
| Memory | macOS: `kern.memorystatus_vm_pressure_level` (normal, warn, critical), swap-outs and compressor bytes from `vm_stat`, swap in use. Linux: PSI `memory some` and `full` `avg10`, swap in use. Each ticket's process-tree RSS from `ps` |
| Time | each run's wall time from `reef ws run` to its exit; the five tickets' makespan |
| Usability | a canary: `reef ws list -o json` every 5 seconds (its p99), and a 10 ms sleep loop (its lateness p99) |
| Correctness | each test's outcome from the log (libtest, `go test`, pytest `-rA`, TAP, Vitest's verbose reporter), compared with the same patch's solo run |
| Refusals | creates `ws run` could not make, and of them those refused with `DISK_SPACE_LOW` (#272); builds whose log shows `ENOSPC` |

The run records Time Machine's local snapshots (`tmutil listlocalsnapshots /`) and Spotlight's state, because a local snapshot keeps a released workspace's blocks for up to a day and `statvfs` does not count them as free.

## Gates

Judged per repository, on the `standard` and `linux` profiles only. `reef-fleet report <dir> --gate` exits 1 when one fails. Until RX.06 turns the gate on, CI runs report-only.

| Gate | Threshold |
| --- | --- |
| G1 disk safety | no create refused (none with `DISK_SPACE_LOW`) and no build failed for disk; free space never below the floor, the larger of 20 GiB and 5% of the volume; the daemon's own level never `low` and its free space never under its floor (#272) |
| G2 disk peak | five tickets consume at most 100 GiB at peak |
| G3 disk scaling | five tickets cost at most 2× one ticket's peak (3× for Rust: linked test binaries are per workspace whatever is shared) |
| G4 disk after | after release and `ws gc`, within 2 GiB of the start beyond what Reef kept on purpose (bases and stores it made during the run). Growth of the shared tool caches (`GOCACHE`, yarn's) is reported apart and not counted (#272, the user's decision 2026-11-13): Reef cannot change what a tool keys its cache by. The daemon's `reef_bytes` at the start and the end is reported beside it |
| G5 load | 1-minute load p95 at most 2× the cores, maximum at most 3× |
| G6 memory | macOS: no time at critical pressure and at most 2 GiB swapped out; Linux: PSI `memory full avg10` never above 10% |
| G7 usability | the canary's `ws list` p99 under 1 s, and none failed or ran past 30 s; the sleep's lateness p99 under 50 ms |
| G8 throughput | the median five-at-once ticket at most 5× its solo time; with `--limit both`, the five tickets' makespan with the limit and the solo ticket's median with it each at most 1.1× without it (`n/a` without `both`) |
| G9 correctness | every ticket's test outcomes equal its solo run's |

## Running it

```sh
# The harness's own test: a generated repository, offline, about 20 s.
cargo nextest run -p reef-vcs --test fleet_smoke

# Clone, set up and check every repository's patches (downloads; untimed).
cargo run -p reef-testing --bin reef-fleet -- run --repo all --setup-only --work-root /tmp/rf

# One repository, on this host (never judged).
cargo run -p reef-testing --bin reef-fleet -- run --repo tokio --work-root /tmp/rf --out ~/fleet/tokio-dev

# Print a run's summary again, judged.
reef-fleet report ~/fleet/tokio-dev --gate
```

`--work-root` holds the run's daemon state and should be short (the daemon's socket lives there); clones are kept in `<work-root>/repos` and reused. `--reef-bin-dir` names the `reef` and `reef-daemon` to measure (default: the `reef` on `PATH`).

### The runbook

1. **Pick the profile.** `standard` in the VM, `linux` on the runner (`gh workflow run fleet.yml`), `dev` on the host for iteration only.
2. **Quiet the machine.** No cargo, nextest or Docker run at the same time; no leaked `ws_run` test processes (`ps -Ao command | grep 'STOP=/tmp/reef-'`); no busy `fseventsd` or `mds`. Never two runs at once.
3. **Check free space** on the host (`df -h /System/Volumes/Data`) and in the VM. The VM's disk is sparse, so its peak lands on the host's volume: with less than 150 GiB free on the host, do not run the Rust repositories there.
4. **Install the toolchains** at the versions in [`crates/reef-testing/README.md`](../crates/reef-testing/README.md), then warm the caches with `--setup-only`.
5. **Run** each variant.
6. **Read** `summary.md`: the gate table first, then each run's times, units compiled and disk, then the disk and pressure figures.
7. **Clean up.** `reef-fleet` releases its workspaces and stops its daemon. Delete the run's work root, `GOCACHE` if it grew, and after the baseline, the VM.

### The standard-machine VM

```sh
brew install lume && lume config telemetry disable
lume pull macos-tahoe-vanilla:latest fleet-standard   # about 30 GB; user `lume`, SSH on
lume set fleet-standard --cpu 8 --memory 16GB --disk-size 512GB
lume run fleet-standard --no-display &
lume ssh fleet-standard                               # then install the toolchains and run
lume delete fleet-standard                            # after the baseline
```

## What a repository can do today

Before the follow-ons land, the spike's largest savings are a repository's own settings, which Reef recommends and never sets (they change what a build produces):

- **Rust**: `CARGO_INCREMENTAL=0` and `[profile.dev] debug = "line-tables-only"` (or `CARGO_PROFILE_DEV_DEBUG`/`CARGO_PROFILE_TEST_DEBUG`). On tokio, five tickets took 6.3 GiB instead of 26, and the median ticket 171 s instead of 267; panics keep file and line.
- **Go**: `-trimpath` (`GOFLAGS=-trimpath`), so workspaces at different paths share `GOCACHE` entries for the repository's own packages. On cli/cli it halved the five tickets' disk and makespan, with the same test results.

## Baseline

Reef 0.4.2 at `26ba7cdf`, no build limit, no seeding, no stores (`--limit off --seed off --stores off`), 5 tickets, 300 s idle.

### Standard (macOS VM, 8 vCPUs, 16 GiB), 2026-11-12

Release `reef` and `reef-daemon` at `26ba7cdf` in a Lume VM (macOS 26.4) on the dev machine. tokio's row is pending: a Rust run would have grown the VM's sparse image by about 40 GB on a host with 97 GiB free, so it follows in a results pull request with a fresh VM. Its Linux row and the dev-host spike (below and in [benchmarks.md](benchmarks.md#build-output-across-workspaces-264)) cover Rust meanwhile.

| Gate | fastapi | excalidraw | cli | tokio |
| --- | --- | --- | --- | --- |
| G1 disk safety | pass | pass | pass | pending |
| G2 disk peak | pass: 0.81 GiB | pass: 5.05 GiB | pass: 6.15 GiB | |
| G3 disk scaling (2×) | **fail: 3.42×** (0.81 / 0.24 GiB) | **fail: 3.49×** (5.05 / 1.45) | **fail: 2.13×** (6.15 / 2.89) | |
| G4 disk after | pass: 0.10 GiB | pass: 0.26 GiB | **fail: 8.20 GiB**, of it `GOCACHE` 7.86 | |
| G5 load (16) | pass: p95 5.5 | **fail: p95 42.0**, max 44.7 | **fail: p95 24.7**, max 28.8 | |
| G6 memory | pass: no pressure, no swap | pass | pass | |
| G7 usability | pass: `ws list` p99 37 ms, late 12.5 ms | pass: 406 ms, 8.0 ms | pass: 113 ms, 11.8 ms | |
| G8 throughput | pass: 1.24× | pass: 2.29× | pass: 3.36× | |
| G9 correctness | pass | **fail: 5 of 5** | pass | |

| | fastapi | excalidraw | cli |
| --- | --- | --- | --- |
| Solo ticket 1 | 147 s | 111 s | 153 s |
| Five at once: median round 1, makespan | 183 s, 331 s | 255 s, 472 s | 515 s, 537 s |
| Largest ticket's peak RSS | 0.96 GiB | 3.14 GiB | 2.32 GiB |

What the baseline says:

- **Load fails first, as expected.** Five TypeScript or Go builds put 3 to 5 runnable threads on every core. The canary's `ws list` and sleep stayed inside G7, so small commands stayed responsive; long ones slowed with everything else (cli's median ticket took 3.4× its solo time). #271's limit is the fix.
- **Load breaks tests.** Five at once, excalidraw's runs hit 35 test timeouts in all (25 at Vitest's 5 s default, 10 at 30 s: the 20,000-index sync tests, the CJK font export), and each ticket failed; alone they pass. A suite with timeouts is a correctness casualty of oversubscription, not only a slowdown.
- **Disk is not the wall on typical repositories** (under 7 GiB for five tickets), but it scales per workspace. Each workspace installs its own `node_modules` (about 1 GB) or `.venv`, so five cost 3.4× one; a cloned `node_modules` (the spike's E6) brings excalidraw to 0.61×. Go's `GOCACHE` keys cli's own packages by the workspace's path and grew 7.9 GiB that outlives the workspaces (G4); `-trimpath` (E5) halves its disk and its makespan.
- **Memory is not the wall at 16 GiB** for these stacks: no pressure, no swap.

The first standard run, on the harness before the review's fixes, read lower peaks (it measured while released workspaces were still being deleted) and passed G3 and G4 it should not have.

### Linux (`ubuntu-24.04`, 4 vCPUs, 16 GiB)

[Run 43302613386](https://github.com/reef-vcs/reef/actions/runs/43302613386), 2026-11-12: release `reef` and `reef-daemon` at this branch's base (`26ba7cdf` plus the harness), ext4, about 108 GiB free after the workflow's cleanup.

| Gate | fastapi | excalidraw | cli | tokio |
| --- | --- | --- | --- | --- |
| G1 disk safety | pass | pass | pass | pass: min free 68 GiB |
| G2 disk peak | pass: 1.77 GiB | pass: 5.76 GiB | pass: 5.50 GiB | pass: 38.8 GiB |
| G3 disk scaling | **fail: 4.11×** | **fail: 3.79×** | **fail: 2.08×** | **fail: 4.90×** (3× for Rust) |
| G4 disk after | pass: 0.12 GiB | pass: 0.33 GiB | **fail: 7.65 GiB**, of it `GOCACHE` 7.37 | pass: 0.14 GiB |
| G5 load (8) | pass: p95 7.8 | **fail: p95 25.7** | **fail: p95 33.4** | **fail: p95 32.0** |
| G6 memory | pass | pass: PSI full max 2.2% | pass | **fail: PSI full max 29.6%**, 0.72 GiB swap |
| G7 usability | pass: 43 ms, 1.8 ms | pass: 142 ms, 3.0 ms | pass: 148 ms, 2.9 ms | pass: 286 ms, 3.0 ms |
| G8 throughput | pass: 1.78× | pass: 2.58× | pass: 2.28× | pass: 4.07× |
| G9 correctness | pass | **fail: 5 of 5** | pass | pass |

| | fastapi | excalidraw | cli | tokio |
| --- | --- | --- | --- | --- |
| Solo ticket 1 | 179 s | 328 s | 385 s | 248 s |
| Five at once: median round 1, makespan | 320 s, 597 s | 848 s, 1643 s | 879 s, 923 s | 1011 s, 1817 s |

On ext4 nothing is cloned: every workspace's `.venv`, `node_modules` and `target/` is a full copy, so G3 fails for every stack, and Rust worst. tokio's five tickets took 38.8 GiB (each workspace compiled all 121 units) and pushed the runner into memory stalls (PSI `full` 29.6%, the first G6 failure): at 4 cores and 16 GiB, five Rust builds are the case #271's job limit (`jobs = min(cores, GiB / 2)`) exists for.

## With disk accounting (#272), 2026-11-13

Release `reef`, `reef-daemon` and `reef-fleet` with #272 and #271's build limit (on), no seeding, no stores, 5 tickets, 300 s idle. G1, G2 and G4 are #272's gates; G1 now also requires the daemon's own level never `low`, and G4 reports the daemon's `reef_bytes` beside the harness's walk. In every round on both machines, the daemon's figure for a workspace (`reef ws inspect`'s unshared bytes) matched the harness's own walk to within 0.01 GiB: on APFS the private bytes (fastapi 0.14 GiB against 0.13 built plus 0.01 other), on ext4 the full copies (tokio 7.69 GiB against 7.69).

### Standard (macOS VM, 8 vCPUs, 16 GiB)

`077e1ccc` in a fresh Lume VM (macOS 26.4, 512 GB sparse disk). The auto floor on its 507 GiB volume is 25 GiB. tokio's row is still pending, for the same reason as the baseline's.

| Gate | fastapi | excalidraw | cli |
| --- | --- | --- | --- |
| G1 disk safety | pass: min free 474.0 GiB, daemon 0 low readings | pass: 468.9 GiB, 0 low | pass: 463.6 GiB, 0 low |
| G2 disk peak | pass: 0.75 GiB | pass: 5.64 GiB | pass: 6.16 GiB |
| G3 disk scaling (2×) | pass: 1.41× (0.75 / 0.53 GiB) | **fail: 3.97×** (5.64 / 1.42) | **fail: 2.15×** (6.16 / 2.86) |
| G4 disk after | pass: 0.35 GiB, daemon's Reef total 0.00 → 0.03 GiB | pass: 0.27 GiB, 0.03 → 0.08 | pass: 8.24 GiB used, of it `GOCACHE` 7.86 (not counted); 0.08 → 0.58 |
| G5 load (16) | pass: p95 7.5 | **fail: p95 37.1** | **fail: p95 35.3** |
| G6 memory | pass | pass | pass |
| G7 usability | **fail: `ws list` p99 2473 ms**, late 20.3 ms | **fail: 4524 ms**, 10.0 ms | **fail: 1050 ms**, 14.1 ms |
| G8 throughput | pass: 1.29× | pass: 1.73× | pass: 2.65× |
| G9 correctness | pass | **fail: 1 of 5** (a 20,000-index sync test) | pass |

G7's failures are 12 of 859 `ws list` calls (median 30 ms). Every call over 500 ms came while the host's self-hosted CI runner was running a job, and the slowest (excalidraw, 4.5 s) during 4 seconds in which the VM's CPUs were idle and the daemon logged nothing, with five installs in flight, then resumed all at once: a stall of the VM's disk, not of the daemon. The same build's `ws list` p99 on Linux is 45 to 48 ms for every stack. A rerun on a quiet host would settle it.

### Linux (`ubuntu-24.04`, 4 vCPUs, 16 GiB)

[Run 43496597183](https://github.com/reef-vcs/reef/actions/runs/43496597183), `e7aea9db` (this pull request's implementation before its rebase), `--limit both`: each stack ran without the limit and then with it, and the gates judge the second.

| Gate | fastapi | excalidraw | cli | tokio |
| --- | --- | --- | --- | --- |
| G1 disk safety | pass: min free 107.1 GiB, 0 low | pass: 98.8 GiB, 0 low | pass: 97.8 GiB, 0 low | pass: 70.4 GiB, 0 low |
| G2 disk peak | pass: 1.79 GiB | pass: 5.75 GiB | pass: 5.48 GiB | pass: 38.71 GiB |
| G3 disk scaling | **fail: 5.26×** | **fail: 5.00×** | **fail: 2.31×** | **fail: 4.97×** (3× for Rust) |
| G4 disk after | pass: 0.01 GiB, daemon 0.03 → 1.74 GiB (its last reading came before the release) | pass: 0.13 GiB, 0.22 → 0.22 | pass: 8.69 GiB used, of it `GOCACHE` 8.67 (not counted); 0.02 → 0.27 | pass: 0.01 GiB, 0.14 → 0.15 |
| G5 load (8) | pass: p95 7.1 | **fail: p95 24.4** | **fail: p95 31.4** | **fail: p95 16.4** |
| G6 memory | pass | pass: PSI full max 1.3% | pass | pass: PSI full max 10.0%, 0.58 GiB swap |
| G7 usability | pass: 45 ms, 2.9 ms | pass: 45 ms, 2.9 ms | pass: 46 ms, 2.9 ms | pass: 48 ms, 3.0 ms |
| G8 throughput | pass: 1.91× | pass: 2.83×; limit on/off makespan 1.01× | pass: 3.69×; 1.03× | pass: 4.74×; 0.96× |
| G9 correctness | pass | **fail: 5 of 5** | pass | pass |

What the runs say for #272:

- **G1, G2 and G4 pass on every stack on both machines**, with no create refused and the daemon never `low`. G4's cli row passes only because the user's decision of 2026-11-13 stops counting the shared `GOCACHE` (7.9 to 8.7 GiB that outlives the workspaces); `-trimpath` remains the fix for that.
- **The daemon's figures agree with an independent walk**, workspace by workspace, on both filesystems.
- The harness read the daemon every 10 s and stopped at the end of `ws gc`, so fastapi's Linux "end" (1.74 GiB, its five workspaces) came before the release. It now takes one more reading after `ws gc`.
- **G3 is unchanged and was not this ticket's to fix**: every workspace still installs its own dependencies (#273 and #274 are next).
