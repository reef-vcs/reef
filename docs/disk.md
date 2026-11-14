# Disk

Workspaces are copy-on-write, so the files Reef gives an agent cost almost nothing. What fills a laptop's disk is what builds add afterwards: `target/`, `node_modules`, `.venv`. Reef keeps a figure for every workspace, refuses new work before the disk fills, and says so early ([#272](https://github.com/reef-vcs/reef/issues/272), [ADR 0076](adr/0076-disk-accounting.md), which amends [ADR 0073](adr/0073-laptop-resource-budget.md)).

## What a workspace costs

`reef ws list` has a DISK column, `reef ws inspect` a `disk` line, and `ListWorkspaces` and `GetWorkspace` a `Workspace.disk`:

```text
$ reef ws inspect 6f1c
...
disk:             1.2 GiB unshared; tracked 40 KiB, build output 1.2 GiB (target 1.1 GiB, web/node_modules 80 MiB); 12034 entries; measured 2026-11-13T10:00:00.000Z
```

| Figure | Meaning |
| --- | --- |
| `tracked_bytes` | Files the working copy tracks that differ from the base, at their allocated size |
| `build_bytes` | Files the working copy does not track: ignored build output and installed dependencies, and new files not yet snapshotted |
| `unshared_bytes` | What releasing the workspace frees at once. On APFS, each file's private size (an unedited clone shares every block with its base, so it costs nothing); on overlayfs, the upper directory; on the copy backend, every file. Unknown (`null`) on reflink filesystems, where `tracked_bytes + build_bytes` is the bound |
| `largest` | Up to three of the largest untracked directories, such as `target` or `web/node_modules` |
| `missing_dependencies` | Directories with a lockfile and nothing installed (below) |
| `files` | Entries walked, directories included |
| `measured_at`, `incomplete` | When the walk ended; whether it stopped at a cap, so every figure is a lower bound |

Tracked and untracked are told apart by the working copy as of its last snapshot, not by matching `.gitignore` again: what jj tracks is what counts as tracked.

### How often

The daemon walks a workspace's directory, never following a symlink and skipping store mounts, at most 2,000,000 entries or 30 seconds, on a thread of its own at low priority. On macOS one `getattrlistbulk` call per directory reads each entry's sizes and APFS private size; a 40 GiB Rust `target/` of 37,000 files takes about 0.3 s.

- **When it is written to.** Any file event in the workspace, ignored paths included (a build writes no snapshot, but it writes files), marks it; a marked workspace is walked once `[disk] measure_interval` (5 minutes) has passed since its last walk. An idle workspace is never walked.
- **When it is held.** A workspace whose holder is heartbeating (`ws run`, a provider adapter, `reef mcp`) is also walked every three intervals, written to or not: paths under `[snapshot] exclude` and Watchman's subscriptions send no events.
- **When asked.** `reef ws inspect` and `GetWorkspace` without `skip_disk_usage` walk now and keep the figure.
- One background walk runs at a time, machine-wide; at most two walks for requests, and never two of one workspace at once. A new workspace starts at zero, measured at its create: an unedited clone costs nothing. After a restart, every active workspace is walked once, in the background.

The figures are kept in the daemon's database (`workspace_disk`), so `ws list` and the web UI read them without walking anything.

## How full the disk is

`reef daemon status` always shows it:

```text
disk:              ok, 96 GiB free (141 GiB with purgeable) of 926 GiB, floor 46 GiB (workspaces and state); Reef 12 GiB of a 232 GiB budget
```

and `GetDaemonStatusResponse.disk`, `ListWorkspacesResponse.disk_status` and `GetWorkspaceResponse.disk_status` carry it as data: each volume Reef writes to (the workspace root's and, if different, the state directory's) with its free space and floor, what Reef holds by kind (workspaces, bases, stores, its own state), and the largest idle workspace.

**Free space counts purgeable space on macOS.** `statvfs` says how much is free; macOS also frees caches and Time Machine's local snapshots when a write needs the room, which its "available for important usage" capacity counts. A released workspace's blocks often sit in a local snapshot for a day, so `statvfs` alone would refuse creates the system could serve. Reef reads `statvfs` first (microseconds) and asks for the purgeable figure only near the floor (10 to 40 ms).

**The level** is `low` when free space on either volume is under the floor, `warn` when it is under twice the floor or Reef holds more than its budget, and `ok` otherwise.

| Setting | Default | Meaning |
| --- | --- | --- |
| `floor` | `auto`: the larger of 20 GiB and 5% of the volume | Under it, creates and store builds are refused |
| `budget` | `auto`: 25% of the workspace root's volume | Over it, Reef warns; it never refuses anything |

## What is refused, and what never is

Below the floor, **`CreateWorkspace` and `BuildStore`** (and so `reef ws create`, `ws run`, `ws provider create`, `reef store build`) fail with reason `DISK_SPACE_LOW`, kind `conflict`, exit 5, before anything is made:

```json
{"error": {"kind": "conflict", "reason": "DISK_SPACE_LOW",
  "message": "18 GiB free on the volume holding /Users/me/.reef, under Reef's floor of 24 GiB",
  "hint": "New workspaces and store builds are refused until space is freed. Releasing a workspace takes a final snapshot first, so nothing written is lost. The largest idle one is 6f1c... (bot@host, 9.1 GiB).",
  "next": ["reef", "ws", "release", "6f1c..."],
  "metadata": {"volume": "/Users/me/.reef", "free_bytes": "...", "floor_bytes": "...", "largest_idle": "6f1c...", ...}}}
```

An idle workspace is active, has no live presence entry (no heartbeating holder, no running `ws run` command), and has had neither a snapshot nor a file event for 30 minutes. With none idle, `next` is `reef ws list`, whose DISK column shows what each costs.

**Never refused**: snapshots, releases, lease expiry, garbage collection, renewals, publishing, moves and `reef rebase`. Written work is always saved, and releasing is how space comes back. A sealed store still answers a `reef store build` below the floor.

## What agents see

Nothing, while the level is `ok`: the outputs agents read every turn stay as they were. Otherwise:

- **`reef here`** adds `disk_status` and the workspace's `disk`, a `disk:` line in text, and its hint says so: a `low` disk comes right after divergence, attention and an expiring lease, with `next` releasing the largest idle workspace. When the workspace's `target` is over 2 GiB it adds that `CARGO_INCREMENTAL=0` and line-tables-only debug info cut Rust build output about fourfold (Reef never sets them: they change what a build produces).
- **`reef ws list`** prints one line on stderr (`reef: disk low: 18 GiB free (floor 24 GiB); new workspaces are refused; next: reef ws release 6f1c...`) and adds each workspace's `disk` (its `unshared_bytes`, else `tracked_bytes` and `build_bytes`) to the brief JSON it prints on a pipe. Its JSON stays an array.
- **MCP's `list_workspaces`** returns the same brief elements; `get_workspace` always has the figure.
- **The web UI's** Disk column reads the daemon's figure and walks nothing; "Measure again" asks the daemon to walk that workspace now. A notice heads the workspaces page while the level is not `ok`.

## Dependencies not installed (#195)

A new workspace has the repository's tracked files and nothing a build or an install made. When a directory tracks a `package.json` with a lockfile (`package-lock.json`, `npm-shrinkwrap.json`, `pnpm-lock.yaml`, `yarn.lock`, `bun.lock`, `bun.lockb`) but has no `node_modules` (nor Yarn's `.pnp.cjs`), or a `pyproject.toml` with `uv.lock` but no `.venv`, `reef here` says so with the install command for that directory, in full, as `next` (`npm ci --prefix <workspace>/web`, `pnpm install --frozen-lockfile --dir …`, `uv sync --frozen --directory …`). Poetry is left out: its environment lives outside the project by default. Each workspace installs its own; a [store](quickstart.md#shared-dependency-stores) shares one across workspaces, read-only until clone stores land (#273). `Cargo.lock` and `go.sum` are not reported: their build directories appear at the first build.

## Time Machine and Spotlight

On macOS the daemon marks `~/.reef` (the default workspace root), the base cache (`<state>/repos/<id>/bases`) and the stores (`<state>/stores`) excluded from Time Machine, with the same attribute `tmutil addexclusion` writes: no privilege needed, and nothing there is worth backing up. Tracked work lives in the repository's own store, which stays in your backups with the repository, as do the daemon's database and evidence. Each directory is marked once; if you remove the exclusion (`tmutil removeexclusion ~/.reef`) Reef does not put it back. A configured `[workspace] root` is never marked; `tmutil addexclusion <dir>` does it. `[disk] exclude_from_backups = false` turns this off.

An exclusion does not stop a local snapshot holding a released workspace's blocks for a day; that space is purgeable, and the floor counts it.

Spotlight does not index `~/.reef`: it is a hidden directory. The bases in the state directory are indexed; [#291](https://github.com/reef-vcs/reef/issues/291) tracks that.

## Configuration

```toml
[disk]
floor = "auto"                 # the larger of 20 GiB and 5% of the volume; a size such as "30GiB"; "0" for none
budget = "auto"                # 25% of the workspace root's volume; a size; "0" for none
measure_interval = "5m"        # the least time between two walks of a workspace written to; 1m to 24h
exclude_from_backups = true    # macOS: Time Machine exclusion of ~/.reef, bases and stores
```

A floor larger than the free space refuses every create, which is a way to stop new work on a machine. Changes take effect at the next daemon start.

## What a repository can do

The largest savings measured ([fleet benchmark](fleet-benchmark.md#what-a-repository-can-do-today)) are a repository's own settings, which Reef recommends and never sets: `CARGO_INCREMENTAL=0` and `debug = "line-tables-only"` for Rust, `-trimpath` for Go.
