# 0020. Local backup format

- Status: accepted
- Date: 2026-10-22
- Ticket: R1.18

## Context

The design's durability section sorts the daemon's state into classes. Two are authoritative:

- each repository's jj state: objects, operations, views, and operation heads;
- Reef's SQLite metadata: repositories, workspace records, leases, the reclaim log, and the snapshot index.

The rest are caches rebuilt from those two: workspace directories, base caches, working-copy state, and jj's index. Phase 1's exit criterion is that a backup taken while agents are writing restores to a daemon that serves the same changes and operation log, with no workspace lost and no lease reclaimed on the first tick.

What jj-lib 0.46.0 writes under `.jj/repo`:

- `op_store/operations/<id>` and `op_store/views/<id>`: one immutable file per object.
- `op_heads/heads/<id>`: an empty file per head. A new head is written after the operation it names, and only then are the old heads removed.
- `store/`: the backend. It holds `store/git` for an internal git repository, or `store/git_target` naming a colocated `.git`. It also holds `store/extra/`, the git backend's change-id table: immutable table files plus a `heads/` file naming the current one.
- `index/`: `type`, `op_links/`, `segments/`, `changed_paths/`. All of it except `type` is rebuildable, and jj rebuilds it into empty directories on load.
- `workspace_store/` and `submodule_store/`: small mutable files.

Every store directory has a `type` file that jj reads on load. Git's loose objects and packs are immutable. Only `git gc` rewrites or removes them, and R1.17 runs it under the maintenance lock.

Reef writes every jj operation from the repository's thread (ADR 0012), so pausing that thread stops operation writes. A `jj` CLI on the user's root is outside that lock. SQLite runs in WAL mode with the daemon as its only writer, and its online backup API copies a live database page by page, restarting a step whose pages change.

## Decision

1. **A backup is a directory the caller names**, absolute, outside the state directory, and new or empty. Its layout:

   ```text
   manifest.json                written first; rewritten last with "complete": true
   reef.sqlite3                 the whole database, through SQLite's online backup API
   repos/<id>/repo/             .jj/repo, without the index's contents
   repos/<id>/git/              a colocated repository's .git, without index, logs/, hooks/, *.lock
   repos/<id>/heads.json        the operation heads, written last
   ```

   The manifest records:
   - `version` (1), `created_at`, `daemon_instance_id`, `reef_version`, `jj_lib_version`, and `state_dir`;
   - per repository: id, root, `.jj/repo` path, colocated, op heads, files, bytes, and how long its thread was paused;
   - the database's pages and bytes;
   - `complete`.

   `repos/<id>/repo/` opens in jj as it is: `index/` holds its `type` file and empty directories, and `op_heads/heads/` holds the heads in `heads.json`. *(addition: the brief said "without `index/`", but jj will not load a repository without `index/type`.)*

2. **Order.** The backup holds the maintenance lock throughout, so GC, quiesce, and another backup are refused (`Unavailable`).
   1. `meta.last_backup_started_at`.
   2. A passive WAL checkpoint, then the online backup from a second, read-only connection: `backup.sqlite_pages_per_step` pages a step, 10 ms apart. Heartbeats keep writing.
   3. Per repository, the immutable operation and view files and the extra-metadata tables are copied first, without the pause. Then, with its thread paused, the op heads are read, and then the extra-metadata heads. After both reads, the pause copies the immutable files written since the first pass, the workspace and submodule stores, and every `type` file. Reading heads before the files they reach makes the copy consistent even against a writer outside the pause. *(addition: the brief's list missed `store/extra/`, without which no change id resolves; the first pass took the pause on the medium fixture from 106 ms to 6 ms.)*
   4. Without the pause, the git repository is copied: refs, `packed-refs`, `HEAD`, and `config` first, objects after. Every copied ref therefore names an object the copy has. Mtimes are preserved, so `git gc --prune` sees the same ages after a restore. Objects written meanwhile are harmless extras, because jj writes objects before the operations that name them.
   5. Each repository's heads go into `repo/` and `heads.json`. Once every repository is done, the manifest is rewritten with `complete: true` and `meta.last_backup_at` is set.

   The database is copied before the jj state. Every workspace row therefore names commits and operations that the copied heads reach.

3. **Quiesce is a standalone RPC** for filesystem snapshots of the state directory. `Quiesce { repo, max_ms }` takes the maintenance lock, holds off automatic snapshots of every workspace (watcher events still mark them dirty) and lease expiry ticks, pauses the repository threads after their queued jobs, starts refusing `Heartbeat` (`Unavailable` with the resume time), and only then checkpoints the WAL (`TRUNCATE`). A request that writes a row outside the repository threads (`Register`, the start of a `Create`) can still add to the WAL afterwards, so the filesystem snapshot must take the whole state directory, the `-wal` and `-shm` files with the database. A named repository that is not open is refused rather than reported held. Snapshot, Create, Release, Move, and Gc wait behind the paused threads up to their own deadlines. `Resume { token }` ends it, and so does the quiesce itself at `min(max_ms or quiesce_max, quiesce_max)`. A crashed caller therefore cannot wedge the daemon. On resume every active lease gets at least `lease.grace` from now. Shutdown resumes first.

4. **Restore is offline.** `reef restore <dir> [--into] [--repo] [--root] [--force]` runs in-process and takes the state directory's `daemon.lock` (`Unavailable` while a daemon runs). It checks everything before it writes anything, and refuses:
   - a manifest without `complete`;
   - a format or schema newer than the binary;
   - an existing database, unless `--force`;
   - a repository whose live op heads are not all in `heads.json`, unless `--force`.

   It then writes, in order:
   - **Each `.jj/repo` copy, staged** beside its target, so a failed copy changes nothing.
   - **The database.** The old one is renamed aside with its `-wal` and `-shm`, so SQLite cannot replay them over the copy.
   - **Each staged copy, renamed into place**, the live `.jj/repo` renamed aside first. What was replaced (the old database, each old `.jj/repo`) is kept under a `.replaced-<nanos>` name and listed in the report: the way back from a `--force`. Nothing removes it. *(addition)*
   - **A colocated `.git`.** If missing, it is created from the copy, without git's index (jj's next colocated command writes it). Otherwise the copy's objects are added (none removed), and its loose refs, `packed-refs`, `HEAD`, and `config` are written over. Loose refs the copy lacks are left alone; a ref only in the live `packed-refs` is lost with it.
   - **The `default` working copy, with `--root` on a directory that has none.** It is recreated through `LocalWorkingCopy::init`, with the `type` file written by hand, and its commit's files are checked out.
   - **The restored database.** With `--root`, the repository's new paths. With `--into` another state directory, every path under the old one, moved. R1.15's restart grace on every active lease. `last_backup_at` set to the backup's time. `restored_from`, `restored_at`, and `rematerialize_pending`.

5. **Rematerialization on demand.** With `rematerialize_pending` set, the next start rematerializes every active workspace instead of reclaiming one whose directory is gone:
   - It uses the jj workspace's commit in the restored view, or the row's commit if the view has none.
   - The working-copy state always goes, even when it survived: after a `--force` restore it can name an operation the restore took back, and jj's staleness check then fails every snapshot for good. *(addition)*
   - A missing directory is built from the base of that commit's tree, its new working copy seeded from the base's state, and the row records the commit and base. A directory still there is kept as it is, with its base (an overlay's lower layer is that base), and its new working copy is reset to the commit, so the next snapshot records whatever is on disk, edits since the backup included.
   - The jj workspace gets a new working copy (no operation), or is created again on the commit if the view lacks it.
   - One that fails, or whose repository is not open, is left unloaded, not reclaimed, and the flag stays set until every one succeeds.

   A new directory uses the commit's tree, not the row's original `base_tree_id`. A directory built from the original base under a working copy recorded at a later commit would snapshot as a revert of everything since. *(addition)*

6. **Retention is a GC floor.** R1.17's `keep_newer` widens the undo window to `backup.retention` while `last_backup_at` is within it. Nothing a recent backup references is pruned, so restoring that backup never meets a missing object.

## Consequences

- The jj part of a backup restores by file copy alone. `repos/<id>/repo` is a `.jj/repo`: copied over a root's, `jj op log` works there. A backup is also inspectable with jj, without Reef.
- jj's index is never backed up and is rebuilt on first load. The first open after a restore costs a reindex.
- A restored workspace whose directory was lost loses edits made after its last snapshot before the backup. That loss is bounded by the watcher's debounce: directories are caches. One whose directory survived keeps them, and its first snapshot rehashes every file.
- `--force` is the only way to go backwards in time. It sets aside operations and database rows newer than the backup; they stay on disk under `.replaced-<nanos>` names until someone removes them. The quickstart (R1.24) says so.
- The pause per repository covers what was written since the first pass, plus a listing of the operation store: never the objects. On the medium fixture with 204 operations it is 6 ms.
- A `git gc` someone runs by hand in a colocated repository during a backup is outside the maintenance lock, and could rewrite packs mid-copy. That is documented, not prevented.
- A backup is restartable only from the start; Phase 1 has no incremental backup. Stores and base caches are not backed up. A restored workspace whose store is missing is marked `store-missing` until `reef store build`.

## Alternatives considered

**`tar` of the state directory with the daemon stopped.** Simple and complete, but it stops every agent for the length of the copy, and it covers only state under the state directory, not each repository's `.jj/repo`. It stays the documented fallback.

**Backing up working-copy state and workspace directories too.** They are caches by design. Copying them multiplies the backup by the number of workspaces and still loses edits since the last snapshot. Rematerializing from the restored commit gives the same result.

**Copying the SQLite file.** It is unsafe with WAL and a live writer: a copy can catch the database and its WAL at different moments. The online backup API exists for this.

**Holding the repository thread for the whole copy, objects included.** It is simpler to reason about, but it stops every snapshot for minutes on a large colocated repository. Reading heads before the immutable files they reach gives the same consistency with a pause of milliseconds.
