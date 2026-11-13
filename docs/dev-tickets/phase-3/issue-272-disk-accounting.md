# Issue #272: disk accounting per workspace, and a free-space floor

Phase 3 · Size M, top end · Fixes: issue #272 (follow-on C of #264), #146 · Depends on: #264 (ADR 0073, the fleet benchmark and its baseline), R1.02 (backends, the base cache), R1.20 (stores), R2.20 (the web UI's Disk column), #260 (the `~/.reef` default root) · Builds on: ADR 0029 (v1 only grows), ADR 0045 (brief JSON), ADR 0050 (`reef schema`), #95 (`Footprint`) · Interacts with: #271 (B, PR #290: its own revision, `GetDaemonStatusResponse.build`, store builds under its limit), #273 (D: clone stores count as build output), #274 (E: evicts by this ticket's figures and budget), #195 (its in-workspace detection is part of the warnings) · Unblocks: G1, G2 and G4 of the fleet gate (RX.06), #274

## Context

A full disk breaks everything at once: snapshots, the daemon's own writes, Docker, the user's other work. Reef's own repository filled the dev machine's disk at least five times in two weeks (#264). Nothing in Reef stops it today: `fs_util::free_bytes` is called only by tests and a bench, and no create, base materialization or store build looks at free space.

Nor does Reef know what a workspace costs. `GetWorkspace` walks the directory on request and reports `DiskUsage { allocated_bytes, files_changed }`: every file that differs from the base, ignored build output included, at full size even where APFS shares the blocks with the base. `ListWorkspaces` carries no figure, so the web UI's Disk column walks every workspace from the browser, one `GetWorkspace` at a time (#146, R2.20).

ADR 0073 decision 7 fixed the design: one daemon-kept figure per workspace from one capped walk; a hard floor (the larger of 20 GiB and 5% of the volume, counting purgeable space on macOS) that refuses creates and store builds and never a snapshot, a release or a GC; a soft budget (25% of the volume) that warns; the level shown to agents only when it is not `ok`. Decision 11 gave this ticket Time Machine and Spotlight exclusion of the default root. This brief turns that into a plan and corrects it where an experiment on this machine contradicted it (listed under "Departures from ADR 0073").

The issue's acceptance: #264's disk gates (no refused creates in the benchmark, peak and release figures) measured by the daemon's own numbers, and a create below the floor refused with `next`.

### What was verified while writing this ticket

Reef at `8fce9be1` (#264 merged): API revision 30 (`crates/reef-api/src/lib.rs:49`), schema 17, ADR 0073 the latest, threat rows to S-187. Open in parallel: #271 (PR #290: revision 31, ADR 0074, S-188 to S-190, `GetDaemonStatusResponse.build = 13`, no migration) and R3.16 (revision 32, schema 18, ADR 0075, S-191 to S-195). This brief takes **API revision 33**, **schema 19** (18 if R3.16 adds no migration or merges after this), **ADR 0076** and threat rows **S-196 to S-199**, renumbered on rebase. Paths are under `crates/` unless they start with `docs/`, `web/`, `sdk/` or `.github/`.

- **The proto.** `Workspace` ends at field **23** (`behind_trunk`, #259), so ADR 0073's `Workspace.disk = 23` becomes **24**. `GetDaemonStatusResponse` ends at 12 (`remote`); #271 takes 13 (`build`), so `disk` is **14**. `GetWorkspaceResponse` ends at 7, `ListWorkspacesResponse` at 3, `DiskUsage` at 2. Error reasons are constants in `reef-api/src/lib.rs` (`pub mod error`), with `hint` and `next` (an argv) built by `reef-daemon/src/error.rs` (`advice_of`).
- **Today's walk.** `Backend::disk_usage` (`backend/mod.rs:368`): clone and copy backends call `fs_util::changed_files_vs_base` (`fs_util.rs:149`), a `walkdir` walk of the whole root compared with the base's `tree_state` by mtime and size, skipping store mounts; overlay backends walk the upper directory (`overlay/mod.rs:396`). `Footprint` (`fs_util.rs:39`) adds APFS's private size per file through `apfs::private_bytes` (`getattrlist` with `ATTR_CMNEXT_PRIVATESIZE`), used nowhere in the daemon's answers. `Manager::inspect` walks only when asked (`manager.rs:580`); the release path walks again for `ReclaimRecord.freed_bytes` (`reclaim.rs:428`).
- **Where creates go.** `Manager::create` (`workspace/create.rs:117`): paths and backend (step 1), resolve (2), `ensure_base` (3, may materialize a whole tree), materialize (4). A store build (`store/build.rs:162`) answers an already-sealed store first, then makes its scratch workspace through `Manager::create` with the copy backend: a full copy of the tree. `MoveWorkspace` and `reef rebase` may also materialize a base.
- **Watch events include ignored paths.** notify reports every path under a workspace; jj's scan drops ignored ones afterwards (`watcher/mod.rs`). So a build writing `target/` produces watch events and scans, but no snapshot. Watchman's subscription is jj's query and may not.
- **Configuration.** `config.toml` has no `[disk]` (`reef-daemon/src/config.rs:93`, `KEYS`). `Config::workspace_base` is `Some(~/.reef)` only under the default root (#260); bases live at `<state>/repos/<id>/bases/`, stores at `<state>/stores/`. `base_cache` rows already carry `bytes` (allocated at materialization); `stores` rows carry none.
- **The clients.** `reef here` asks with `skip_disk_usage: true` and prints a flattened `json::Workspace` plus `hint` and `next`; its text must stay within 12 lines (`here.rs:400`). `reef ws list` prints a JSON **array** (full and brief), so a list-level object has nowhere to go without changing its shape; its brief view is a key subset of the full element (`brief.rs:1084`). `ws inspect` prints `disk_usage`. `reef daemon status` prints `json::Status`. `reef schema` derives every row from schemars, so new fields need no row edits; `outputs_match_their_schemas` (`reef-vcs/tests/schema.rs:413`) validates real outputs. MCP's `list_workspaces` returns brief elements, `get_workspace` the whole `Inspected`; `tools/list` carries no output schemas, so new fields do not touch its 23,350-byte budget. The SDKs are generated from the protos; nothing hand-written lists `Workspace` fields.
- **The web UI.** `web/src/fleet/disk.ts` (`DiskScheduler`) walks one workspace at a time through `GetWorkspace`, spaced by `paced.ts`; `Fleet.tsx` renders the column; `WorkspaceDetail.tsx` has "Measure again". The bundle budget is `reef-web/tests/assets.rs` (650 KiB total, 550 KiB per route).
- **The fleet harness** walks workspaces itself (`reef-testing/src/fleet/disk.rs`, "not the daemon's … (#146, which C builds)"), computes the floor as the larger of 20 GiB and 5% of the volume (`report.rs:164`), counts a refused create as a `ws run` with no workspace id, and reads no daemon disk figure.
- **Nothing detects a dependency layout** (#195): no CLI or daemon code looks at `package-lock.json`, `uv.lock` or `Cargo.lock`.

#### The experiments

Spikes under the session scratchpad (`i272/spike/`), never in the workspace: a Rust binary (CoreFoundation declared locally, `libc` 0.2.191's `getattrlist` and `getattrlistbulk`), run on the dev machine (M2 Max, macOS 27.2, APFS, 926 GiB volume).

| | Experiment | Result |
| --- | --- | --- |
| D1 | Free space three ways, `/` and `~/.reef`, 5 runs each | `statvfs` (`f_bavail × f_frsize`): **96.6 GiB in 2–18 µs**. CoreFoundation's `kCFURLVolumeAvailableCapacityForImportantUsageKey`: **141.4 GiB in 9–43 ms** (an IPC; the first call 26 ms). `…ForOpportunisticUsageKey`: 86.5 GiB, 8–35 ms. So 45 GiB was purgeable with no Time Machine local snapshot present (`tmutil listlocalsnapshots /` listed only OS-update snapshots): purgeable space is more than snapshots. `core-foundation-sys` 0.8.7 has both the key and `CFURLCopyResourcePropertyForKey` commented out, so the daemon declares the two itself, as `apfs.rs` declares its calls |
| D2 | One walk of a Rust `target/` (36,676 entries, 42.0 GiB), three ways, warm | stat only: 140–166 ms (3.8–4.5 µs per entry). Plus `getattrlist` private size per file: 346–945 ms (9–26 µs). **`getattrlistbulk` per directory** (name, type, allocated size and private size in one call): **287–297 ms (7.8–8.1 µs)**, the same totals. Private size **40.83 of 42.00 GiB (97%)**: build output shares nothing |
| D3 | The same on `~/.cargo/registry/src` (93,705 small files, 1.53 GiB) | stat 427–533 ms; per file 0.80–1.21 s; bulk 0.67–0.71 s. Private = allocated |
| D4 | This brief's own workspace (an APFS clone, 1,567 entries) | 0.02 GiB allocated, **0.00 GiB private**, walked in 6–9 ms: an unedited clone costs nothing, which today's figure gets right only by skipping unchanged files |
| D5 | Time Machine: `tmutil addexclusion <dir>` (no `-p`), unprivileged | Succeeds; writes the xattr `com.apple.metadata:com_apple_backup_excludeItem` = a binary plist string `com.apple.backupd`; `tmutil isexcluded` reports the directory and new children excluded. Writing the same xattr directly works too. `~/.reef` and the state directory are `[Included]` today |
| D6 | Spotlight: a file with a unique token in a hidden home directory, a visible one, a visible one holding `.metadata_never_index`, a `*.noindex` one; `mdls` and `mdfind` after 30 s | Hidden directory: **not indexed** (15 `mdls` attributes, filesystem only; `mdfind` finds nothing). Visible: indexed (47 attributes, text embedding). **`.metadata_never_index` no longer works** in a non-root directory (indexed, 47). `*.noindex`: not indexed. A file in this workspace under `~/.reef`: 15 (not indexed). A file in a **base** under `~/Library/Application Support/reef/repos/…/bases/`: **41, indexed** |
| D7 | Linux, the privileged `reef-linux-dev` container (ext4, 911 GiB): `statvfs`; a kernel overlay over 2,000 lower files of 8 KiB, then one byte appended to 10, 100 new 64 KiB files in `target/`, one deleted | `f_bfree` 233.3 M blocks, **`f_bavail` 221.2 M: 5% (46 GiB) reserved for root**, so `f_bavail` is the figure (what `fs_util::free_bytes` already reads). The upper directory held 113 entries and 6.68 MB: the 10 appended files **copied up whole** (8,194 bytes each), the 100 new ones, one whiteout. Its walk is exactly the workspace's unshared bytes. The image has no `mkfs.xfs` or `mkfs.btrfs`, so reflink sharing (`FIEMAP`) was not tried |

What the experiments change: the floor reads `statvfs` first and asks CoreFoundation only near the floor (D1); the walk uses `getattrlistbulk` on macOS (D2, D3); unshared bytes matter for tracked files and clones, not for build output (D2, D4); on Linux `f_bavail` and an overlay's upper walk are exact, and reflink stays unknown (D7); Spotlight needs nothing under `~/.reef` and `.metadata_never_index` is dropped (D6).

## Decisions

### Size (decision 1)

**M, top end**: one daemon module (the walk, the kept figures, the volumes, the floor, the level), one migration, five proto additions, four CLI outputs, the web column's switch to the daemon's figure, the Time Machine exclusion, the harness reading the daemon's figures, and the gate runs. Out: eviction (E), `evictable_bytes` (decision 4), Spotlight work on the bases (decision 13).

### What one walk measures (decisions 2 to 4)

**One walk per workspace** (`disk/walk.rs`), never following symlinks, skipping store mounts, on the blocking pool, producing:

| Figure | Meaning |
| --- | --- |
| `tracked_bytes` | allocated bytes of files the workspace's working copy tracks (its own `tree_state`, as of the last snapshot) that differ from the base by mtime or size: what today's figure counts, without ignored files |
| `build_bytes` | allocated bytes of files the working copy does not track: ignored build output and dependencies (`target/`, `node_modules/`, `.venv/`), and new files not yet snapshotted or over `snapshot.max_new_file_size` |
| `unshared_bytes` | what releasing frees at once: APFS's private size of every file (D2, D4); on overlay, the upper directory's allocated bytes (exact, nothing in it is shared); on the copy backend, every file's (nothing is shared); **unset on reflink**, where only a `FIEMAP` per file could tell (D7) and `tracked_bytes + build_bytes` stands in |
| `files` | entries walked |
| `largest` | the three largest untracked directories at depth 1 to 3 (repository-relative, with bytes), such as `target` or `web/node_modules`: what an agent or E would remove |
| `missing_dependencies` | directories with a dependency manifest and lockfile but no dependency directory (decision 11) |
| `measured_at`, `incomplete` | when; whether a cap stopped the walk (the figures are then lower bounds) |

**Classification** (decision 2) uses the workspace's own `tree_state` (read with `tree_state_entries`, as `changed_files_vs_base` reads the base's), not `.gitignore` matching: jj already decided what it tracks, and re-matching ignore rules would cost more and could disagree. A file in neither the base nor the working copy is build output until the next snapshot tracks it; the figure is "as of the last snapshot", like `changed_file_count`.

**On macOS the walk is `getattrlistbulk`** per directory (name, type, `ATTR_FILE_ALLOCSIZE`, `ATTR_CMNEXT_PRIVATESIZE`, link count and file id for hard links), 1.3 to 1.7× faster than a stat walk with a private-size call per file (D2, D3), in one `unsafe` function with a `SAFETY` comment, its buffer parsing tested on canned bytes. Elsewhere `walkdir` and `symlink_metadata`, as today. Hard links count once (`Footprint`'s rule).

**Caps** (decision 3): 2,000,000 entries or 30 s, whichever first, then `incomplete` (ADR 0073). At D2's 8 µs per entry the entry cap is about 16 s; the `large` synthetic preset (100,000 files) about 0.8 s. The walk's thread runs at lowered priority (`symbols::lower_priority`, the existing helper).

**`evictable_bytes` is left to #274** (decision 4). It is the bytes under declared build directories, and nothing declares one until #273's clone stores and #274's `[build] dirs`; a field that is always 0 would mislead. #274 adds it to `WorkspaceDisk` (v1 grows) with its declarations. `largest` gives E and agents the candidates meanwhile.

**`DiskUsage` stays** and comes from the same walk: `allocated_bytes = tracked_bytes + build_bytes`, `files_changed` as today, so `ws inspect`'s existing line and every client reading it are unchanged. `Backend::disk_usage` and `changed_files_vs_base` are replaced by the walk (the backends keep `tear_down` and the rest).

### When it measures (decision 5)

The daemon keeps one figure per active workspace, in the `workspace_disk` table (decision 6), and refreshes it:

- **When the workspace is dirty**: any watch event for it (ignored paths included, so a build counts) marks it dirty; a dirty workspace is measured once at least `[disk] measure_interval` (default **5 min**) has passed since its last walk. A build that runs for an hour is walked about 12 times; an idle workspace never. A Watchman workspace, whose events may omit ignored paths, is also measured every 3 × `measure_interval` while its lease heartbeats.
- **On request**: `GetWorkspace` without `skip_disk_usage` walks now (as today), updates the kept figure and returns both. A walk already running for that workspace is joined, not repeated.
- **At create**: the row starts at zero with `measured_at` the create (an unedited clone costs nothing, D4), so a new workspace has a figure with no walk.
- **At daemon start**: figures come from the table; every active workspace is marked dirty (events while the daemon was down are unknown) and measured in the background, oldest figure first.
- **At release**: the row is deleted after the record moves to released; the release's `freed_bytes` uses the kept `unshared_bytes` when it is under `measure_interval` old and complete, else walks as today.

**One background walk at a time machine-wide** (a semaphore of one), oldest-dirty first, so five building workspaces cost at most one walk in flight: at D2's rate, a 40 GiB `target/` every 5 minutes is 0.3 s of one thread, about 0.1% of a core. On-request walks run outside the semaphore, as `GetWorkspace`'s do today.

**Machine-wide figures**: bases from `base_cache.bytes` (an upper bound: bases cloned from each other share blocks); stores and the state directory's own files (database, artifacts, evidence, logs) walked once an hour and after a GC, kept in memory; workspaces from the table. Free space per volume is read on demand and cached 5 s (decision 7).

### The table (decision 6, schema 19)

```sql
-- #272: the daemon-kept disk figure of each active workspace (ADR 0076), and the eviction
-- columns #274 fills. A row per active workspace, deleted at release. Released migrations
-- are never edited.
CREATE TABLE workspace_disk(repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
       tracked_bytes INTEGER NOT NULL, build_bytes INTEGER NOT NULL, unshared_bytes INTEGER,
       files INTEGER NOT NULL, largest TEXT NOT NULL DEFAULT '[]', missing_dependencies TEXT NOT NULL DEFAULT '[]',
       incomplete INTEGER NOT NULL DEFAULT 0, measured_at INTEGER NOT NULL,
       build_output TEXT, evicted_at INTEGER, evicted_bytes INTEGER, evict_reason TEXT,
       PRIMARY KEY (repo_id, workspace_id));
```

Keyed by repository (ADR 0003), copied by backups. Persisted rather than kept in memory so a restart costs no walks before the web column and `ws list` have figures, and so `measured_at` means what it says. The four nullable eviction columns are ADR 0073's "E's columns from the start", which #274's issue counts on ("no schema change"); this ticket never writes them.

### Free space, the floor and the budget (decisions 7 and 8)

**Volumes.** The daemon tracks the volume of the workspace root (`Config::workspace_root`, or its nearest existing ancestor) and the state directory's (bases, stores, the database); under #260's default they are usually one volume, told apart by `st_dev`. Each volume's figures: total, `free` (`statvfs`), on macOS `purgeable_free` (CoreFoundation's important-usage capacity, D1), the floor, the level.

**Usable free space** is `statvfs` free, or on macOS the larger of it and the important-usage capacity. **The fast path is `statvfs`** (2 µs): at or above the floor it decides alone, since usable free space is the larger of the two; only below it does the daemon ask CoreFoundation (9 to 43 ms), cached 5 s. `reef daemon status` always asks, to show it.

**The floor** (decision 7): `[disk] floor = "auto"`, the larger of 20 GiB and 5% of the volume's total (24 GiB on a 512 GB disk, 46 GiB on the dev machine's 926 GiB), or a size (`"30GiB"`), or `"0"` for none. Below it on either volume:

- **Refused**: `CreateWorkspace` (checked at step 1, before the base or any directory is made, so a remote create too) and `BuildStore` (after the already-sealed answer, before `start_build`). Reason **`DISK_SPACE_LOW`**, kind `Conflict`, gRPC `FAILED_PRECONDITION`, exit 5 (ADR 0073). Metadata: `volume` (a directory on it), `free_bytes`, `floor_bytes`, and when there is one, `largest_idle` (workspace id), `largest_idle_owner`, `largest_idle_bytes`. Hint: "<free> free on the volume holding <dir>, under Reef's floor of <floor>. Releasing a workspace takes a final snapshot first, so nothing written is lost." `next`: `reef ws release <largest idle>` when there is one, else `reef ws list` (which shows each workspace's disk while the level is low; ADR 0073 said `reef ws gc`, which frees little: departure).
- **Never refused** (ADR 0073, and the issue): snapshots, releases, reclaims, GC, renewals, publishes. Also **moves and rebases**, though they may materialize a base: they act on a workspace that exists, an agent mid-ticket must be able to continue, and `reef rebase` is how a stale workspace comes back. They log a warning when they run below the floor.
- **Largest idle**: among active workspaces with no live `Lease` or `ACTIVITY` presence entry and no snapshot in the last 30 minutes, the one with the most `unshared_bytes` (else `tracked_bytes + build_bytes`).

**The budget** (decision 8): `[disk] budget = "auto"`, 25% of the workspace volume's total (about 115 GiB on 460 GiB usable), or a size, or `"0"` for none. Reef's total is the workspaces' `unshared_bytes` (else `tracked_bytes + build_bytes`), bases, stores and the state directory. Soft: it never refuses; #274 evicts against it.

**The level**: `low` below the floor on either volume; else `warn` when Reef's total is over the budget or usable free space is under twice the floor; else `ok`. Each `warn` and `low` carries a `reasons` list (`FREE_BELOW_FLOOR`, `FREE_NEAR_FLOOR`, `OVER_BUDGET`).

### The API: revision 33 (decision 9)

`workspaces.proto`:

```proto
// In Workspace:
  // What its directory costs on disk, from the daemon's last walk (#272,
  // ADR 0076). Set by ListWorkspaces and GetWorkspace for an active
  // workspace once measured (at create it is all zero); unset otherwise and
  // at an operation.
  WorkspaceDisk disk = 24;

// What a workspace's directory costs on disk, as of `measured_at`.
message WorkspaceDisk {
  // Allocated bytes of tracked files that differ from the base.
  uint64 tracked_bytes = 1;
  // Allocated bytes of files the working copy does not track: ignored
  // build output and dependencies, and new files not yet snapshotted.
  uint64 build_bytes = 2;
  // What releasing it frees at once: APFS's private size, an overlay's
  // upper directory, every file of a copy. Unset where the filesystem
  // cannot tell (reflink); `tracked_bytes + build_bytes` is the bound then.
  optional uint64 unshared_bytes = 3;
  // Entries walked.
  uint64 files = 4;
  // When the walk ended.
  google.protobuf.Timestamp measured_at = 5;
  // The walk stopped at a cap (2,000,000 entries or 30 s): every figure
  // is a lower bound.
  bool incomplete = 6;
  // Up to three of the largest untracked directories, largest first.
  repeated DiskPath largest = 7;
  // Directories with a dependency manifest and lockfile but no dependency
  // directory, such as `web` with `package-lock.json` and no
  // `node_modules` (#195). At most 8.
  repeated string missing_dependencies = 8;
}

// A directory and its allocated bytes.
message DiskPath {
  // Repository-root relative, `/`-separated.
  string path = 1;
  uint64 bytes = 2;
}

// In GetWorkspaceResponse:
  // The machine's disk level and figures (#272); unset at an operation.
  DiskStatus disk_status = 8;
// In ListWorkspacesResponse:
  // The machine's disk level and figures (#272); unset at an operation.
  DiskStatus disk_status = 4;
```

Also in `workspaces.proto`, since `daemon.proto` already imports it (the other way round would be a cycle), and used by `daemon.proto` as `GetDaemonStatusResponse.disk = 14`, after #271's `build = 13`:

```proto
// How full the disk is, by Reef's floor and budget (#272, ADR 0076).
message DiskStatus {
  DiskLevel level = 1;
  // Why it is not OK; empty when it is.
  repeated DiskReason reasons = 2;
  // The workspace root's volume and, if different, the state directory's.
  repeated DiskVolume volumes = 3;
  // `[disk] budget` in bytes; 0 for none.
  uint64 budget_bytes = 4;
  // What Reef holds, by kind, and their sum.
  uint64 workspaces_bytes = 5;
  uint64 bases_bytes = 6;
  uint64 stores_bytes = 7;
  uint64 state_bytes = 8;
  uint64 reef_bytes = 9;
  // The workspace a release would best free space from; unset if none is idle.
  IdleWorkspace largest_idle = 10;
  // Workspaces whose figure is older than `[disk] measure_interval` or incomplete.
  uint32 stale_workspaces = 11;
}
enum DiskLevel { DISK_LEVEL_UNSPECIFIED = 0; DISK_LEVEL_OK = 1; DISK_LEVEL_WARN = 2; DISK_LEVEL_LOW = 3; }
enum DiskReason { DISK_REASON_UNSPECIFIED = 0; DISK_REASON_FREE_BELOW_FLOOR = 1; DISK_REASON_FREE_NEAR_FLOOR = 2; DISK_REASON_OVER_BUDGET = 3; }
message DiskVolume {
  // A directory on it: the workspace root, or the state directory.
  string path = 1;
  // `workspaces`, `state`, or both.
  repeated string holds = 2;
  uint64 total_bytes = 3;
  // `statvfs`: available to an unprivileged user.
  uint64 free_bytes = 4;
  // macOS: free plus what the system purges under pressure (the
  // important-usage capacity); unset elsewhere or when not read.
  optional uint64 purgeable_free_bytes = 5;
  uint64 floor_bytes = 6;
  DiskLevel level = 7;
}
message IdleWorkspace { string workspace = 1; string repo_id = 2; string owner = 3; uint64 bytes = 4; google.protobuf.Timestamp idle_since = 5; }
```

The reason constant `DISK_SPACE_LOW` in `reef_api::error`, documented on `CreateWorkspace` and `BuildStore`. `API_REVISION` 33 with its digest; `just api` and `just api-breaking` against the root's `.git`. No new RPC, so no `NOT_TOOLS` row and no remote-rules row; `disk_status` and `GetDaemonStatusResponse.disk` are served through the remote listener as the rest of those responses are (volume paths are the daemon's own, already shown in `state_dir`).

### What agents and people see (decision 10)

ADR 0073: agents see the level as data only when it is not `ok`, so the token benchmark's outputs stay as measured.

- **`reef here`**: JSON and text gain `disk_status` (level, reasons, the lower volume's free and floor, `largest_idle`) **only when the level is not `ok`**, and the workspace's own `disk` object only then too (the flattened `Workspace` drops it otherwise). The hint and `next` follow `advise`'s order: after divergence, attention and grace, **a `low` level comes first** ("The disk is nearly full (<free> free, floor <floor>); new workspaces are refused. Release workspaces you are done with." `next`: release the largest idle, or this one when it is `ws run`'s and nothing else is idle), then `behind_trunk`, then `missing_dependencies` (decision 11), then `warn` (the same, softer), then the rest. Text adds at most one line (`disk: low, 18 GiB free (floor 24 GiB)`), inside the 12-line test.
- **`reef ws list`**: full JSON elements always carry `disk` when set. Brief elements gain `disk` (reduced to `unshared_bytes`, else `build_bytes`) **only when the level is not `ok`**; a brief key stays a subset of the full element's. The list's top-level shape stays an array: the level itself goes to **stderr** as one `reef: disk <level>: …` line with the `next`, in every output form, only when not `ok`. Text gains a `DISK` column (`du -h` style, `render::size`, `-` when unmeasured, `~` suffix when incomplete).
- **`reef ws inspect`**: a `disk` block: tracked, build output, unshared (or "not known on this filesystem"), files, the largest directories, missing dependencies, when measured, and "incomplete" when it is; the existing `disk_usage` line stays.
- **`reef daemon status`**: always a `disk` line per volume in text (`disk: ok, 96 GiB free (141 GiB with purgeable) of 926 GiB, floor 46 GiB; Reef 12.3 GiB of a 232 GiB budget`) and the `disk` object in JSON.
- **Errors**: `DISK_SPACE_LOW` needs no client code (`ErrorObject` carries `hint`, `next`, `metadata`).
- **`reef schema`**: no row edits (schemars). The schema test runs `here`, `ws list` and `ws inspect` once more against a daemon configured with `[disk] floor` above the volume's free space, so the not-`ok` shapes are validated too.
- **MCP**: `list_workspaces` gets brief elements (so `disk` only when not `ok`); `get_workspace` gets `Inspected` whole. No tool description changes; `tools/list` is unaffected.
- **SDKs**: generated, nothing to write. Their cached `GetDaemonStatus` (for the revision check) must not be read for `disk`; their docs say to read `disk_status` from `ListWorkspaces`.
- **Web UI**: the Disk column reads `Workspace.disk` from `ListWorkspaces` (the page already lists with `changed_file_counts`): `unshared_bytes` (else tracked plus build), with a tooltip of tracked, build, largest and when measured. `DiskScheduler` and its pacing go (#146's browser walks); "Measure again" calls `GetWorkspace` with the walk, which updates the kept figure. A banner on the workspaces page when `disk_status.level` is not `ok`. The bundle shrinks.

### #195: dependencies missing in a workspace (decision 11)

The walk sees every directory, so detection is free: a directory with `package.json` and one of `package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `bun.lockb` but no `node_modules`; or `uv.lock` or `poetry.lock` with `pyproject.toml` but no `.venv`. Not `Cargo.lock` or `go.sum`: their build directories appear at the first build, and they have no install step. At most 8 directories, at depth 0 to 3, skipping anything under an untracked directory (a dependency's own `package.json` in `node_modules` is not the repository's).

`reef here` turns it into its hint: "web has package-lock.json but no node_modules: each workspace installs its own (`npm ci` there), or the repository can declare a shared store in reef.toml (`reef store --help`)." `next`: the install command for that manager in that directory (`npm ci --prefix web`, `pnpm install --frozen-lockfile --dir web`, `yarn --cwd web install --immutable`, `uv sync --frozen --directory <dir>`). Not the `reef.toml` block: symlink stores are read-only, and `npm ci` against one fails (#278) until #273's clone stores make them writable; the docs carry the block. It shows while the directory is missing, then disappears; it costs nothing when there is nothing to say.

The ADR's other hint, `CARGO_INCREMENTAL=0` and line-tables-only debug info, goes into `here`'s `warn` and `low` hints when the workspace's `largest` names a `target` directory over 2 GiB: "Rust build output is most of this; CARGO_INCREMENTAL=0 and line-tables-only debug info cut it about fourfold (docs/fleet-benchmark.md)." Reef never sets them (ADR 0073).

### Time Machine and Spotlight (decisions 12 and 13)

**Time Machine** (decision 12): on macOS, at start, the daemon sets the sticky exclusion xattr (D5's bytes) on `~/.reef` (only under the default root, S-185's checks) and on `<state>/repos/*/bases` and `<state>/stores` as each is created, unless `[disk] exclude_from_backups = false`. Everything excluded is a cache: a workspace's tracked work is in the repository's jj store, which stays backed up with the repository; bases and stores are rebuilt from commits; the database and evidence stay in the backup. Set **once**: the daemon records in `meta` that it set each path, so a user who removes the exclusion (`tmutil removeexclusion`) is not overridden. A configured root is never touched; `docs/disk.md` names `tmutil addexclusion` for it. An exclusion does not stop local snapshots holding freed blocks, which is why free space counts purgeable space.

**Spotlight** (decision 13): **nothing under `~/.reef`**: a hidden directory is already not indexed (D6), and `.metadata_never_index`, which ADR 0073 planned, no longer works in a non-root directory. The bases in the state directory are indexed (D6): every new base is a full tree for `mds` to import. The fix (a `.noindex` suffix on the bases directory) changes the state directory's layout and needs its own measurement of `mds`, so it is a follow-up, [#291](https://github.com/reef-vcs/reef/issues/291), with the measurement below as its baseline (in the PR).

**Measured in the PR**: `mds` and `mds_stores` CPU seconds (`ps -o time`) before and after creating one `large` base, and before and after five workspaces build under `~/.reef`, to record that the second costs nothing and what the first costs.

### Configuration (decision 14)

```toml
[disk]
floor = "auto"                 # the larger of 20 GiB and 5% of the volume; a size; "0" for none
budget = "auto"                # 25% of the workspace volume; a size; "0" for none
measure_interval = "5m"        # the least time between two walks of a dirty workspace, 1m to 24h
exclude_from_backups = true    # macOS: Time Machine's sticky exclusion on ~/.reef, bases and stores
```

`KEYS` and its test gain the section; `reef config show` prints the values with their sources and, for `auto`, what they resolve to. A floor over 50% of the volume or a budget under the floor is refused at start with the key named. Changes take effect at the next start.

### The ADR (decision 15)

**ADR 0076, "Disk accounting: one walk per workspace, a floor that reads statvfs first"**, amending ADR 0073's decisions 7 and 11 with what the experiments showed and the API settled (the departures below). ADR 0073 gains "Amended by 0076" in its header (beside #271's 0074); the index gains the row. #271 did the same for its decisions; a new record keeps 0073 as the user accepted it.

### Threat rows (S-196 to S-199)

| Id | Area | Actor | Severity | Finding | Status |
| --- | --- | --- | --- | --- | --- |
| S-196 | The disk walk | Repository content, an agent's build | Low | A tree with millions of entries or a deep symlink farm makes each walk expensive, starving the blocking pool or the disk. | Bounded: walks never follow symlinks, stop at 2,000,000 entries or 30 s with `incomplete`, run one at a time at lowered priority, at most once per `measure_interval` per workspace, only when dirty. Tests: `disk::walk::caps`, `disk::schedule::*` |
| S-197 | The floor | A same-uid agent | Low | One agent fills the disk (a build, a download) until every other agent's creates are refused; or names another's workspace in a release to free space. | Documented: one uid is one trust domain (ADR 0027). Snapshots, releases, GC, moves and rebases are never refused, so no written work is lost and running tickets continue; the refusal names the largest idle workspace, whose release takes a final snapshot. Tests: `disk_floor::*` |
| S-198 | The free-space figure | The operating system | Low | On macOS the floor counts purgeable space the system may fail to purge in time, so a create allowed by it could meet `ENOSPC`; `statvfs` alone would refuse creates the system could serve. | Accepted: the floor is a margin of 20 GiB or more above zero, the daemon survives `ENOSPC` (R1.23, #229), and snapshots are never refused. `reef daemon status` shows both figures. Tests: `disk::volume::*` |
| S-199 | Time Machine exclusion | Reef itself, the user's setup | Low | Untracked files an agent or person keeps in a workspace (a `.env`, local data) are no longer in Time Machine backups. | Documented: workspaces are caches (design principle 5); tracked work lives in the repository's store, which stays backed up; the exclusion is set once and a user's removal is respected; `exclude_from_backups = false` turns it off; never set on a configured root. Tests: `disk::backup::*` |

The review paragraph: "#272's disk accounting (rows S-196 to S-199) has had no external review. RX.02 reviews the walk's caps and the Time Machine exclusion."

### Measurement and acceptance (decision 16)

**The harness** (`reef-testing/src/fleet/`): reads the daemon's figures, as the issue asks.

- The sampler reads `reef daemon status -o json`'s `disk` every 5 s beside its own `statvfs` and important-usage samples: level, each volume's free, purgeable free and floor, `reef_bytes` by kind.
- After each round, each ticket's figure comes from `reef ws inspect -o json` (`disk`, which the harness already calls for the path); the harness's own walk stays beside it, and `summary.md` shows both with their ratio (they should agree within 10% on unshared bytes, a check on the daemon's figure).
- **G1** reads the daemon: no create refused with `DISK_SPACE_LOW` (refusals now counted by reason, from `ws run`'s error JSON), the level never `low`, free never under the daemon's `floor_bytes` (the harness's own formula stays as a cross-check). **G2** is judged on `statvfs` as before, with the daemon's `reef_bytes` peak beside it. **G4** after release and `ws gc`: the daemon's `reef_bytes` back within 2 GiB of the start, and `statvfs` as before; shared tool caches (`GOCACHE`, yarn's) reported apart and **not counted** (decision 17).
- `--repo synthetic` in `fleet_smoke` asserts the daemon's figures are present and the agreement check passes.

**The gate runs**: the standard VM for fastapi, excalidraw and cli (G1, G2, G4), and `fleet.yml` on the Linux runner for all four including tokio. tokio's VM row stays with #264's pending results PR (it needs about 40 GiB on a host that has about 96 GiB free, under the runbook's 150 GiB rule). Pass: G1, G2 and G4 on both profiles, each from the daemon's numbers; G3 and G5 to G9 reported, unchanged.

**G4 and Go's cache** (decision 17): the baseline's only G4 failures are cli's `GOCACHE` growth (7.9 GiB), which Go keys by workspace path and which outlives every workspace: Reef never sets `-trimpath` (ADR 0073). Recommended: G4 counts what Reef holds (its `reef_bytes` and `statvfs` less the tool caches' growth, which is reported apart, as `docs/fleet-benchmark.md` already reports it), so the gate judges this ticket's work; `here`'s Go hint is not added (Go has no workspace-local build directory to detect).

## Deliverables

### `crates/reef-api/`

- `workspaces.proto`: `Workspace.disk = 24`, `WorkspaceDisk`, `DiskPath`, `DiskStatus`, `DiskLevel`, `DiskReason`, `DiskVolume`, `IdleWorkspace`, `GetWorkspaceResponse.disk_status = 8`, `ListWorkspacesResponse.disk_status = 4`; the reason on `CreateWorkspace`. `daemon.proto`: `GetDaemonStatusResponse.disk = 14`. `stores.proto`: the reason on `BuildStore`. `src/lib.rs`: `DISK_SPACE_LOW`, `API_REVISION` 33, the digest.

### `crates/reef-daemon/`

- `src/disk/` (new): `mod.rs` (`Disk`: the kept figures, the dirty set, the one-walk semaphore, the hourly machine walk, `level()`, `check_floor(volume) -> Result<(), DiskError>`), `walk.rs` (the walk, its classification and caps; `bulk.rs` for macOS's `getattrlistbulk`), `volume.rs` (volumes, `statvfs`, CoreFoundation's important-usage capacity behind `#[cfg(target_os = "macos")]`, the 5 s cache, a test hook injecting free bytes), `deps.rs` (#195's detection), `backup.rs` (the Time Machine xattr, once).
- `src/db/migrations/0019_workspace_disk.sql` (or 0018), `src/db/workspace_disk.rs`.
- `src/config.rs`: `[disk]`, `KEYS`, validation, `config show`.
- `src/workspace/create.rs`: the floor at step 1. `src/store/build.rs`: the floor before `start_build`. `src/workspace/manager.rs`: `inspect` walks through `disk`; `reclaim.rs`: `freed_bytes` from the kept figure; the row deleted at release. `src/snapshot/` (the scheduler): watch events mark the workspace dirty.
- `src/backend/`: `disk_usage` and `changed_files_vs_base` removed (the walk replaces them); `Footprint` kept for its callers.
- `src/error.rs`: `DiskError::BelowFloor` → `DISK_SPACE_LOW`, its metadata and advice.
- `src/service/workspaces.rs`, `src/service/daemon.rs`, `src/service/stores.rs`: the fields.

### `crates/reef-client/`

- `json`: `WorkspaceDisk`, `DiskStatus` (and volumes, idle), `Workspace.disk` (skipped when unset, appended last), `Inspected.disk_status`, `Status.disk`. `brief`: `Workspace.disk` (reduced, only when not `ok`, which the list passes in).

### `crates/reef-vcs/`

- `src/cli/here.rs` (the object, the order of advice, #195's hint, the text line), `src/cli/ws.rs` and `render.rs` (`ws list`'s column, brief and stderr line; `ws inspect`'s block), `src/cli/daemon.rs` (the line and object).

### `web/`

- `src/views/fleet-model.ts`, `Fleet.tsx`, `WorkspaceDetail.tsx`: the column and detail from `Workspace.disk`, the banner; `src/fleet/disk.ts` and its test removed, "Measure again" kept.

### `crates/reef-testing/`

- `src/fleet/sample.rs` (the daemon's `disk`), `run.rs` (refusals by reason, the daemon's per-ticket figure), `report.rs` (G1 and G4 from the daemon, the agreement column), `README.md`.

### Documentation

- **`docs/disk.md`** (new): what is counted and how often, the floor, the budget and the level, what is never refused, `DISK_SPACE_LOW` and what to do, macOS's purgeable space, Time Machine and Spotlight, `[disk]`, the hints, and what a repository can do (the Rust and Go settings).
- `docs/quickstart.md`: `[disk]` under Configuration, one line on the floor. `docs/fleet-benchmark.md`: the daemon's figures in "What is measured", G1 and G4 as decided, the results tables. `docs/threat-model.md`: S-196 to S-199 and the review paragraph. `docs/adr/0076-disk-accounting.md`, indexed; ADR 0073's header. `CHANGELOG.md` Unreleased: Added (`Workspace.disk`, the floor and `DISK_SPACE_LOW`, `[disk]`, `disk_status`, the Time Machine exclusion), Changed (the web Disk column, `ws list`'s DISK column). `README.md`'s Phase 3 section: one line. `docs/dev-tickets/phase-3/README.md`: this brief's row.

## Tests

Sizes: S under a second, M a few seconds. New fixtures use `testing::no_automatic_snapshots`. No assertion on walk speed beyond the caps (the timing-flake rule).

In `crates/reef-daemon/` (unit):

1. (S) `disk::walk::classify`: a fixture workspace and base with an unchanged tracked file, an edited one, a new untracked file, `target/` and `node_modules/` (ignored), a symlink out of the tree, a store mount and a hard link: each lands in the right figure; the symlink is not followed; the store mount is skipped; the hard link counts once; `largest` lists `target` and `node_modules` in order.
2. (S) `disk::walk::caps`: a cap of 100 entries on a 500-entry tree gives `incomplete` and lower bounds; a time cap of zero stops at once.
3. (S, macOS) `disk::bulk::parse`: canned `getattrlistbulk` buffers (a file, a directory, a name at an odd offset, a missing private size) parse; on a real APFS directory the totals equal the stat walk's and `Footprint`'s.
4. (S) `disk::walk::unshared`: an APFS clone of a 1 MiB file has 0 unshared, rising after a write (macOS); an overlay upper's bytes (Linux); a copy's whole tree; unset on reflink.
5. (S) `disk::volume::*`: the floor's `auto` (20 GiB on 100 GiB, 25.6 GiB on 512 GiB), a size, `"0"`; the fast path makes no CoreFoundation call above the floor (a counter through the test hook); the level from injected figures (`low`, `warn` near the floor and over the budget, `ok`).
6. (S) `disk::deps::detect`: npm, pnpm, yarn, bun, uv and poetry layouts with and without their directories; a `package.json` under `node_modules` ignored; at most 8; nothing for `Cargo.lock`.
7. (S, macOS) `disk::backup::*`: the xattr's bytes; set once and recorded; not set again after removal; never on a configured root; off with `exclude_from_backups = false`.
8. (S) `config::tests::disk_section`: defaults, each key, bounds, the floor over 50% and the budget under the floor refused, unknown keys refused.
9. (S) `db::workspace_disk`: insert, update, delete at release, the migration on a schema-17 (or 18) database.

In `crates/reef-daemon/tests/disk.rs` (through `TestDaemon`, free space injected through the test hook):

10. (M) `disk_floor_refuses_creates`: below the floor, `CreateWorkspace` fails with `DISK_SPACE_LOW`, kind `Conflict`, the metadata and `next` naming an idle workspace (one whose lease is not heartbeating and whose snapshot is old, by the test clock) or `reef ws list`; nothing was made on disk (no base, no directory); `BuildStore` is refused the same way, and an already-sealed store still answers.
11. (M) `disk_floor_never_refuses`: below the floor, a snapshot of an edit, a move, a renew, a release (whose final snapshot keeps the edit) and `ws gc` all succeed.
12. (M) `disk_figures`: a workspace that writes 3 MiB into an ignored `target/` and edits a tracked file: after the interval (1 s through the test hook, below the configurable minimum), `ListWorkspaces` shows `build_bytes` ≥ 3 MiB and `tracked_bytes` > 0, `measured_at` moved; with no writes, no further walk happens (a walk counter); `GetWorkspace` with the walk updates the kept figure; a restart keeps the figure and re-measures.
13. (M) `disk_status`: `GetDaemonStatus`'s `disk` and `ListWorkspaces`' `disk_status` with one volume and with the state directory on another (two temporary directories reported as two `st_dev` through the hook); `reef_bytes` sums the kinds; `largest_idle`.

In `crates/reef-vcs/` (`tests/cli.rs`, `tests/here.rs`, `tests/schema.rs`, `tests/mcp.rs`):

14. (M) `here_disk`: with the level `ok`, `here`'s JSON has no `disk` and no `disk_status` (the exact key set as today); with `[disk] floor` above free space, `disk_status` with level `low`, the hint and `next`, and the text within 12 lines; `missing_dependencies` gives the install hint and `next`.
15. (M) `ws_list_disk`: the DISK column; brief elements without `disk` when `ok` (the existing exact-string test unchanged) and with it when `low`; the stderr line only when not `ok`; full JSON always has `disk` for measured workspaces.
16. (S) `ws_inspect_disk` and `daemon_status_disk`: the block and line, JSON objects; `create` below the floor prints the error with `next` and exits 5.
17. (S) the schema test's not-`ok` run; `mcp`'s `list_workspaces` result without `disk` when `ok`; the digest at revision 33.

In `web/`: 18. (S) Vitest: the column from `Workspace.disk` (unshared, fallback, incomplete marker, not measured), the banner, "Measure again" calling `GetWorkspace` once; the e2e smoke's Disk check unchanged in spirit.

In `crates/reef-testing/`: 19. (S) `fleet::report`: G1 from the daemon's floor and reason-counted refusals; G4 with tool caches apart; the agreement column. 20. (M) the fleet smoke: daemon figures present, agreement within 10%.

## Implementation notes

- **Order**: the walk and its tests (1 to 4); volumes and the floor (5, 8, 10, 11); the table, the schedule and the fields (9, 12, 13); the CLI (14 to 17); the web (18); detection (6); Time Machine (7); the harness (19, 20); the Linux container; the gate runs; documents and ADR 0076.
- **Unsafe code**: `getattrlistbulk`, `CFURLCopyResourcePropertyForKey` (declared with the key in a `#[link(name = "CoreFoundation", kind = "framework")]` block; `core-foundation-sys` 0.8.7 lacks both) and `setxattr`, each in one small function with a `SAFETY` comment.
- **The dirty mark** is set where the scheduler receives a `WatchEvent`, before its own filtering, as an atomic flag on the workspace entry; it costs nothing per event.
- **Linux**: the overlay and copy walks, test 4's Linux half and tests 10 to 13 run in the privileged container before pushing (macOS never compiles `cfg(target_os = "linux")` code).
- **Rebase onto #290**: `GetDaemonStatusResponse.disk` is 14 because #290 takes 13; the CLI's `daemon status` text gains its line after #290's `build` line; `store/build.rs` gets the floor check before #290's environment code. If R3.16 adds no migration, the file is `0018_workspace_disk.sql`.
- **Builds** of Reef itself use `CARGO_INCREMENTAL=0` and `CARGO_PROFILE_DEV_DEBUG=line-tables-only`.

## Performance expectations, and the resource criterion

| Measurement | Expect |
| --- | --- |
| The floor check at create | < 10 µs above the floor (`statvfs` only); < 50 ms below it on macOS (one CoreFoundation call, then cached) |
| One walk | ~8 µs per entry on APFS (D2); a 40 GiB Rust `target/` 0.3 s; the `large` preset 0.8 s |
| Background walks | one at a time; a building workspace at most every 5 min; an idle one never |
| `ListWorkspaces` | no walk: one table read; < 1 ms more for 20 workspaces |
| The web page | no browser walks: the Disk column fills with the list |
| The daemon's memory | one row per active workspace |

The resource criterion is #264's G1, G2 and G4 on the standard profile and the Linux runner, from the daemon's figures (decision 16), the numbers in the PR and in `docs/fleet-benchmark.md`.

## Acceptance criteria

- [ ] `ListWorkspaces` and `GetWorkspace` carry each active workspace's `disk`, kept by the daemon, refreshed when dirty at most every `measure_interval`, with `incomplete` at the caps (tests 1 to 4, 12). Closes #146.
- [ ] Below the floor, creates and store builds are refused with `DISK_SPACE_LOW`, its hint, metadata and `next`; snapshots, releases, GC, moves and renewals are not (tests 10, 11, 16).
- [ ] On macOS free space counts purgeable space, reading `statvfs` first (test 5).
- [ ] `reef here` and `ws list` show the level only when not `ok`; `ws inspect` and `daemon status` always show the figures; the web column reads the daemon's figure (tests 14 to 18).
- [ ] A workspace missing its dependencies gets the install hint (test 6, 14).
- [ ] `~/.reef`, bases and stores are excluded from Time Machine once (test 7); the `mds` measurement is in the PR.
- [ ] G1, G2 and G4 pass on the standard VM (fastapi, excalidraw, cli) and the Linux runner (all four), from the daemon's numbers; results in `docs/fleet-benchmark.md`.
- [ ] ADR 0076 and S-196 to S-199 written; `just ci`, `just doc`, `just api` pass; the web bundle within budget.

## Verification

```sh
export CARGO_TARGET_DIR=/Users/arsood/Projects/ReefVCS/target-272 CARGO_INCREMENTAL=0 CARGO_PROFILE_DEV_DEBUG=line-tables-only
cargo nextest run -p reef-daemon disk:: config::tests::disk_section db::workspace_disk
cargo nextest run -p reef-daemon --test disk
cargo nextest run -p reef-vcs --test cli --test here --test schema --test mcp
cargo nextest run -p reef-vcs --test fleet_smoke
just web-check
# By hand, against a scratch daemon:
reef daemon status                                   # the disk line
reef ws list; reef ws inspect <id>                   # DISK column, the block
# Below the floor (a scratch daemon with [disk] floor = "10TiB"):
reef ws create --repo <id> -o json                   # DISK_SPACE_LOW, next
reef here                                            # disk_status, hint
# The gate (VM, host free space checked first; then fleet.yml):
reef-fleet run --repo fastapi,excalidraw,cli --profile standard --out ~/fleet/272
reef-fleet report ~/fleet/272 --gate
gh workflow run fleet.yml --ref <branch>
just api && just api-breaking /Users/arsood/Projects/ReefVCS/reef/.git#branch=main
just doc && just ci
```

## Out of scope

- Eviction and `evictable_bytes` (#274), clone stores (#273).
- Refusing anything for the budget; refusing moves and rebases.
- Spotlight exclusion of the bases (#291, with this ticket's `mds` measurement).
- `FIEMAP`-based unshared bytes on reflink filesystems.
- `reef init`'s detection message (#195's own issue stays open for it).
- The executor's job directories (`~/Library/Caches/reef/executor`), outside Reef's view; counted by a follow-up if they grow.

## Risks

- **The walk's classification lags a snapshot**: a file created since the last snapshot counts as build output until the next one. Documented; the figure is "as of the last snapshot", and a snapshot follows within the debounce.
- **Purgeable space not purged in time** (S-198): mitigated by the floor's margin and `ENOSPC` survival.
- **Watchman workspaces** may miss build-only changes: mitigated by the 3 × interval refresh while heartbeating.
- **`ws list`'s stderr line** may be dropped by a harness that discards stderr: the brief elements' `disk` and `here`'s object carry it as data too.
- **The token benchmark's `ws list -o text`** gains a column (about 8 bytes a row): rerun the scenario and record the number; the 7× live margin covers it.

## Choices this brief made, and why

- **`statvfs` first, CoreFoundation near the floor**, because D1 shows the second costs 10,000 times the first, and the floor takes the larger of the two.
- **Classification by the working copy's own `tree_state`**, because jj already decided what is tracked and an ignore matcher would cost more and could disagree.
- **Dirty-driven walks**, because build output changes without snapshots but always with watch events (D2's cost makes a walk per 5 min of activity negligible).
- **A table, not memory**, so restarts cost no walks and #274 needs no migration.
- **The level only when not `ok`**, and `ws list`'s shape kept, because agents pay for every byte and every parser of `ws list` reads an array.
- **No Spotlight work under `~/.reef`**, because D6 shows it is not indexed and the planned marker no longer works.

## What later tickets inherit

- **#274 (E)**: the table's eviction columns, `largest`, the budget and `level`, `largest_idle`; it adds `evictable_bytes` and evicts when the level is `warn` or `low`.
- **#273 (D)**: clone-store mounts count as build output; its store builds pass the floor check.
- **RX.06**: G1, G2 and G4 from the daemon's figures.
- **#291**: Spotlight and the bases.

## Departures from ADR 0073

Recorded in ADR 0076; ADR 0073's text stays, with "Amended by 0076" in its header.

- `Workspace.disk` is field 24 (23 went to #259's `behind_trunk`); `DiskStatus` also rides on `ListWorkspacesResponse` and `GetWorkspaceResponse` (`disk_status`), so `here` and `ws list` need no second call.
- `evictable_bytes` is left to #274, which declares what is evictable; `WorkspaceDisk` gains `largest` and `missing_dependencies`.
- Usable free space reads `statvfs` first and CoreFoundation's important-usage capacity only near the floor.
- Measuring is driven by watch events (dirty, at most every `measure_interval`, default 5 min) instead of "after a snapshot settles, and every 15 minutes".
- Moves and rebases are not refused below the floor; `next` falls back to `reef ws list`, not `reef ws gc`.
- Spotlight: nothing under `~/.reef` (not indexed; `.metadata_never_index` no longer works); the bases are a follow-up. Time Machine also excludes bases and stores, once.
- `ws list` keeps its array shape: the level goes to stderr, and brief elements carry `disk` only when not `ok`.
- Numbers: API revision 33, schema 19 (or 18), ADR 0076, S-196 to S-199 (four rows; the ADR planned three).

No departure from `dev-phases.md` beyond G4's tool caches (decision 17), which `docs/fleet-benchmark.md` records if accepted.

## Decisions for the user

**Answered 2026-11-13: all eighteen accepted as recommended.** One condition on decision 5: before dirty marking relies on watch events, the implementer verifies on macOS and in the Linux container that the watcher delivers events under ignored directories such as `target/` (R1.30 narrowed notify's scans); if it does not, a fallback (a periodic look at the largest untracked directories, or a re-walk of workspaces whose figure is older than a bound) is added and recorded under the departures. The Spotlight follow-up (decision 13) is [#291](https://github.com/reef-vcs/reef/issues/291). The recommendations as they were put:

**Group 1: scope and the figure**

1. **Size and scope.** M, top end: the walk, the table, the floor and budget, the API, four CLI outputs, the web column, #195's hint, Time Machine, the harness and the gate runs. *Recommended*: as stated. Alternatives: leave the web column and Time Machine to follow-ups (M); add `FIEMAP` on reflink.
2. **Classification.** Tracked vs build output by the working copy's own `tree_state` (as of the last snapshot). *Recommended*: as stated (jj already decided; no second ignore matcher). Alternative: match `.gitignore` in the walk (exact between snapshots, slower, may disagree with jj).
3. **The walk.** `getattrlistbulk` on macOS (D2: 1.3–1.7× a stat-plus-private walk), caps 2,000,000 entries or 30 s, lowered priority. *Recommended*: as stated. Alternative: `walkdir` plus a private-size call per file (one code path; slower).
4. **`evictable_bytes`.** Left to #274, which declares what is evictable; `largest` (top three untracked directories) added now. *Recommended*: as stated. Alternative: the field now, always 0 until #273/#274.
5. **Cadence.** Dirty from watch events (ignored paths included), walked at most every `measure_interval` (5 min), one background walk at a time; `GetWorkspace` walks on request and updates it. *Recommended*: as stated. Alternative: ADR 0073's after-snapshot plus every 15 min (misses build-only growth between snapshots).
6. **Persistence.** A `workspace_disk` table (schema 19, or 18) with #274's four eviction columns from the start. *Recommended*: as stated (restarts cost no walks; #274 needs no migration). Alternatives: C's columns only, #274 adds its own; memory only, re-measured at start.

**Group 2: the floor and the budget**

7. **The floor.** `auto` = max(20 GiB, 5%); usable free = `statvfs`, or on macOS the larger with the important-usage capacity, read only near the floor (D1); refuses `CreateWorkspace` and `BuildStore` with `DISK_SPACE_LOW` (Conflict, exit 5); never snapshots, releases, GC, renewals, publishes, **moves or rebases**; `next` = release the largest idle workspace, else `reef ws list`. *Recommended*: as stated. Alternatives: also refuse moves and rebases (they may materialize a base); `next` falls back to `reef ws gc` (ADR 0073's text; frees little).
8. **Budget and level.** Budget `auto` = 25% of the workspace volume, soft; level `low` under the floor, `warn` over the budget or under twice the floor, else `ok`. *Recommended*: as stated (ADR 0073).

**Group 3: what is shown**

9. **API.** `Workspace.disk = 24` (`WorkspaceDisk`), `disk_status` on `GetWorkspaceResponse` and `ListWorkspacesResponse`, `GetDaemonStatusResponse.disk = 14`, reason `DISK_SPACE_LOW`, no new RPC. *Recommended*: as stated (`here` and `ws list` need no second call). Alternative: `DiskStatus` only in `GetDaemonStatus`, which the CLI calls too when it needs the level.
10. **Agents and people.** `here`: `disk_status` and the workspace's `disk` only when not `ok`, a `low` hint before `behind_trunk`. `ws list`: array shape kept; brief elements carry `disk` only when not `ok`; one stderr line with the level and `next`; text gains a DISK column. `ws inspect` and `daemon status` always show figures. Web: the column from the daemon's figure, the browser walks removed, a banner when not `ok`. *Recommended*: as stated. Alternatives: no stderr line (the level only through `here` and brief elements); no text DISK column (the token benchmark's text form unchanged); `ws list` as an object (breaks every parser of the array).
11. **#195 and the Rust hint.** Detect npm, pnpm, yarn, bun, uv and poetry layouts without their dependency directory; `here`'s hint names the install command as `next` and mentions stores in the docs; when `target/` exceeds 2 GiB at `warn` or `low`, the hint recommends `CARGO_INCREMENTAL=0` and line-tables-only. *Recommended*: as stated (a symlink store's `npm ci` fails, #278, until #273). Alternatives: `next` is the `reef.toml` store block (ADR 0073's wording); no Rust hint.

**Group 4: backups, indexing, records**

12. **Time Machine.** Sticky exclusion, unprivileged, set once (a user's removal respected), on `~/.reef` and on the state directory's bases and stores; never a configured root; `[disk] exclude_from_backups`. *Recommended*: as stated (all are caches; tracked work stays in the backed-up repository). Alternatives: `~/.reef` only (ADR 0073's text); none, documented only.
13. **Spotlight.** Nothing under `~/.reef` (D6: a hidden directory is not indexed, and `.metadata_never_index` no longer works); a follow-up issue for the bases, which are indexed, with this ticket's `mds` measurement. *Recommended*: as stated. Alternative: rename the bases directory `bases.noindex` here (a layout change and its migration).
14. **Configuration.** `[disk] floor`, `budget`, `measure_interval`, `exclude_from_backups`. *Recommended*: as stated.
15. **ADR.** ADR 0076 amending ADR 0073's decisions 7 and 11 (as #271's 0074 did 9 and 10). *Recommended*: as stated. Alternative: amend ADR 0073 in place.

**Group 5: acceptance and numbers**

16. **Acceptance.** G1, G2 and G4 from the daemon's own figures (refusals by reason, the daemon's floor and level, `reef_bytes`), on the standard VM for fastapi, excalidraw and cli and on the Linux runner for all four; tokio's VM row stays with #264's pending results PR (host disk). *Recommended*: as stated. Alternative: also tokio on the VM after freeing host disk to 150 GiB.
17. **G4 and tool caches.** G4 counts what Reef holds; shared tool caches' growth (cli's `GOCACHE`, 7.9 GiB in the baseline, which Reef cannot affect without setting `-trimpath`) reported apart. *Recommended*: as stated (the gate judges this ticket's work). Alternative: G4 unchanged, and cli's row keeps failing for Go's cache.
18. **Numbers.** API revision 33, schema 19 (18 if R3.16 has none), ADR 0076, threat rows S-196 to S-199. *Recommended*: as stated.

## Departures in the implementation

- **The watch-event check (the condition on decision 5)** held: `disk::disk_figures` writes build output under an ignored `target/` and waits for the background walk that only a watch event triggers, on macOS (`FSEvents`) and in the Linux container (inotify). Events under `[snapshot] exclude` paths are dropped before the scheduler, and Watchman's subscriptions may omit ignored paths, so the fallback is general: **every held workspace** (a live presence entry) is walked every three intervals, written to or not, not only Watchman workspaces.
- **`Backend::disk_usage` stays** for the backend conformance suite; the daemon's answers (`disk_usage`, `Workspace.disk`, `freed_bytes`) come from the walk. `disk_usage` is now set even when the workspace's base is no longer cached (its tracked files then count in full), where it was unset.
- **A file whose APFS private size cannot be read counts its allocated size** in `unshared_bytes`, instead of unsetting the whole figure as `Footprint` does.
- **No start-time refusal of `floor` over 50% of the volume or `budget` under the floor**: a floor above the free space is how a test (and a person) stops new work, and the budget measures what Reef holds, not free space, so the two do not compare. `measure_interval` is checked (1 minute to 24 hours).
- **The machine figures** (stores and the state directory's own files) are walked hourly and at start, not also after each GC.
- **`reef here`'s `next` at `low` with nothing idle** is `reef ws list`, not "this workspace" (releasing the agent's own workspace mid-ticket is not advice).
- **The web UI still reads the selected workspace** (`GetWorkspace` without a walk) for its snapshots and the other workspaces on its change; only "Measure again" walks, and then the list refreshes. The background walks of every workspace (#146) are gone.
- **`DiskStatus` and its messages live in `workspaces.proto`**: `daemon.proto` imports it, and the other way round would be a cycle. The four new message fields are boxed in the generated Rust (`large_futures`).
- **Request walks are serialized, not joined**: a `GetWorkspace` that arrives during a walk of the same workspace waits for it and walks again, so its answer covers what was written before it asked; at most two request walks run at once. A walk clears the dirty mark when it starts, so a write during the walk marks it again.
- **Idle excludes recent writes**: a workspace with a watch event within the idle period (a build writing output without snapshots) is never the largest idle one.
- **Moves log a warning below the floor**; rebases update workspaces without a move and log nothing.
- **#195's detection leaves Poetry out** (its environment is outside the project by default) and counts Yarn's Plug'n'Play (`.pnp.cjs`) as installed; `next` names the directory in full, since `reef here` runs from any subdirectory, and Yarn's frozen flag follows `.yarnrc.yml` (2 and later) or not (classic).
- **A workspace whose directory is gone has no figure**: the walk fails rather than recording zeros.
- **Numbers**: API revision 33, schema 19 (`0019_workspace_disk`), as planned, on `main` after #271 (revision 31) and R3.16 (revision 32, `0018_history`).
