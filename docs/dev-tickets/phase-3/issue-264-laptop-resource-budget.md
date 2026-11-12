# Issue #264: five tickets at once on a standard MacBook

Phase 3 · This ticket: size M at its top end, the issue's steps 1 to 3 plus ADR 0073. Four follow-on tickets build steps 4 to 6 (decision 1) · Fixes: issue #264 (its acceptance: steps 1 to 3 done, an ADR for 4 to 6, and a ticket for each of those that is a go) · Depends on: R1.12 (the workspace bench and its disk rows), R1.20 (stores), R1.21 (`reef ws run`), R1.23 (the load test, `disk_full`), RX.07 (pinned real repositories in a harness) · Builds on: ADR 0015 (backends and the base cache), ADR 0022 (stores), #95 (`Footprint`), R2.26 (the workspace provider) · Interacts with: #260 (the default workspace root moves), #259 (its own API revision), #146 and #195 (folded into the follow-ons) · Unblocks: the four follow-ons, RX.06's gate for this goal

## Context

The goal, set by the user on 2026-11-09: a standard MacBook (16 GB of memory, a 512 GB SSD) works on five tickets at once **in a typical user repository** without running out of disk and without becoming unusable. Benchmarks must include a compiled build (Rust or Go), not only JavaScript. Reef must not assume data-centre resources.

The source tree is not where a laptop's resources go. Workspaces are copy-on-write, so the tracked tree costs almost nothing. The cost is everything a build adds after the create:

- **Build output fills the disk.** In Reef's own repository one workspace's `target/` reached 42 GB (#184), a shared target directory 142–163 GB, and the disk filled at least five times between 2026-10-26 and 2026-11-06 (#224, #225 among them). A full disk breaks everything at once: snapshots, Docker, the daemon's writes (#229 was a real bug found that way).
- **Sharing one target directory by hand is incorrect, not only slow.** Cargo hashes workspace members relative to the workspace root, so a test binary built in one Reef workspace is reused in another with the first one's `env!("CARGO_MANIFEST_DIR")` and `build.rs` output baked in (seen as `crate_graph` failing with "manifest path …/ws/<old id>/… does not exist", and a `reef --version` naming a stale commit). A build in workspace B replaced `target/debug/reef` while A's suite ran, so A's tests ran B's binary. Cargo's build-directory lock also serializes the builds.
- **CPU and memory are the other wall.** Three agents building at once pushed the load average to 40–50 on the 12-core dev machine and made timing tests fail (#241, #251). Every build assumes it owns every core: cargo's `-j`, rustc's codegen threads, nextest's test threads. A 16 GB machine running five such builds swaps long before it runs out of cores.
- **Stores answer half of the dependency question.** R1.20's stores share `node_modules` read-only across workspaces, but this repository declares none (it has no `reef.toml`), nothing points an agent at them (#195), they are read-only (an agent's `npm install <pkg>` or a tool that writes `node_modules/.vite` fails), and they do not cover build output.

The issue orders the work: a benchmark first, as the baseline; stores dogfooded here; a spike on sharing build output; then disk accounting and a budget, idle eviction, and a machine-wide build concurrency limit; then the gate. Its acceptance asks this ticket for steps 1 to 3 and an ADR for 4 to 6, with a ticket for each. This brief keeps that shape (decision 1), and decides now what can be decided for 4 to 6 so that ADR 0073 records it and the follow-on tickets start from settled designs.

### What was verified while writing this ticket

Reef at `0d14bf1e` (#258 merged; API revision 29, `reef-api/src/lib.rs:49`; schema 17, migrations up to `0017_conflict_assignments.sql`; ADR 0070 the latest; threat rows up to S-176). #259 takes API revision 30, ADR 0071 and S-177 to S-184; #260's draft brief takes ADR 0072 and S-185 to S-187, and possibly revision 31. This brief plans **ADR 0073** for this ticket, **API revision 32**, **schema 18** and threat rows from **S-193** for the follow-ons, renumbered on rebase. Paths are under `crates/` unless they start with `docs/`, `web/`, `sdk/` or `.github/`.

- **The machine.** The dev machine is an M2 Max: 12 cores (8 performance, 4 efficiency; `sysctl hw.ncpu hw.perflevel0.physicalcpu hw.perflevel1.physicalcpu`), 32 GiB (`hw.memsize` 34,359,738,368), a 926 GiB data volume with 216 GiB free while this was written (`df -h /System/Volumes/Data`). The standard machine has two-thirds of the cores (8 to 10, of which only 4 are performance cores), half the memory, and just over half the disk. Toolchains here: cargo 1.99.1 (`rust-toolchain.toml` pins 1.99.1), node 22.16.0; **no Go toolchain** and no sccache are installed. The state directory was 373 MB (bases 194 MB), because build caches are cleaned at every ticket start.
- **No free-space check exists.** `fs_util::free_bytes` (`reef-daemon/src/backend/fs_util.rs:114`) is called only by tests and `benches/workspace_create.rs`. Nothing refuses a create, a base materialization or a store build for disk. `tests/disk_full.rs` (R1.23) proves the daemon survives `ENOSPC` and that a snapshot taken after space is freed keeps the edit; #229 fixed a release stuck in `releasing` on a full disk.
- **Disk figures today.** `Backend::disk_usage` returns `DiskUsage { allocated_bytes, files_changed }` (`backend/mod.rs:254`). For clone and copy backends, `changed_files_vs_base` (`fs_util.rs:149`) counts every file whose mtime or size differs from the base's `tree_state`, **ignored files included** ("build output … count"), at full allocated size even where APFS shares the blocks. `Footprint` (`fs_util.rs:39`, #95) already gives APFS's unshared (private) bytes in the same walk; nothing serves it per workspace. `GetWorkspace` walks on request (`skip_disk_usage` turns it off); `ListWorkspaces` carries no disk figure (#146). `ReclaimRecord.freed_bytes` is measured with `allocated_bytes` at reclaim.
- **The proto.** `Workspace` ends at field 22 (`provenance`), `DiskUsage` at 2, `GetDaemonStatusResponse` at 12 (`remote`), `StoreMountMode` at `STORE_MOUNT_MODE_BIND = 2`, `Attention` at `ATTENTION_MOUNT_FAILED = 8`, `OperationKind` at `OPERATION_KIND_PUBLISH = 17`. `ErrorKind` (`reef-error/src/lib.rs:21`) has no resource-exhausted kind.
- **Configuration.** `config.toml` has `[workspace] root` (default `<state>/repos/<repo-id>/ws`), `[base_cache] max_entries` (8 per repository), `[gc]`, `[lease]` (30 min default, 24 h max), `[store] allow_builds`, and no `[disk]` or `[build]` table (`reef-daemon/src/config.rs`). Sizes already parse as bytes or `KiB`/`MiB`/`GiB` (`ByteSize`).
- **`reef.toml` is parsed strictly** (`reef-lib/src/spec.rs`, `deny_unknown_fields`): `[workspace] lease`, `[snapshot] exclude`, `[[store]] name, lockfile, mount, build, env`. A key added later makes an older daemon refuse the file. R3.06 put checks in a new `reef-checks.toml` for that reason.
- **Stores** (`reef-daemon/src/store/`): one directory per `(name, blake3 of the lockfile)`, built by the daemon in a scratch workspace, sealed read-only, mounted as a symlink (or a read-only bind mount on Linux with `CAP_SYS_ADMIN`). A create whose store has no sealed build fails by default (`MissingStores::Fail`, `workspace/mod.rs:221`) with a `next` naming `reef store build`.
- **`reef ws run` starts the command in the CLI's process**, not the daemon's (`reef-vcs/src/cli/ws_run.rs:388`–`:400`), with `REEF_WORKSPACE`, `REEF_WORKSPACE_PATH`, `REEF_REPO`, `REEF_SOCKET` and `REEF_AGENT` (`cli/env.rs`), the provenance variables and `--env`. A workspace held by the provider contract (`reef ws provider create`, R2.26) or created through `reef mcp` runs the agent's commands in the agent's own shell: Reef starts no process there and sets no variable.
- **Where things live.** Bases at `<state>/repos/<id>/bases/<tree>/`, stores at `<state>/stores/<name>/<hash>/`, workspaces at `[workspace] root`. #260's draft moves the default root on macOS to `~/.reef/<repo-short>/<ws-id>` and keeps bases and stores in the state directory, relying on both being on the Data volume (the APFS probe falls back to copy when not). Its out-of-scope list hands Spotlight and Time Machine exclusion to this ticket.
- **Existing benchmarks do not build.** R1.12's `benches/workspace_create.rs` measures create time and disk on synthetic repositories; `tests/load.rs` measures write-to-snapshot latency for 200 workspaces. RX.07's onboarding harness (`reef-testing/src/onboarding/`, `DEFAULT_REPOSITORY` = `BurntSushi/same-file`) is the precedent for cloning a pinned public repository in a harness binary.
- **The executor** (R3.06) runs checks in `~/Library/Caches/reef/executor/jobs/…` with optional per-check caches (`CARGO_TARGET_DIR = "{cache}/target"`, documented as a poisoning risk), outside the daemon's disk view and with no concurrency coordination with workspaces.
- **This repository**: `web/package-lock.json` (buf 1.74.0 through `@bufbuild/buf`) and `sdk/typescript/package-lock.json` (buf 1.75.0), `sdk/python/uv.lock`; `.gitignore` ignores `target/` and `web/node_modules`, and `sdk/typescript/.gitignore` ignores its `/node_modules/`. `just web-check` runs lint, typecheck, Vitest and the build without an install; `just web-install` and `just web-build` run `npm ci` first.

## Decisions

### Size, and the split (decision 1)

The issue as written is XL: a benchmark harness over four real repositories, a spike, store dogfooding, a daemon-kept disk figure and budget, eviction, a jobserver, and the gate, with four API changes. It splits into five tickets, in this order:

| Ticket | What | Size | API | Schema | Threat rows |
| --- | --- | --- | --- | --- | --- |
| **#264** (this brief) | The benchmark (`reef-fleet`) and its baseline on Linux CI and the standard-machine profile; `reef.toml` stores in this repository; the build-output spike; ADR 0073; the four tickets filed | M, top end | none | none | none (no product change) |
| **B: build limit** | A machine-wide jobserver owned by the daemon, job-count variables for tools without one, lower priority for workspace commands; `reef ws run` and the provider contract hand them out | M | 32 | none | S-193 to S-195 |
| **C: disk accounting and floor** | #146's daemon-kept figure (tracked, build output, unshared), a free-space floor that refuses creates, a soft budget, warnings in `reef here` and `ws list` | M, top end | 33 | 18 | S-196 to S-198 |
| **D: clone stores** | A `[[store]] mode = "clone"`: a writable copy-on-write clone of a daemon-built, lockfile-keyed directory (dependency build output) at create | M (L if Linux overlay seeding is in) | 34 | 19 | S-199 to S-200 |
| **E: idle eviction** | Dropping declared build output from idle workspaces, and from the least recently used first over the budget | S | 35 | none (C's table carries its columns) | S-201 to S-202 |

**Order**: #264 first (the issue's own rule: the baseline before any fix). Then B and C, which are independent and can run in parallel. B goes first if only one runs: it is the smaller, it fixes the symptom reported most (load 40–50, timing-test flakes, a machine that stops responding), and the baseline is expected to fail the load and memory gates first on typical repositories (disk fails first only on Reef's own heavy build). D follows the spike's verdict. E needs C's figures and D's declarations.

**This ticket closes #264** when its acceptance holds; B to E are new issues it files, each pointing at ADR 0073 and this brief. #146 closes with C; #195's detection is part of C's warnings (decision 7) and its own issue stays for `reef init`'s message.

### The standard machine, and how the benchmark approximates it (decision 2)

The **standard machine** is 8 cores (4 performance, 4 efficiency: the base M1 to M3; a base M4 has 10, of which 4 performance), 16 GiB of memory, a 512 GB SSD (about 460 GiB usable after the system), with 150 GiB free when the run starts: a laptop whose owner also keeps their own files on it.

The dev machine overstates it in all three dimensions: 1.5× the cores (2× the performance cores), 2× the memory, and 1.8× the disk. Measured there unchanged, five Rust builds that would swap a 16 GB machine run comfortably. The brief accounts for that in three ways:

1. **The pass verdict comes only from runs with the standard machine's limits**: a macOS virtual machine on the dev machine with 8 vCPUs, 16 GiB and a sparse 512 GB APFS disk (Apple's Virtualization framework; the tool, such as `tart`, is chosen at implementation after checking its license), and on Linux the `ubuntu-24.04` hosted runner (4 vCPUs and 16 GiB: stricter on cores, equal on memory). Memory and core caps are real there; APFS clones behave as on a laptop.
2. **Gates are machine-relative or absolute, never raw wall time**: load is divided by the core count, memory is judged by the OS's pressure signals, disk is in bytes against the standard machine's 150 GiB, and wall time is gated only as ratios (five tickets against one, limited against unlimited). The VM's vCPUs run mostly on performance cores, so its wall times are optimistic by roughly the efficiency-core share; the report says so and gates nothing on them.
3. **Runs on the dev machine's host are labelled `dev` and never decide a pass.** They are for iterating quickly; the report prints the host's cores and memory beside every number.

### The benchmark (decision 3)

A new harness binary, `reef-fleet` (`reef-testing/src/fleet/`, `src/bin/fleet.rs`), in the manner of RX.07's `reef-onboarding`. It drives the installed `reef` CLI and a daemon with its own state directory, never the user's.

**Repositories.** One per stack, pinned by commit in `reef-testing/fleet/repos.toml`, chosen at implementation against these criteria: public, permissive license, builds and tests offline once fetched, a cold solo build and test of 3 to 10 minutes on the standard profile, at least five packages or crates so five tickets touch different ones, and a lockfile.

| Stack | Candidate (fallback) | Setup (not timed) | Build and test (timed) |
| --- | --- | --- | --- |
| Rust | `tokio-rs/tokio` at a release tag (`BurntSushi/ripgrep`) | `cargo fetch --locked` | `cargo test --workspace --locked --offline` (as the project's CI runs it, narrowed if over 10 min) |
| Go | `cli/cli` at a release tag (another mid-size, pure-Go CLI) | `go mod download` | `go build ./... && go test ./...` |
| TypeScript | `excalidraw/excalidraw` at a tag (a Vite application with a yarn or npm lockfile) | the package manager's cache warmed | install from the lockfile, typecheck, unit tests, production build |
| Python | `fastapi/fastapi` (`pallets/flask`) | `uv` cache warmed | `uv sync --frozen && uv run pytest -q` |
| Reef (reported, not gated) | this repository at the run's commit | `cargo fetch` | `cargo nextest run --workspace` with the repository's usual variables |

Downloads are not what is measured, so registries and package caches are warmed in setup; build caches are not (`GOCACHE` is emptied at the start, since it is build output). Go 1.2x is installed on the runner and the VM.

**Tickets.** Each repository has five patches (`reef-testing/fleet/patches/<repo>/1..5.patch`), each a realistic small ticket in a different package: a function changed and its test updated. A second, smaller patch per ticket (`<n>b.patch`) is the review round. Patches apply with `git apply` from outside any git repository (a workspace has no `.git`).

**The scenario**, per repository:

1. **Solo**: one `reef ws run` creates a workspace, applies patch 1, runs the build and test, and releases. This is "one workspace" for every ratio.
2. **Five at once**: five `reef ws run` start within one second, each with its own patch, run the build and test, apply their `b` patch and run again (the incremental round), stay idle for 5 minutes with their workspaces kept (`--keep`), then release.
3. **After**: `reef ws gc`, then the final free-space reading.

Variants are flags, so one harness measures the baseline and every follow-on: `--limit on|off` (B), `--seed off|clone|sibling` (the spike, then D), `--stores on|off`, `--profile standard|dev`.

**Metrics**, sampled every second by the harness into `report.json`, with `summary.md` beside it:

| Metric | How | Per |
| --- | --- | --- |
| Disk consumed, peak and after release | `statvfs` free bytes on the volume holding the workspace root (and on the state directory's, if different), against the start; also the macOS "available for important usage" figure, which counts purgeable space | run |
| Disk per workspace | `reef ws inspect`'s walk at the end of each round: allocated and APFS-unshared bytes, tracked and ignored apart; `du` of the external caches (`GOCACHE`, the cargo registry, the package manager's cache) | workspace |
| Load | `getloadavg` 1-minute, peak and p95; CPU busy share | run |
| Memory | macOS: `kern.memorystatus_vm_pressure_level` time at warn and critical, swap-outs and compressor size from `vm_stat`; Linux: PSI `memory` `some` and `full` `avg10`, swap used. Per-ticket peak RSS of the command's process tree, from a `ps` sample each second | run, ticket |
| Time | create to first passing build; the incremental round; the makespan of all five | ticket, run |
| Usability | a canary: `reef ws list -o json` every 5 s (latency p99) and a 10 ms sleep loop (lateness p99) | run |
| Correctness | each ticket's test outcome set equals the one the same patch gets in a cold solo workspace | ticket |
| Refusals | creates refused, builds failed with `ENOSPC` | run |

The run records the Time Machine local snapshots present (`tmutil listlocalsnapshots /`) and whether Spotlight indexing is on for the volume, because a local snapshot keeps a released workspace's blocks for up to a day and `statvfs` does not count them as free.

### Pass thresholds (decision 4)

Proposed now; this ticket records the baseline against them and may move a number only with the user's agreement, written into ADR 0073 and `docs/fleet-benchmark.md`. Every gate is judged on the standard profile.

| Gate | Threshold |
| --- | --- |
| G1 disk safety | no create refused and no build failed for disk; free space never below the floor (decision 7) |
| G2 disk peak, absolute | five tickets consume at most 100 GiB at peak, so a machine with 150 GiB free keeps the floor and some margin |
| G3 disk scaling | five tickets cost at most **2×** one ticket's peak for Go, TypeScript and Python (the issue's number), and at most **3×** for Rust (see "Flagged" below) |
| G4 disk after | after release and `reef ws gc`, within 2 GiB of the start plus the caches Reef keeps on purpose (bases, stores, seeds), each reported |
| G5 load | 1-minute load p95 at most 2× the core count (the issue's number), maximum at most 3× |
| G6 memory | no time at critical pressure; swap-outs at most 2 GiB over the run (Linux: `memory full avg10` never above 10%) |
| G7 usability | the canary's `ws list` p99 under 1 s; the sleep loop's lateness p99 under 50 ms |
| G8 throughput | the five-ticket makespan with the limit at most 1.1× the makespan without it; the median ticket at most 5× its solo time |
| G9 correctness | every ticket's test outcomes equal its cold solo run's |

**Flagged in the issue**: "five concurrent workspaces cost at most twice one workspace's build output" is unlikely to hold for Rust. Linked binaries (every test executable statically links its dependencies) are rebuilt per workspace whatever is shared, and in a test-heavy workspace they are most of `target/`. G3 keeps the issue's 2× for the other stacks and proposes 3× for Rust, to confirm against the spike's divergence numbers (experiment E8).

The baseline is expected to fail G5 and G6 on the Rust and Go repositories and to pass G1 and G2 on the typical repositories (Reef's own row would fail G2). CI runs the harness report-only until the last follow-on ticket turns the gate on (RX.06).

### Stores in this repository (decision 12)

**Deferred** by the user on 2026-11-11: the stores below are tried in a scratch workspace (never committed), and each failure is filed as an issue. No `reef.toml` lands in this ticket, so no other agent's workspace changes under it. The design is kept for when D lands.

A `reef.toml` at the root:

```toml
[[store]]
name = "web-node-modules"
lockfile = "web/package-lock.json"
mount = "web/node_modules"
build = ["npm", "ci", "--prefix", "web", "--ignore-scripts"]

[[store]]
name = "sdk-ts-node-modules"
lockfile = "sdk/typescript/package-lock.json"
mount = "sdk/typescript/node_modules"
build = ["npm", "ci", "--prefix", "sdk/typescript", "--ignore-scripts"]
```

`--ignore-scripts` is checked first: if `@bufbuild/buf` needs its install script to fetch the binary, drop the flag for that store and note why. The implementer runs `just web-check`, the web build, `buf generate` from both packages and the TypeScript SDK's tests in a fresh workspace, with no `npm ci`, and **files every failure as an issue** rather than fixing it here, unless the fix is in `reef.toml` itself. Expected friction, to confirm: Vite writes `node_modules/.vite` (read-only in a store), the two packages pin different buf versions, a Dependabot lockfile bump makes every create fail until someone runs `reef store build` (the default `MissingStores::Fail`), a workspace that edits a lockfile gets `ATTENTION_STORE_HASH_MISMATCH`, and `just web-install` and `just web-build` run `npm ci`, which meets a read-only symlinked `node_modules` (the recipes may need to skip the install when a store is mounted). The Python SDK needs no store: `uv` clones from its own cache on APFS (measured in E7). The PR records the times and disk with and without the stores in the benchmark's Reef row.

This affects every workspace of this repository after merge, including other agents' tickets; the PR description says so, and the brief's verification builds both stores first.

### Shared build output: the options (decisions 5 and 6)

| Option | Shares | Disk | Correct? | Verdict |
| --- | --- | --- | --- | --- |
| One shared `CARGO_TARGET_DIR` for every workspace | everything, in theory | one copy, growing with every variant | **No.** Members hash relative to the workspace root, so artifacts carry another workspace's `CARGO_MANIFEST_DIR` and `build.rs` output; a build replaces binaries under a running suite; cargo's lock serializes builds | rejected |
| A compiler cache (sccache style, `RUSTC_WRAPPER`) | compiled dependency crates, by content hash | none saved: outputs are copied into each target directory, plus the cache | yes (keys include `CARGO_*` variables, so members miss across paths) | a repository's own choice; Reef neither installs nor requires it |
| The tools' own shared caches (`GOCACHE`, `uv`'s cache, the pnpm store, the npm cache) | what each tool keys by content | already shared per user | yes, by the tools' design | Reef keeps them shared (never sets a per-workspace `HOME` or cache variable) and documents them |
| Clone a warm **sibling's** build directory at create | everything the sibling built | blocks shared until rewritten | **No, without more.** Cargo and make judge freshness by mtime: Reef's workspaces keep the base's old mtimes, so a sibling's output built from *edited* sources looks newer than the new workspace's unedited files and is reused; and it carries the sibling's paths. One agent's workspace also becomes the input of another's build (poisoning, as the executor's cache warning says) | spike only (E1, E2) |
| Clone a **daemon-built, lockfile-keyed** directory at create (a clone store) | dependency build output | blocks shared until rewritten; members private | yes, if the store's build leaves no workspace-member output (the repository's command says how) | **recommended**, subject to the spike (E3) |
| A stable path inside every workspace (each sees itself at one path) | member output too | one copy | yes | rejected: needs a mount namespace (Linux only, and restricted on hosted runners), and nothing on macOS without privileges; measured once on Linux (E9) for the record |

**Recommended (decision 5): clone stores, owned by Reef, filled by the repository's command.** A `[[store]]` gains `mode = "clone"` and `key = [...]` (extra files hashed into the store's key, such as `rust-toolchain.toml`). At create the daemon clones the sealed directory into the workspace's mount path (APFS `clonefile` of the directory; `FICLONE` per file on btrfs and XFS; on overlayfs and the copy backend, seeding is skipped with a note, because a copy costs what a cold build saves in disk). The clone is the workspace's own: writable, measured as build output, evicted by E. The store is still built once per key by the daemon in a scratch workspace, under the build limit (B), sealed, and collected by GC, as R1.20 does.

- **Reef owns**: the declaration's parsing, the key, building it (under B's limit and the floor), sealing, cloning per backend, accounting (C), eviction (E), GC, and the hint when a repository has no declaration (C's warning, from #195's detection).
- **The repository configures**: which directory, which files key it, and the command that fills it. For Rust the documented recipe is a full `cargo build --workspace --all-targets --locked` followed by `cargo clean -p <member>` for each workspace member (from `cargo metadata --no-deps`), leaving only registry and git dependencies, whose sources live at fixed paths in `CARGO_HOME` and so stay fresh in every workspace. For JavaScript, a clone store is a writable `node_modules` (fixing Vite's cache directory and `npm install <pkg>` in a workspace). Go and Python need none.
- **Not automatic (decision 6).** Reef never runs a repository's build without a declaration: a daemon running `build.rs` scripts unprompted changes the trust posture S-03 describes. C's warning prints a ready `reef.toml` block in `next` for a detected `Cargo.lock` or lockfile.

The spike decides whether D is a go: E3 must show registry dependencies fresh, members rebuilt, G9's outcomes equal, and a disk saving worth an M ticket. If cargo rebuilds the cloned dependencies anyway (a fingerprint that includes the target directory's path, for example), D narrows to JavaScript's writable `node_modules`, and the ADR says so.

**The spike's experiments**, run through `reef-fleet --seed` modes with `cp -c` (APFS) or `cp --reflink=always` (Linux), no daemon change:

| | Experiment | Measures |
| --- | --- | --- |
| E1 | Rust: clone a sibling's `target/` built on the same base, sibling unedited | which units rebuild (`cargo build -v`, fingerprint log reasons), and whether a probe test reading `env!("CARGO_MANIFEST_DIR")` sees the sibling's path |
| E2 | Rust: clone a sibling's `target/` after the sibling edited a crate | whether the new workspace's unedited crate is wrongly reported fresh (a probe whose outcome the edit changes) |
| E3 | Rust: clone a dependency-only `target/` (members cleaned) | dependency freshness, member rebuild time, unshared bytes after the build, G9 |
| E4 | Rust: E3 with `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=line-tables-only` as the repository's choice | how much of `target/` those two settings remove (Reef's own Linux volume fell from 105 to 22 GB with them) |
| E5 | Go: five workspaces on one `GOCACHE`, with and without `-trimpath` | cache hits for main-module packages across workspace paths; what `-trimpath` breaks (`runtime.Caller` paths in tests) |
| E6 | TypeScript: `npm ci` per workspace; a symlink store; a clone of a warm `node_modules`; `.vite` and `.next/cache` cloned | time, disk, and what fails read-only |
| E7 | Python: a cloned `.venv` (absolute shebangs) against `uv sync` from `uv`'s cache | whether the clone runs; time and disk |
| E8 | Divergence: unshared bytes per workspace after each patch round, per stack | the basis for G3's Rust number |
| E9 | Linux only: one fixed path through `unshare -Urm` and a bind mount | member reuse at one path; recorded, not built |

### Disk: what is counted, the floor and the budget (decision 7, built by C)

**Counted**, per workspace, by one daemon-kept walk (#146), never following symlinks, skipping store mounts:

- `tracked_bytes`: files that differ from the base and are tracked (today's figure, without ignored files);
- `build_bytes`: ignored files (build output), and of those the bytes under declared build directories (clone stores and `[build] dirs`, decision 8), which are the evictable part;
- `unshared_bytes`: APFS's private bytes of the whole workspace, which releasing it would free at once (`Footprint`); `None` off APFS, where `allocated_bytes` stands in;
- `files` and `measured_at`.

Machine-wide: bases, stores and seeds, the state directory's own files (database, artifacts, evidence), the sum of workspaces, and the free space of each volume involved (the state directory's and the workspace root's, which #260 separates).

**Measured** one walk at a time on the blocking pool at low priority: after a snapshot settles (at most every 5 minutes per workspace), every 15 minutes while a workspace is active, on `GetWorkspace` as today, and before any eviction decision. A walk stops at 2 million entries or 30 s and records `incomplete`. Figures are kept in a table (schema 18, `workspace_disk`, keyed by repository and workspace, with E's eviction columns from the start) and served in `ListWorkspaces` with `measured_at`, which is #146's staleness contract.

**The floor** is the hard limit: free space below it refuses `CreateWorkspace` (including its base materialization) and store builds, with reason `DISK_SPACE_LOW` (kind `Conflict`, gRPC `FAILED_PRECONDITION`, exit 5: retrying is useless until space is freed). **It never refuses a snapshot, a release or a GC**: work already written must be saved, and releasing is how space comes back. Default `floor = "auto"`: the larger of 20 GiB and 5% of the volume (24 GiB on a 512 GB disk). On macOS the free figure is the volume's "available for important usage" capacity, which counts purgeable space (Time Machine's local snapshots), because macOS frees that itself under pressure and `statvfs` alone would refuse creates the system could serve. The refusal's metadata gives `free_bytes`, `floor_bytes` and `largest_idle` (workspace, owner, `build_bytes`, idle time); its hint says that releasing takes a final snapshot first, so nothing is lost; its `next` is `reef ws release <largest idle>` when one is idle for more than E's minimum, else `reef ws gc`.

**The budget** is soft: Reef's own total (workspaces' unshared bytes, bases, stores, seeds, state). Default `budget = "auto"`: 25% of the volume (about 120 GiB on a 512 GB disk). Over the budget, or with free space under twice the floor, Reef warns and E evicts idle build output, least recently used first. The budget never refuses anything by itself.

**Agents see it** as data, without asking: `reef here` and `ws list` (all three outputs) add one `disk` object only when the level is not `ok` (so the token benchmark's `ws list` stays as measured), with `level` (`warn`, `low`), the figures, a `hint` and a `next`; `reef daemon status` always shows it; `ws inspect` shows the workspace's figures. When a repository has a detectable dependency layout and no store (#195's list: a lockfile with no `[[store]]`, a `Cargo.lock` with no clone store), `reef here` adds the matching `reef.toml` block as its hint, once per workspace.

### Idle eviction (decision 8, built by E)

A workspace is a cache (design principle 5). E drops a workspace's **declared** build directories and keeps its change, its directory, its lease and its tracked files. Declared means a clone store's mount path, or a path in a new `[build] dirs` list in `reef.toml`, and in either case a path the tree's `.gitignore` ignores and that is not a symlink or a store mount. Undeclared ignored files (a `.env`, a local database, an agent's notes) are never deleted.

- **Idle** means: no `reef ws run` command running in it (its `ACTIVITY` presence entry), no watcher event under the workspace, ignored paths included, and no snapshot, for `evict_idle_after` (default 2 hours). A held workspace whose agent has stopped is idle; one whose build is running is not.
- **Over the budget** or under twice the floor, idle workspaces are evicted least recently active first, down to 30 minutes idle, until the level is back to `ok`.
- **How**: each directory is renamed to the existing `*.trash-<nanos>` name and deleted in the background, so a build never sees a half-deleted directory. The next build is warm from the clone store (re-cloned at the first `reef ws run` or `reef here` in the workspace, or by `reef ws seed`) or cold.
- **Reported**: on the workspace (`build_output`: `present` or `evicted`, with when, why and bytes freed), in `ws inspect`, and in `reef here`'s hint ("build output was removed after 2 h idle; the next build re-seeds"). It is not an operation: jj's view does not change. It is logged and counted in `reef daemon status`.
- `[disk] evict = false` turns it off; `reef ws run --keep-build-output` and the provider's `create` can mark a workspace exempt.

### Machine-wide build concurrency (decision 9, built by B)

**A jobserver the daemon owns, plus job counts for tools without one, handed to commands through their environment.**

- **The jobserver.** The daemon creates a named FIFO (`<state>/build/jobserver-<generation>`, mode 0600 in the 0700 state directory) holding `jobs` tokens, in GNU make's protocol. Cargo takes a jobserver from its environment (the `jobserver` crate reads `CARGO_MAKEFLAGS`, then `MAKEFLAGS`) and passes it to rustc and to `cc`-crate builds, so every Rust build on the machine draws from one pool: a build alone gets every token, five share them, and the share adjusts as builds start and end. Reef sets **`CARGO_MAKEFLAGS=-j --jobserver-auth=fifo:<path>`**, not `MAKEFLAGS`: macOS's `/usr/bin/make` is 3.81 and does not know the `fifo:` form. `MAKEFLAGS` is set only when the repository opts in (`[build] make_jobserver = true`). To verify at implementation: that cargo 1.99 honours a `fifo:` jobserver from `CARGO_MAKEFLAGS` and draws its rustc and build-script tokens from it.
- **Job counts for the rest**, computed once when the command starts: `share = max(2, jobs / builders)`, where `builders` is the number of active workspaces with a running `ws run` command or a live holder, at least 1. Reef sets `RUST_TEST_THREADS`, `NEXTEST_TEST_THREADS` and `GOFLAGS=-p=<share>` (appended to an existing `GOFLAGS` only if it has no `-p`), and any variable the repository declares in `[build] jobs_env` (names matching `^[A-Z][A-Z0-9_]{0,63}$`, not `PATH`, `HOME`, `REEF_*`, `LD_*`, `DYLD_*` or a variable a tool executes; values a template of `{share}` or `{jobs}` and digits). **The share is fixed at start**, because an environment cannot change under a running process; the jobserver is the part that adjusts. A variable already in the command's environment is never overridden.
- **Priority.** Commands started by `ws run` run at `nice` 10 (`[build] nice`), so the person's own terminal and editor stay responsive while agents build. Background QoS is not used: it confines work to efficiency cores and throttles I/O, which costs far more throughput.
- **Default `jobs = "auto"`**: the smaller of the logical cores and memory in GiB ÷ 2 (a rustc or a link of a mid-size crate peaks at 1 to 2 GiB). 8 on the standard machine (16 GiB), 12 on the dev machine. The baseline calibrates the divisor.
- **Degrading.** No daemon, or a remote daemon: `ws run` sets the job counts from the local core count and no jobserver. A tool that ignores the variables runs as before. A token that a killed process held is lost: the daemon starts a new generation (a new FIFO with a full count) whenever no `ws run` command and no holder has been live for a minute, and `reef daemon status` shows the generation, tokens and builders. `reef ws run --no-build-limit` and `[build] limit = false` opt out.
- **Reaching commands Reef does not start** (decision 10). Through `ws run` (the environment), through the provider contract (`create` and `attach` print an `env` object the adapter applies to the session; an addition to contract 1, which ignores unknown keys), and, for cargo, which is where the cores go, through a `.cargo/config.toml` the daemon writes in the parent of the workspaces (`[build] jobs = <share>`, rewritten when the number of active workspaces changes, only under a default root Reef owns), which cargo reads from any directory beneath it and which a repository's own `.cargo/config.toml` overrides. Under `ws run`, `CARGO_BUILD_JOBS` is set to `jobs`, which overrides the file, so the jobserver and not the fixed share governs there. An MCP client cannot set its shell's environment, so it relies on the cargo file and on the adapter. Agents learn nothing new in any of the three.
- **The executor and store builds** draw from the same pool: store builds in the daemon from B; the executor (R3.06) needs the FIFO passed into its sandbox, a follow-up.

**Not chosen**: a fixed share only (a lone build gets a fifth of the machine); a semaphore on `ws run` commands (an agent harness is often itself the `ws run` command and would hold a slot for hours); cgroups (Linux only, and `systemd-run --user` is not on every Linux machine; a follow-up for real enforcement there).

### The API and numbers (decision 11)

This ticket changes no API, CLI or schema. ADR 0073 fixes the follow-ons' surfaces so their briefs start from them:

- **B (revision 32)**: `WorkspaceService.GetBuildEnvironment` (READ; workspace → `env` map, `jobs`, `share`, `builders`, `jobserver` path or why none, `generation`); `GetDaemonStatusResponse.build` (field 13); `reef ws run --no-build-limit`; the provider's `env` key; `[build]` in `config.toml` (`limit`, `jobs`, `nice`, `make_jobserver`, `cargo_config`); `[build] jobs_env` in `reef.toml`. MCP: the RPC is in `NOT_TOOLS` (a tool result cannot set a shell's environment). Threat rows: S-193 repository-declared environment (names and values restricted as above), S-194 jobserver exhaustion by a same-uid process (one trust domain; generations; opt-out), S-195 the cargo file written outside the tree (only under a Reef-owned default root; documented).
- **C (revision 33, schema 18)**: `Workspace.disk` (`WorkspaceDisk`, field 23: `tracked_bytes`, `build_bytes`, `evictable_bytes`, `unshared_bytes`, `files`, `measured_at`, `incomplete`); `GetDaemonStatusResponse.disk` (`DiskStatus`: per volume free, floor, budget, Reef's total by kind, `level`); reason `DISK_SPACE_LOW` on `CreateWorkspace` and `BuildStore`; `[disk]` (`floor`, `budget`, `measure_interval`); `reef here`, `ws list`, `ws inspect`, `daemon status` and their `reef schema` rows; the web UI's Disk column reads the daemon's figure (closing R2.20's open question). Threat rows: S-196 a walk made expensive by a huge tree (caps, `incomplete`), S-197 one agent filling the disk to refuse another's creates (documented; snapshots never refused), S-198 the free-space figure (purgeable space counted on macOS).
- **D (revision 34, schema 19)**: `[[store]] mode` and `key`; `STORE_MOUNT_MODE_CLONE = 3`; the `stores` row gains its mode; `reef store list` shows it; `CreateWorkspaceRequest.skip_seed`. Threat rows: S-199 a clone store's contents running in every workspace (the same as S-03's store builds: repository content at a commit, built by the daemon, never an agent's workspace), S-200 a sealed store changed through a clone (clones are separate files; the store keeps R1.20's modes).
- **E (revision 35)**: `Workspace.build_output` (`BuildOutput`); `reef ws seed` and `reef ws evict` (WRITE, remote-rules rows); `[disk] evict`, `evict_idle_after`; `[build] dirs` in `reef.toml`. Threat rows: S-201 eviction deleting what is not build output (declared and ignored paths only, renamed without following symlinks, never a store mount), S-202 eviction during a build (idleness excludes a running command and recent writes).

`reef.toml` stays one file: `[build]` and the store keys are added to it, and an older daemon refuses a newer file with its parse error. A second file (as R3.06's `reef-checks.toml`) was considered and rejected: the store and build declarations are one concern, and the parse error names the key.

### Interactions with #259 and #260

- **#260** moves the default workspace root to `~/.reef/<repo-short>/` and keeps bases and stores in the state directory. Clone stores and the base cache must stay on the workspace root's volume: D clones only when `same_volume(store, root)` holds (the existing probe's rule) and otherwise skips with a note. C measures free space on both volumes and takes the lower level. B's cargo file goes in `~/.reef/<repo-short>/.cargo/config.toml` under #260's default (a directory Reef owns), and nowhere under a configured root. #260 handed Spotlight and Time Machine exclusion to this ticket: the spike measures `mds` CPU during the five-ticket run, and the ADR records whether C marks the root excluded (Time Machine's sticky exclusion attribute needs no privilege; a `.noindex` name or `.metadata_never_index` for Spotlight, to verify on macOS 27). A Time Machine exclusion does not stop local snapshots holding freed blocks, which is why the floor counts purgeable space.
- **#259** (`reef rebase`, landed-commit snapshots) touches no file this ticket touches. A rebased workspace's tracked files change mtimes, which may invalidate cloned build output: E3 is run once more after a rebase, and the spike notes the result.
- Both take API revisions and ADR numbers before this ticket's follow-ons; the follow-ons renumber at their briefs.

## Deliverables

### `crates/reef-testing/`

- `src/fleet/` (new): `mod.rs` (the scenario), `repos.rs` (`repos.toml`, clone and pin, setup), `sample.rs` (free space per volume, macOS important-usage capacity, `getloadavg`, CPU, memory pressure and swap on macOS, PSI on Linux, process-tree RSS, the canary), `seed.rs` (the spike's `--seed` modes), `report.rs` (`report.json`, `summary.md`, the gate table with `--gate` exiting non-zero on a miss). `src/bin/fleet.rs` (`reef-fleet run|report`), with `--repo`, `--profile`, `--limit`, `--seed`, `--stores`, `--state-dir`, `--out`, `--keep-caches`.
- `fleet/repos.toml` and `fleet/patches/<repo>/{1..5,1b..5b}.patch`.
- A fixture crate for E1 and E2's probes (a test reading `env!("CARGO_MANIFEST_DIR")`, and one whose outcome a patch changes).

### Repository root

- None: `reef.toml` is deferred (deferred by the user, 2026-11-11: tried in a scratch workspace and its friction filed as issues; no `reef.toml` lands in this ticket).

### CI

- `.github/workflows/fleet.yml`: `workflow_dispatch` and weekly, `ubuntu-24.04`; frees the runner's preinstalled SDKs to reach at least 60 GiB free; installs Go and `uv`; runs every stack, report-only; uploads the report. A macOS job is not added (the hosted `macos-15` runner has 3 vCPUs, 7 GiB and about 14 GiB free, below the standard machine in every dimension).

### Documentation

- **`docs/fleet-benchmark.md`** (new): the standard machine, the profiles and how to run each (the VM, the host, Linux CI, Docker with `--cpus 8 --memory 16g`), the repositories and patches, the scenario, every metric and how it is read, the gates, the baseline tables (Linux CI and the VM; `dev` rows apart), and the runbook below.
- **`docs/benchmarks.md`**: a section "Build output across workspaces (#264)" with E1 to E9's numbers and verdicts.
- **`docs/adr/0073-laptop-resource-budget.md`** (accepted), indexed:
  - **Decisions**: the standard machine and its profiles; the gates; clone stores as the way build output is shared, Reef owning keying, building, cloning, accounting and eviction and the repository owning the command; no automatic builds; the disk figure, floor (never refusing a snapshot) and soft budget; eviction of declared, ignored directories only; the daemon's jobserver through `CARGO_MAKEFLAGS`, job counts fixed at start, `nice`, and the three ways they reach commands; the follow-ons' surfaces and numbers; Spotlight and Time Machine.
  - **Alternatives**: a shared target directory; sccache by default; seeding from siblings; a stable path; automatic builds from detection; a hard budget; `statvfs` alone; evicting every ignored directory; a fixed share; a `ws run` semaphore; cgroups; `MAKEFLAGS`; background QoS; a separate declarations file.
- **`docs/dev-tickets/phase-3/README.md`**: the follow-ons' rows when filed. **`CHANGELOG.md`** Unreleased: Added (`reef.toml` stores in this repository; the fleet benchmark).
- **`CONTRIBUTING.md`**: a line that this repository's workspaces now mount the two stores, and what to run when a lockfile changes (`reef store build <name>`).

### GitHub

- The four follow-on issues (B, C, D or its narrowed form, E), each linking ADR 0073 and this brief, with sizes and planned numbers; an issue per friction found in decision 12. Closing #146 is C's.

## Tests

1. `fleet::sample` unit tests: parsing `vm_stat`, `sysctl` and `/proc/pressure/*` fixtures; the process-tree RSS sum on a canned `ps` listing; the canary's lateness computation.
2. `fleet::report`: a canned `report.json` renders the gate table; `--gate` exits 1 on each gate's miss and 0 when all pass; `dev` profile rows never pass or fail a gate.
3. `fleet::repos`: `repos.toml` parses; every patch applies to its pinned commit (a network test, skipped with a note when offline, run in `fleet.yml`).
4. `fleet` smoke (`--repo synthetic`, offline): a tiny generated Cargo workspace and a tiny npm package run the whole scenario against a scratch daemon in under a minute, so the harness itself is tested in `just ci`.
5. (Deferred with decision 12: no repository `reef.toml` to test.)

## Implementation notes

- **Order**: the harness against the synthetic smoke repository (tests 1 to 4); the four pinned repositories; the baseline on Linux CI, then on the VM; `reef.toml` and its friction; the spike E1 to E9 on the VM (E9 in Docker); the ADR with the numbers; the follow-on issues.
- **Never measure while anything else builds** on the host: no cargo, nextest or Docker run at the same time (the load-test runbook's rule). Before a run, check for leaked `ws_run` test processes (`ps -Ao command | grep 'STOP=/tmp/reef-'`) and a busy `fseventsd` or `mds`.
- **Disk on the host**: the VM's disk is sparse, so its peak lands on the host's volume. Check `df -h /System/Volumes/Data` first; with less than 150 GiB free, ask before running the Rust stack (the 50 GiB rule plus the run's 100 GiB). Delete the VM's disk image after each baseline.
- **Builds** of Reef itself use `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=line-tables-only`, as every ticket does; the benchmark repositories use their own defaults (E4 measures the difference).
- **The harness runs `reef` as an agent would**: `reef ws run` for every ticket, the patches applied inside it, no Reef-specific variables set by the harness (so B's later variables are measured, not the harness's).
- **Go** must be installed on the VM and the runner; the README of `reef-testing` says which version.

## Performance expectations

The harness itself costs less than 1% of a core while sampling. Full-matrix durations, to record in the runbook: about 30 minutes per stack on the VM (solo, five at once, two rounds, idle, release), so 2 to 3 hours for the four gated stacks and one variant; the Linux runner is slower (4 vCPUs), within `fleet.yml`'s 6-hour job limit by running stacks as matrix jobs.

## Acceptance criteria

- [ ] `reef-fleet` runs the scenario on four pinned repositories (Rust, Go, TypeScript, Python) and Reef's own, records every metric in decision 3, and evaluates the gates of decision 4 (tests 1 to 4).
- [ ] The baseline is recorded on Linux CI and on the standard-machine VM in `docs/fleet-benchmark.md`, with `dev` host runs labelled apart.
- [ ] The two stores are tried from a scratch workspace with an uncommitted `reef.toml`; each failure is filed (decision 12, deferred).
- [ ] E1 to E9 are measured and written up in `docs/benchmarks.md`, with a go or no-go for D.
- [ ] ADR 0073 records decisions 2 to 12 with the spike's numbers; the gates are agreed with the user.
- [ ] The follow-on issues are filed with sizes, order and planned numbers.
- [ ] `just ci` and `just doc` pass; `fleet.yml` runs report-only on a branch dispatch.

## Verification

```sh
export CARGO_TARGET_DIR=/Users/arsood/Projects/ReefVCS/target-264 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
cargo nextest run -p reef-testing fleet
cargo nextest run -p reef-lib spec
cargo run -p reef-testing --bin reef-fleet -- run --repo synthetic --profile dev --out "$TMPDIR/fleet-smoke"
# Stores: build them, then use them from a fresh workspace
reef store build web-node-modules && reef store build sdk-ts-node-modules
reef ws run -- just web-check
reef ws run -- sh -c 'cd sdk/typescript && npm test'
# The baseline, in the standard-machine VM (after checking df on the host)
reef-fleet run --repo all --profile standard --limit off --seed off --out ~/fleet/baseline
reef-fleet report ~/fleet/baseline --gate
gh workflow run fleet.yml --ref <branch>
just doc && just ci
```

### The measurement runbook (in `docs/fleet-benchmark.md`)

1. Pick the profile: `standard` in the VM (8 vCPUs, 16 GiB, 512 GB sparse APFS disk, Time Machine off, Spotlight as the user's default), `linux` on the runner, `dev` on the host for iteration only.
2. Check free space on the host and in the VM; record local snapshots (`tmutil listlocalsnapshots /`).
3. Install the toolchains at the versions in `repos.toml`; warm the download caches with `reef-fleet run --setup-only`.
4. Run each variant on a quiet machine; never two runs at once.
5. Read `summary.md`: the gate table first, then per-ticket times, the disk curve and the pressure timeline.
6. Clean up: `reef-fleet` releases its workspaces and stops its daemon; delete `GOCACHE`, the run's state directory and, after the baseline, the VM's disk image.

## Out of scope

- Building any of B to E (each its own ticket).
- Enforcing CPU or memory limits with cgroups on Linux, and passing the jobserver into the executor's sandbox (follow-ups named in ADR 0073).
- Remote caches (Bazel-style, sccache's remote backends) and Phase 4's store as a build cache.
- Cargo's checksum-based freshness (unstable today), which would remove E2's hazard when stable: noted in the ADR as the thing to revisit.
- Seeding on overlayfs (a lower layer per build directory) and on the copy backend.
- `reef init`'s detection message for #195 (its own issue; C adds the in-workspace hint).

## Risks

- **Real repositories drift and break offline.** Mitigated: pinned commits, warmed caches, a fallback per stack, and the synthetic smoke repository for CI's own test of the harness.
- **The VM is not a laptop.** Its vCPUs are mostly performance cores and its disk is a file on a faster SSD. Mitigated: gates are ratios, bytes and pressure states, never absolute times; the Linux runner gives a second, stricter view on cores.
- **The spike may show cargo rebuilding cloned dependencies.** Then D narrows to JavaScript and the Rust gate relies on B and E alone; the ADR records it, and G3's Rust number is revisited.
- **`reef.toml` changes every workspace of this repository**, including other agents' tickets, and a lockfile bump fails creates until `reef store build` runs. Mitigated: the PR says so, CONTRIBUTING says what to run, and the friction is filed as issues (a fallback to `MissingStores::Skip` with a hint is a candidate fix).
- **Disk on the dev machine.** A full five-ticket Rust run can take 100 GiB on the host volume. Mitigated by the runbook's check and by deleting the VM image after each run.

## Choices this brief made, and why

- **Benchmark, spike and ADR first, building nothing**, because the issue's acceptance asks for exactly that, and because the spike's answer changes D's shape.
- **Gates judged only on the standard profile**, because the dev machine overstates every resource the goal is about.
- **Clone stores, not sibling seeding or a shared target directory**, because mtime freshness and baked-in paths make both incorrect for Rust, and because a store built by the daemon from a commit cannot carry one agent's build output into another's workspace.
- **A soft budget and a hard floor**, because a budget that refused work would fail agents for space macOS could free, while a floor protects the machine itself; and never refusing a snapshot, because the design promises that written work is saved.
- **A jobserver for adaptivity and fixed counts for the rest**, because only a shared token pool lets one build use the whole machine and five share it, and the environment is the only channel every tool reads.
- **Delivery by environment, the provider contract and a cargo file**, because agents must not learn Reef-specific steps and Reef starts no process outside `ws run`.

## What later tickets inherit

- **B** builds decision 9 and the cargo file of decision 10, and turns on G5 to G8.
- **C** builds decision 7, closes #146, and turns on G1, G2 and G4.
- **D** builds decision 5 as the spike left it, and turns on G3.
- **E** builds decision 8 on C's table and D's declarations.
- **RX.06** runs `fleet.yml` with `--gate` once E lands.
- **R3.20** (compute budgets in policy) can read B's limits when it budgets checks.

## Departures from dev-phases.md

This issue has no row. When the user accepts the split, `dev-phases.md` gains rows for B to E beside RX.06 (or in Phase 3's cross-cutting table), and this directory's README gains a departures entry; neither is edited by this brief.

## Decisions for the user

Decided 2026-11-11: the user accepted every recommendation except decision 12, which is deferred (deferred by the user, 2026-11-11: tried in a scratch workspace and its friction filed as issues; no `reef.toml` lands in this ticket). The follow-ons B to E are filed as issues now.

**Group 1: scope**

1. **Split.** #264 = benchmark, baseline, stores dogfood, spike and ADR 0073 (M, top end, no API); then B build limit (M, rev 32), C disk accounting and floor (M top, rev 33, schema 18, closes #146), D clone stores (M, rev 34, schema 19), E idle eviction (S, rev 35). Order: #264, then B and C in parallel (B first if serial), then D, then E. *Recommended*: as stated. Alternatives: fold C into #264 (L) so the safety net lands with the baseline; one L ticket for B to E.

**Group 2: the benchmark**

2. **Standard-machine profile.** A macOS VM on the dev machine (8 vCPUs, 16 GiB, 512 GB sparse APFS) decides passes; `ubuntu-24.04` (4 vCPUs, 16 GiB) is the CI view; host runs are `dev` only. *Recommended*: as stated. Alternatives: host runs with Reef's jobs capped at 8 (no memory cap is possible on macOS); a paid larger Linux runner; buying a base MacBook as a runner.
3. **Repositories.** tokio (Rust), cli/cli (Go), excalidraw (TypeScript), fastapi (Python), Reef reported but not gated. *Recommended*: as stated, pinned at implementation. Alternatives: ripgrep for a lighter Rust row; add a Next.js application; gate Reef's own row too.
4. **Gates.** G1 to G9, with the issue's 2× disk scaling kept for Go, TypeScript and Python and 3× proposed for Rust; 100 GiB absolute peak; load p95 at most 2× cores. *Recommended*: as stated, confirmed after the baseline. Alternative: the issue's 2× for every stack (likely unmeetable for Rust, flagged).

**Group 3: build output and disk**

5. **Sharing build output.** Clone stores (a writable, daemon-built, lockfile-keyed directory cloned at create; the repository's command leaves dependencies only), subject to E3. *Recommended*: as stated. Alternatives: seed from a sibling's directory with Reef bumping mtimes of differing files (tool-agnostic, but baked paths and cross-agent poisoning remain); sccache by default; nothing for Rust beyond B and E.
6. **Automatic or declared.** Reef never builds a store without a `reef.toml` declaration; it prints the block to add. *Recommended*: as stated. Alternative: built-in recipes run automatically for a detected `Cargo.lock` or lockfile (zero setup, but the daemon runs repository build scripts unasked).
7. **Floor and budget.** A hard floor (default the larger of 20 GiB and 5%, counting purgeable space on macOS) refusing creates and store builds, never snapshots; a soft budget (default 25% of the volume) that warns and evicts. *Recommended*: as stated. Alternatives: a hard budget refusing creates too; `statvfs` only; different defaults.
8. **Eviction.** Declared build directories only, after 2 h idle, or least recently active first above the budget. *Recommended*: as stated. Alternatives: every ignored directory (risks `.env` and local data); a longer idle default such as 8 h; no automatic eviction (manual `reef ws evict` only).

**Group 4: concurrency**

9. **Mechanism.** A daemon-owned jobserver through `CARGO_MAKEFLAGS`, job counts fixed at start for other tools, `nice` 10, `jobs = min(cores, GiB/2)`. *Recommended*: as stated. Alternatives: fixed shares only; a semaphore on `ws run`; also `MAKEFLAGS` by default (breaks macOS's make 3.81).
10. **Reaching agents outside `ws run`.** The provider contract's `env` for adapters, plus a daemon-written `.cargo/config.toml` above the workspaces under the default root. *Recommended*: both. Alternatives: `ws run` only (agents through MCP or the provider get no limit); the provider `env` only.

**Group 5: numbers and documents**

11. **Numbers.** ADR 0073 in #264; revisions 32 to 35, schemas 18 and 19, rows S-193 to S-202 for B to E, renumbered at each brief. **Declarations** stay in `reef.toml` (an older daemon refuses the new keys). *Recommended*: as stated. Alternative: a separate `reef-build.toml` that older daemons ignore.
12. **This repository's `reef.toml`.** Two `npm ci` stores now, friction filed rather than fixed here. *Recommended*: as stated. Alternative: also a clone store for `target/` once D lands (Reef's own build is the heaviest user of disk).
