# 0073. Five tickets at once on a standard laptop

- Status: accepted; decisions 9 and 10 amended by [ADR 0074](0074-build-limit-jobserver.md), decisions 7 and 11 by [ADR 0076](0076-disk-accounting.md)
- Date: 2026-11-12
- Ticket: [#264](https://github.com/reef-vcs/reef/issues/264) ([brief](../dev-tickets/phase-3/issue-264-laptop-resource-budget.md))
- Follow-ons: [#271](https://github.com/reef-vcs/reef/issues/271) build limit, [#272](https://github.com/reef-vcs/reef/issues/272) disk accounting and floor, [#273](https://github.com/reef-vcs/reef/issues/273) clone stores, [#274](https://github.com/reef-vcs/reef/issues/274) idle eviction

## Context

Reef's users run agent fleets on their own machines, not in a data centre. The goal set on 2026-11-09 is that a standard laptop works on five tickets at once in a typical repository without running out of disk and without becoming unusable.

The tracked tree costs almost nothing: workspaces are copy-on-write. What costs is what builds add after the create. In Reef's own repository one workspace's `target/` reached 42 GB and a shared target directory 142–163 GB, and the disk filled at least five times in two weeks. Three agents building at once pushed the load to 40–50 on a 12-core machine and made timing tests fail. Sharing one target directory by hand was not only slow but wrong: cargo hashes workspace members relative to the workspace root, so one workspace ran test binaries another had built, with the other's paths baked in.

This ADR records how that cost is measured, and the designs four follow-on tickets build. The numbers it rests on are in [`docs/fleet-benchmark.md`](../fleet-benchmark.md) and [`docs/benchmarks.md`](../benchmarks.md#build-output-across-workspaces-264).

## Decision

### The machine and the gates

1. **The standard machine** is 8 cores (4 performance), 16 GiB of memory and a 512 GB SSD with 150 GiB free. Gates are judged only on runs with its limits: a macOS VM with 8 vCPUs, 16 GiB and a 512 GB sparse disk, and the `ubuntu-24.04` runner (4 vCPUs, 16 GiB). Runs on the dev machine are labelled `dev` and decide nothing.
2. **Gates are bytes, ratios and pressure states**, never raw wall time: G1 disk safety (the floor below), G2 at most 100 GiB for five tickets at peak, G3 five tickets at most 2× one ticket's disk (3× for Rust, whose linked test binaries are per workspace whatever is shared), G4 at most 2 GiB left after release and gc beyond what is kept on purpose, G5 load p95 at most 2× the cores, G6 no critical memory pressure and at most 2 GiB swapped out, G7 `ws list` p99 under 1 s and a 10 ms sleep late by under 50 ms at p99, G8 the median ticket at most 5× its solo time (and, with the limit, the makespan at most 1.1× without it), G9 every ticket's test outcomes equal to its solo run's.
   The user kept these thresholds unchanged after the baseline (2026-11-12), although G3 fails for every stack and G5 for every compiled one: they are what #271 to #274 must meet.
3. **`reef-fleet`** measures them on one pinned repository per stack (tokio, cli/cli, excalidraw, fastapi) and on Reef itself, reported but not gated. Five tickets run at once through `reef ws run` with a review round each, then sit idle, then are released. `fleet.yml` runs it weekly on Linux, report-only, until RX.06 turns the gate on.

### Build output

4. **Clone stores share build output.** A `[[store]]` in `reef.toml` gains `mode = "clone"` and `key = [...]` (extra files hashed into the key). The daemon builds the store once per key in a scratch workspace, as R1.20's stores are built, and at create clones the sealed directory into the workspace's mount path: APFS `clonefile`, `FICLONE` on btrfs and XFS, and nothing on overlayfs or the copy backend, where a copy would cost what it saves. The clone is the workspace's own: writable, counted as build output, evictable. Reef owns the key, the build, sealing, cloning, accounting, eviction and gc; the repository owns which directory, which files key it, and the command that fills it. For Rust the command builds the workspace and then `cargo clean -p` each member, leaving dependencies only.
   The spike's verdict: go for JavaScript (a cloned `node_modules` took excalidraw's five tickets from 4.4 to 0.9 GiB); for Rust correct but modest (tokio's dependencies stayed fresh, 8 units compiled instead of 112, but its test binaries are most of `target/`, so unshared bytes fell only 9%); not needed for Go or Python.
5. **Never seeded from a sibling, never one shared target directory.** Cargo judges freshness by mtime and bakes paths into artifacts. The spike showed both hazards: a sibling's build cloned into a new workspace ran the sibling's test binaries (a probe reading `CARGO_MANIFEST_DIR` failed), and a sibling that had applied a patch gave unedited workspaces its patched test binaries. A store built by the daemon from a commit, with members cleaned, carries no workspace's edits and rebuilt every member correctly.
6. **No automatic builds.** Reef never runs a repository's build without a declaration: a daemon running `build.rs` unasked changes S-03's trust posture. When a repository has a lockfile and no store, `reef here` prints the `reef.toml` block to add.

### Disk

7. **One daemon-kept figure per workspace** (#146): tracked bytes, build output (and of it the evictable part), unshared bytes (APFS's private size), files and when measured, from one capped walk on the blocking pool. **A floor** refuses creates and store builds when free space falls below the larger of 20 GiB and 5% of the volume, with reason `DISK_SPACE_LOW` and a `next` naming the largest idle workspace to release. It never refuses a snapshot, a release or a gc: written work is saved, and releasing is how space comes back. On macOS free space counts purgeable space (the volume's important-usage capacity), which macOS frees under pressure. **A soft budget**, 25% of the volume, warns and evicts but never refuses. Agents see the level as data in `reef here` and `ws list` only when it is not `ok`.
8. **Idle eviction** drops a workspace's declared build directories (clone store mounts and `[build] dirs`, ignored by the tree and not symlinks) after 2 hours idle, or least recently active first over the budget. The change, the directory, the lease and the tracked files stay. Undeclared ignored files (a `.env`, a local database) are never deleted.

### Concurrency

9. **A machine-wide jobserver owned by the daemon**, in GNU make's protocol over a FIFO, handed to cargo as `CARGO_MAKEFLAGS=-j --jobserver-auth=fifo:<path>` (not `MAKEFLAGS`: macOS's make 3.81 does not know the form). A build alone gets every token; five share them. Tools without a jobserver get job counts fixed at start (`RUST_TEST_THREADS`, `NEXTEST_TEST_THREADS`, `GOFLAGS=-p`, and names a repository declares), never overriding a variable already set. Commands `ws run` starts run at `nice` 10. `jobs` defaults to the smaller of the cores and memory in GiB ÷ 2.
10. **Reaching commands Reef does not start**: `ws run` sets the environment; the provider contract's `create` and `attach` print an `env` object for the adapter; and under the default root Reef owns, a daemon-written `.cargo/config.toml` above the workspaces sets cargo's job count, which a repository's own file overrides.

### Spotlight and Time Machine

11. **#272 excludes the default workspace root from Time Machine** (the sticky exclusion attribute, no privilege needed) **and from Spotlight** (`.metadata_never_index`, to verify on macOS 27): build output is neither worth backing up nor searching. The baseline did not isolate `mds`'s CPU (the standard runs' load comes from the builds themselves), so #272 measures it before and after. A Time Machine exclusion does not stop local snapshots holding a released workspace's blocks, which is why the floor counts purgeable space.

### Numbers

12. API revisions 32 to 35, schemas 18 and 19 and threat rows S-193 to S-202 are planned for #271 to #274, renumbered at each brief. Declarations stay in `reef.toml`, one file: an older daemon refuses a newer file with a parse error naming the key.

## Alternatives

- **One shared `CARGO_TARGET_DIR`**: incorrect (baked paths, binaries replaced under a running suite) and serialized by cargo's lock.
- **sccache by default**: saves compile time, not disk (outputs are copied into each target directory); a repository's own choice.
- **Seeding from a sibling**, with or without bumping mtimes: the spike showed wrong reuse, and one agent's workspace would become the input of another's build.
- **A stable path inside every workspace**: needs a mount namespace, Linux only and restricted on hosted runners.
- **Automatic builds from detection**: zero setup, but the daemon would run repository build scripts unasked.
- **A hard budget**: would fail agents for space the system could free. **`statvfs` alone**: would refuse creates macOS can serve.
- **Evicting every ignored directory**: risks `.env` files and local data.
- **A fixed share per build**: a lone build would get a fifth of the machine. **A semaphore on `ws run`**: an agent harness is often itself the `ws run` command and would hold a slot for hours. **cgroups**: Linux only; a later follow-up for real enforcement there. **`MAKEFLAGS`**: breaks macOS's make. **Background QoS**: confines work to efficiency cores and throttles I/O.
- **A separate declarations file**, which older daemons would ignore: the store and build declarations are one concern, and the parse error names the key.

## Consequences

- The largest Rust and Go savings measured are the repository's own settings: `CARGO_INCREMENTAL=0` with line-tables-only debug info took tokio's five tickets from 26 to 6.3 GiB and its median ticket from 267 to 171 s; `-trimpath` halved cli's disk and makespan. Reef recommends them (docs, and #272's hint) and never sets them: they change what a build produces.
- Each follow-on has a baseline to beat and a gate it turns on: #271 G5 to G8, #272 G1, G2 and G4, #273 G3, #274 builds on #272's figures and #273's declarations.
- Cargo's checksum-based freshness, once stable, would remove the sibling hazard; it is the thing to revisit.
- The executor's sandbox does not get the jobserver yet, and Linux gets no cgroup enforcement: both are follow-ups.
