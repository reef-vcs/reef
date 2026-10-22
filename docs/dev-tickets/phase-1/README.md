# Phase 1 tickets

Source of truth for scope: [`docs/dev-phases.md`](../../dev-phases.md) (Phase 1 tables) and [`docs/design.md`](../../design.md) (Workspaces, Architecture, Durability and backup, Scale). These files expand each Phase 1 row into an implementation brief. They are written for an implementing agent, not for humans: every decision that could be made in advance has been made here, and the brief says so. If a brief and `dev-phases.md` disagree on scope, the brief wins for the implementation and the disagreement is fixed in `dev-phases.md` in the same PR. Where the briefs already deviate from `dev-phases.md`, the deviation is listed under "Departures from dev-phases.md" below and `dev-phases.md` was updated when the briefs were written.

Phase 0 is complete (R0.01 to R0.09 merged). What it left behind, and what every Phase 1 brief builds on: `reef_lib::jj` (open, snapshot, op log, undo over jj-lib 0.46.0, ADR 0008), `reef_lib::error::{ErrorKind, Classify}` and `reef_lib::telemetry::fields` (ADR 0009), `reef_daemon::logging`, `reef_testing::synth` (R0.05), the criterion harness in `crates/reef-lib/benches/` (R0.06, `docs/benchmarks.md`), and CI on `ubuntu-24.04` and `macos-15` (R0.03).

## Tickets

| ID | File | Size | Depends on | Unblocks |
| --- | --- | --- | --- | --- |
| R1.01 | [R1.01-workspace-schema.md](R1.01-workspace-schema.md) | S | R0.04 | R1.02, R1.03, R1.19 |
| R1.02 | [R1.02-backend-trait-and-base-cache.md](R1.02-backend-trait-and-base-cache.md) | M | R1.01 | R1.05, R1.07, R1.08, R1.09 |
| R1.03 | [R1.03-daemon-skeleton.md](R1.03-daemon-skeleton.md) | M | R1.01, R0.09 | R1.04, R1.05 |
| R1.04 | [R1.04-config-and-state-dir.md](R1.04-config-and-state-dir.md) | S | R1.03, R0.09 | R1.18, R1.22, R2.08 |
| R1.05 | [R1.05-workspace-service.md](R1.05-workspace-service.md) | M | R1.02, R1.03, R1.07 | R1.06, R1.12, R1.13, R1.15, R1.20, R1.21, R1.22, R2.01 |
| R1.06 | [R1.06-workspace-move.md](R1.06-workspace-move.md) | S | R1.05, R1.13, R1.11 | |
| R1.07 | [R1.07-copy-backend.md](R1.07-copy-backend.md) | S | R1.02 | R1.05, R1.11 |
| R1.08 | [R1.08-apfs-clone-backend.md](R1.08-apfs-clone-backend.md) | M | R1.02 | R1.10, R1.20 |
| R1.09 | [R1.09-overlayfs-backend.md](R1.09-overlayfs-backend.md) | L | R1.02 | R1.20 |
| R1.10 | [R1.10-reflink-backend.md](R1.10-reflink-backend.md) | S | R1.08 | |
| R1.11 | [R1.11-backend-conformance-tests.md](R1.11-backend-conformance-tests.md) | M | R1.07 | R1.06, R1.12; every backend must pass it |
| R1.12 | [R1.12-workspace-creation-benchmarks.md](R1.12-workspace-creation-benchmarks.md) | S | R1.11, R1.05, R0.06 | RX.06 |
| R1.13 | [R1.13-watcher-snapshots.md](R1.13-watcher-snapshots.md) | M | R1.05, R0.04 | R1.06, R1.14, R1.15, R1.16, R1.20 |
| R1.14 | [R1.14-watchman-backend.md](R1.14-watchman-backend.md) | M | R1.13 | |
| R1.15 | [R1.15-lease-manager.md](R1.15-lease-manager.md) | M | R1.05, R1.13 | R1.17, R1.18, R1.21, R1.23 |
| R1.16 | [R1.16-local-snapshot-marking.md](R1.16-local-snapshot-marking.md) | S | R1.13 | R1.17, R1.18, R4 (publish) |
| R1.17 | [R1.17-garbage-collection.md](R1.17-garbage-collection.md) | M | R1.15, R1.16, R1.22 | R1.18 |
| R1.18 | [R1.18-backup-and-restore.md](R1.18-backup-and-restore.md) | M | R1.04, R1.15, R1.16, R1.17, R1.22 | R1.24 |
| R1.19 | [R1.19-workspace-spec-file.md](R1.19-workspace-spec-file.md) | S | R1.01 | R1.20 |
| R1.20 | [R1.20-shared-store-mounting.md](R1.20-shared-store-mounting.md) | M | R1.19, R1.08, R1.09, R1.05, R1.13, R1.22 | R1.23, R1.24, RX.01 |
| R1.21 | [R1.21-ws-run.md](R1.21-ws-run.md) | S | R1.05, R1.15, R1.22 | R1.24, R2.11 |
| R1.22 | [R1.22-workspace-cli.md](R1.22-workspace-cli.md) | M | R1.04, R1.05 | R1.17, R1.18, R1.20, R1.21, R1.24, R2.05 |
| R1.23 | [R1.23-crash-reclaim-load-tests.md](R1.23-crash-reclaim-load-tests.md) | M | R1.15, R1.20 | |
| R1.24 | [R1.24-quickstart-docs.md](R1.24-quickstart-docs.md) | S | R1.18, R1.20, R1.21, R1.22, R1.25, R1.26, R1.27, R1.28 | RX.04 |
| R1.25 | [R1.25-zero-step-repository-onboarding.md](R1.25-zero-step-repository-onboarding.md) | M | R1.05, R1.21, R1.22 | R1.24, R1.27, RX.07 |
| R1.26 | [R1.26-agent-facing-cli.md](R1.26-agent-facing-cli.md) | M | R1.05, R1.21, R1.22 | R1.24, R1.27, R2.13, RX.07 |
| R1.27 | [R1.27-agent-config-init.md](R1.27-agent-config-init.md) | S | R1.25, R1.26 | R1.24, R2.13, R2.25, RX.07 |
| R1.28 | [R1.28-prebuilt-binaries-and-installer.md](R1.28-prebuilt-binaries-and-installer.md) | M | R0.03, R1.22 | R1.24, R4.17, RX.07 |
| R1.29 | [R1.29-create-latency-off-the-watcher-path.md](R1.29-create-latency-off-the-watcher-path.md) | M | R1.12, R1.13, R1.14 | The APFS half of the first exit criterion, RX.06 |
| RX.07 | [RX.07-agent-onboarding-benchmark.md](RX.07-agent-onboarding-benchmark.md) | M | R1.25, R1.26, R1.27, R1.28 | R2.17 (cross-cutting; first needed for the Phase 1 release) |

## Suggested execution order

1. R1.01 (needs only Phase 0), then R1.03 (needs R1.01 for the database). R1.02 (needs R1.01) runs in parallel with R1.03; R1.04 follows R1.03.
2. R1.07 (needs R1.02), then R1.05 (needs R1.02, R1.03, and R1.07: its tests need a backend that materializes). In parallel: R1.08, R1.09 (need R1.02) and R1.19 (needs R1.01). R1.09 is the long pole of the phase; start it as soon as R1.02 lands.
3. R1.13 (needs R1.05). R1.22 (needs R1.04 and R1.05). R1.11 (needs R1.07), then R1.10 and R1.12 (R1.12 also needs R1.05).
4. R1.15, R1.16, R1.14 (need R1.13). R1.06 (needs R1.13 and R1.11). R1.20 (needs R1.19, R1.08, R1.09, R1.13, and R1.22).
5. R1.17 and R1.21 (need R1.15 and R1.22; R1.17 also needs R1.16).
6. R1.25 and R1.26 (need R1.21 and R1.22, both merged) in parallel; R1.28 (needs only R0.03 and R1.22) alongside them. R1.27 after both R1.25 and R1.26.
7. R1.29 (needs only merged tickets) any time before R1.23: R1.12 found `create_api/large/apfs/20` at 1.93 s, and R1.23 should measure the phase with the fix in.
8. R1.18, R1.23, R1.24, then RX.07 last. R1.23 is the phase's acceptance test and should run against everything else merged; RX.07 is the phase's agent acceptance test and needs R1.24's quickstart and R1.28's installer published to run scenario C.

## Phase exit criteria (from dev-phases.md)

- Twenty workspaces on the 100k-file synthetic repo, each created in under one second on APFS and overlayfs, with disk usage per workspace proportional to modified files.
- Killing an agent process mid-edit leaves a snapshot in the store and no directory on disk after the lease expires.
- Two workspaces on the same change work without interfering.
- A shared dependency store is mounted read-only into every workspace and verified against a lockfile hash.
- Works on macOS and Linux with the same CLI.
- A workspace moves to another change without losing its lease or leaving a directory behind.
- Two hundred workspaces with active watchers on one machine, with a write reaching the store as a snapshot within five seconds at that load.
- A backup taken while agents are writing restores to a daemon that serves the same changes and operation log, with no workspace lost and no lease reclaimed on the first tick.
- On the onboarding benchmark's scenario A (RX.07), an agent with no Reef configuration and only `reef` on `PATH` reaches a successful `reef ws run` in at most eight tool calls and with at most two failed `reef` invocations, median of three trials.

Which ticket proves each criterion: R1.12 (creation time and disk), R1.23 (kill, 200 workspaces, same-change workspaces, both platforms), R1.20 (shared store), R1.06 (move), R1.18 (backup).

## Departures from dev-phases.md

These were decided while writing the briefs. `dev-phases.md` was edited to match in the same change.

- **R1.02 grows to M and owns the base cache.** The `Backend` trait does not "capture the writable layer as a tree": jj-lib's working copy scans the merged directory and builds the tree itself (see "Workspace model on jj"). Backends materialize, report disk usage, swap the base, and tear down. Every backend materializes from a base cache entry, so the base cache (materialization of a tree through jj-lib, keyed by tree id) lives in R1.02, not R1.08. Base cache eviction is in R1.17, which `dev-phases.md` already listed there.
- **R1.09 does not use user namespaces in Phase 1.** A mount made inside a user and mount namespace is visible only to processes in that namespace, and an agent must be able to `cd` into a workspace from any shell. The Linux backend has two tiers: kernel overlayfs when the daemon has `CAP_SYS_ADMIN`, `fuse-overlayfs` otherwise. The user-namespace path is written up in the ADR as the option for a sandboxed `reef ws run` later.
- **R1.05 adds a `Repositories` service.** "Every request names a registered repository" needs a way to register one. `Register`, `List`, and `Forget` are part of R1.05.
- **R1.13 fixes the staleness policy** ADR 0008 left open, and the behavior when two workspaces edit one change. See "Snapshots, staleness, and divergence".
- **R1.22 adds `reef completions <shell>`** (shell completions through `clap_complete`), which the `dev-phases.md` row did not list, and **`reef ws gc` moves to R1.17**, which owns the `Gc` RPC. R1.22 builds the CLI skeleton with a stub for `ws gc`, and R1.17 replaces the stub, so R1.22 no longer waits for R1.17.
- **R1.25 to R1.28 and RX.07 were added on 2026-10-21**, after R1.01 to R1.22 had merged, to make Phase 1 usable by an agent with no setup: implicit registration and jj alongside git (R1.25), the agent-facing CLI contract with `reef guide`, error hints, and `reef here` (R1.26), `reef init` writing harness files (R1.27), prebuilt binaries and an installer pulled forward from R4.17 (R1.28), and an onboarding benchmark that becomes a Phase 1 exit criterion (RX.07). **R1.24 changes scope**: it no longer requires an existing jj repository or a `repo register` step, installs with the installer rather than two `cargo install` commands (R1.22's autostart already runs the daemon from the `reef` binary, so one binary was always enough), and gains a "set up your agent" section; it now depends on the four new tickets. R2.13, R2.17, RX.04, and R4.17 rows were amended and R2.24 to R2.26 added in `dev-phases.md` in the same change.

## Decisions fixed across all Phase 1 tickets

These were settled while writing the briefs. Do not re-open them inside a ticket; open a new ADR if one turns out to be wrong. Each brief repeats the subset it depends on and adds its own.

### Daemon process and threading

| Topic | Decision | Set by |
| --- | --- | --- |
| Runtime | tokio multi-thread runtime in `reef-daemon` for the gRPC server, timers, the lease loop, and watcher event handling. Nothing from jj-lib runs on tokio worker threads (ADR 0008). | R1.03 |
| Repository thread | One dedicated `std::thread` per registered repository, started on register or daemon start, owning a `RepoHead { repo: Arc<ReadonlyRepo>, settings: UserSettings, root, colocated }`. Every jj transaction (`start_transaction` to `commit`), every `load_at_head`, every `git::import_refs`, and the GC run on this thread, through an `mpsc` of `Box<dyn FnOnce(&mut RepoHead) + Send>` jobs with a oneshot reply. This thread is the repository's write lock: R1.18's quiesce pauses it. | R1.03 |
| Workspace threads | Scans, check-outs, and `finish` for one workspace run on `tokio::task::spawn_blocking` with `pollster::block_on`, holding that workspace's `Arc<std::sync::Mutex<WorkspaceHandle>>` (the jj `Workspace`, which is `Send` and not `Sync`). Concurrency is bounded by a `tokio::sync::Semaphore` of `watcher.max_concurrent_scans` permits (default: `std::thread::available_parallelism`). | R1.03, R1.13 |
| Data across threads | Ids (`OperationIdHex`, `CommitIdHex`, tree ids), `SnapshotStats`-style plain structs, and `Arc<ReadonlyRepo>`. `Arc<ReadonlyRepo>: Send + Sync` holds in jj-lib 0.46.0 (verified: its index trait objects are declared `Send + Sync` and its `change_id_index` is a `once_cell::sync::OnceCell`). R1.03 adds a static assertion so a jj-lib bump that breaks it fails to compile. | R1.03 |
| Single instance | `fd-lock` on `<state>/daemon.lock`. A second daemon on the same state directory exits 1 with the holder's pid. | R1.03 |
| Shutdown | SIGTERM and SIGINT: stop accepting connections, finish in-flight RPCs (5 s budget), snapshot every dirty workspace, persist, exit 0. Workspaces and leases survive a restart; leases get `restart_grace` on start (see leases). | R1.03, R1.15 |
| In-process API for tests | `reef_daemon::Daemon::start(Config) -> DaemonHandle` (socket path, shutdown, join) so integration tests run a daemon in a temp state directory without spawning the binary. `crates/reef-daemon/tests/common/mod.rs` has `TestDaemon` (temp state dir, a registered synthetic repo, a connected client). | R1.03 |
| Logs | stderr as in R0.09, plus a file `<state>/logs/daemon.log` in JSON, rotated daily by `tracing-appender`, keeping the `log.keep_days` (default 7) most recent daily files (a count, not an age: R1.03 uses `tracing-appender`'s own pruning). | R1.03 |

### State directory and configuration

| Topic | Decision | Set by |
| --- | --- | --- |
| State directory | `REEF_STATE_DIR`, else Linux `$XDG_STATE_HOME/reef` (default `~/.local/state/reef`), macOS `~/Library/Application Support/reef`. Resolved with `etcetera` 0.11 (the `Apple` base strategy explicitly on macOS; `choose_base_strategy` is XDG there too). Created `0700`. | R1.04 |
| Socket | `REEF_SOCKET`, else `config.daemon.socket`, else Linux `$XDG_RUNTIME_DIR/reef/daemon.sock` falling back to `<state>/daemon.sock` (the `$XDG_RUNTIME_DIR` socket only for the default state directory: a daemon given `REEF_STATE_DIR` or `--state-dir` keeps its socket in it, so daemons on different state directories never share a socket; R1.03), macOS `<state>/daemon.sock`. Path must fit `sockaddr_un` (104 bytes on macOS); the daemon fails to start with a clear error otherwise. Socket mode `0600`. | R1.04 |
| Config file | `REEF_CONFIG`, else `<state>/config.toml`. Missing file means defaults. Parsed with `toml` 1 + serde, durations through `humantime-serde` (`"30m"`), sizes as integers in bytes or strings with `KiB`/`MiB`/`GiB` via `reef_lib::jj::settings::parse_byte_size` (made `pub` by R1.04). | R1.04 |
| Environment overrides | Only `REEF_STATE_DIR`, `REEF_SOCKET`, `REEF_CONFIG`, and the R0.09 `REEF_LOG*` variables. No generic `REEF_<SECTION>_<KEY>` mapping. | R1.04 |
| Layout | `<state>/config.toml`, `daemon.sock`, `daemon.lock`, `daemon.pid`, `reef.sqlite3` (+ `-wal`, `-shm`), `logs/`, `repos/<repo-id>/bases/<tree-id>/` (base cache entries; `<tree-id>.state/` beside each holds jj's tree state for it), `repos/<repo-id>/ws/<ws-id>/` (workspace roots; overlay backends add `<ws-id>.upper/` and `<ws-id>.work/` as siblings), `repos/<repo-id>/wc/<ws-id>/` (jj working-copy state for the workspace, outside the tree), `stores/<name>/<lockfile-hash>/` (R1.20), `backups/` is not used (backups go where the caller says). | R1.04 |
| Config schema | Sections and defaults: `[daemon] socket, pid_file`; `[identity] name = "reef", email = "reef@<hostname>"`; `[log] keep_days = 7`; `[lease] default = "30m", max = "24h", grace = "5m", restart_grace = "5m", heartbeat = "60s", tick = "10s"`; `[watcher] backend = "auto" (auto, notify, watchman), debounce = "500ms", min_interval = "1s", fallback_interval = "60s", max_concurrent_scans = <cpus>, watchman_threshold_files = 50000, watchman_attach_quiet = "2s", watchman_attach_defer_max = "10s"` (R1.29); `[workspace] backend = "auto" (auto, copy, apfs, reflink, overlayfs, fuse-overlayfs), root = "<state>/repos/<repo-id>/ws", fuse_overlayfs = "fuse-overlayfs", overlay_nosuid = false`; `[base_cache] max_entries = 8`; `[gc] undo_window = "7d", git_executable = "git", reindex_limit = 100000`; `[backup] retention = "30d", quiesce_max = "60s", sqlite_pages_per_step = 256`; `[snapshot] max_new_file_size = "1MiB"`; `[repo] auto_register = true` (R1.25: a command given a path in no registered repository registers it, initializing jj alongside a git checkout, and retries once; read by the CLI). `reef config show` prints the effective configuration as TOML with a `# source: default | file | env` comment per key. | R1.04, R1.25 |
| jj settings | Built by `reef_lib::jj::settings` from `[identity]` and `[snapshot]`; `operation.hostname` and `operation.username` set to the machine hostname and the OS user name (jj-lib leaves both empty otherwise, ADR 0008). jj's own user config files are never read. | R1.04 |

### Identifiers and time

| Topic | Decision | Set by |
| --- | --- | --- |
| Ids | `RepoId`, `WorkspaceId`, `LeaseId`: 64 random bits from `rand::rng()`, displayed as 16 lowercase hex characters, newtypes in `reef_daemon::model::ids` with `Display`, `FromStr`, `rusqlite::ToSql/FromSql`, and serde as strings. The CLI accepts any unique prefix of at least 4 characters, like jj; JSON output always carries the full id. | R1.01 |
| jj workspace name | `reef-<ws-id>`. The user's own workspace at the registered root stays `default` and is never leased, watched, or reclaimed. | R1.01 |
| Owner | Opaque string the client supplies. `reef ws run` sends `<user>@<host>:<pid>`; `reef ws create` sends `<user>@<host>` unless `--owner` is given. Replaced by the session identity in R3.03 without a schema change. | R1.01 |
| Timestamps | Unix milliseconds UTC as SQLite `INTEGER`; RFC 3339 UTC in the API, JSON, and logs. | R1.01 |
| Change references | API fields are hex strings: `change_id` (jj change id, hex as `ChangeIdHex`), `commit_id`, `op_id`, `tree_id`. The CLI also prints jj's reverse-hex "k to z" change id form for humans, through a helper in `reef_lib::jj`. | R1.05 |

### Workspace model on jj

| Topic | Decision | Set by |
| --- | --- | --- |
| A Reef workspace is a jj workspace | Each Reef workspace has its own jj working-copy commit and its own `LocalWorkingCopy`, built with `LocalWorkingCopy::init(store, workspace_root, state_path, op_id, name, &settings)` and `Workspace::new(root, repo_path, Box::new(wc), repo_loader)` (both verified public in jj-lib 0.46.0). The state path is `<state>/repos/<repo-id>/wc/<ws-id>/`, outside the tree, so the workspace directory holds only the tree: no `.jj` for an agent to delete and nothing extra for a backend to clone. The workspace is also registered in jj's `WorkspaceStore` (`repo.loader().workspace_store().add(name, root)`) so `jj workspace list` in the user's checkout shows it, and forgotten on release. | R1.01, R1.05 |
| Starting point | `Create` takes one of: `change: <id prefix>` (edit that change: `MutableRepo::edit(name, &commit)`), `new_on: <revision>` (a new empty commit on top: `MutableRepo::check_out(name, &commit)`), or nothing (`new_on` the repository's `main` bookmark if it exists, else the `default` workspace's working-copy commit). Revisions resolve as change id prefix, commit id prefix, or local bookmark name, in that order; no revset language in Phase 1. In a colocated repository, `git::import_refs` runs on the repository thread before a bookmark is resolved. | R1.05 |
| Materialization | Base cache entry for the target commit's tree (materialized through jj-lib once per tree per machine, with jj's `tree_state` for it kept beside it) → backend materializes the workspace from it → the workspace's `tree_state` is seeded by copying the base's, so the first snapshot is a no-op scan rather than a rehash of every file. This requires backends to preserve each file's mtime and size, which R1.11 tests. If seeding fails or the copy is missing, `locked_wc.reset(&commit)` is the fallback (jj marks every file's state as a placeholder and rehashes on the next snapshot; verified in `TreeState::reset`). | R1.02, R1.05 |
| Release | Final snapshot, `MutableRepo::remove_workspace(name)` (abandons an empty working-copy commit, keeps a non-empty one as a visible head), `workspace_store().forget`, backend tear-down, row kept with `state = released`. | R1.05 |
| Lifecycle states | `active`, `releasing`, `released`, `reclaimed`. Rows are never deleted by release or reclaim; R1.17 GC deletes `released` and `reclaimed` rows older than the undo window. | R1.01 |

### SQLite schema

Set by R1.01 unless noted. `rusqlite` 0.40 with features `bundled` and `backup`, WAL mode, `foreign_keys = ON`, `busy_timeout = 5s`. One connection owned by a `std::sync::Mutex` in the daemon; reads and writes are short. Migrations are numbered SQL files embedded with `include_str!` and applied by `PRAGMA user_version`: `0001_initial` (R1.01), `0002_base_cache` (R1.02), `0003_snapshots` (R1.16), `0004_stores` (R1.20), `0005_watchman` (R1.14: `workspaces.watcher`, `repositories.tracked_files`). A migration's number is fixed by this list, not by landing order; migrations are independent so they apply in any order. Every table carries `repo_id`. Timestamps are `INTEGER` unix milliseconds. Ids are `TEXT`.

```sql
repositories(id TEXT PRIMARY KEY, root TEXT NOT NULL UNIQUE, jj_repo_path TEXT NOT NULL,
             colocated INTEGER NOT NULL, registered_at INTEGER NOT NULL)
workspaces(id TEXT PRIMARY KEY, repo_id TEXT NOT NULL REFERENCES repositories(id),
           owner TEXT NOT NULL, purpose TEXT NOT NULL DEFAULT '',
           change_id TEXT NOT NULL, commit_id TEXT NOT NULL, base_tree_id TEXT NOT NULL,
           backend TEXT NOT NULL, path TEXT NOT NULL UNIQUE, state_dir TEXT NOT NULL,
           lease_id TEXT REFERENCES leases(id),
           state TEXT NOT NULL CHECK (state IN ('active','releasing','released','reclaimed')),
           divergent INTEGER NOT NULL DEFAULT 0, attention TEXT,
           created_at INTEGER NOT NULL, last_snapshot_at INTEGER, last_snapshot_op TEXT)
leases(id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL REFERENCES workspaces(id),
       owner TEXT NOT NULL, issued_at INTEGER NOT NULL, expires_at INTEGER NOT NULL,
       last_heartbeat_at INTEGER NOT NULL, length_ms INTEGER NOT NULL, grace_ms INTEGER NOT NULL,
       ended_at INTEGER, end_reason TEXT CHECK (end_reason IN ('released','expired','restored')))   -- a move keeps the lease
reclaims(id INTEGER PRIMARY KEY AUTOINCREMENT, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL,
         reason TEXT NOT NULL CHECK (reason IN ('released','expired','gc','restore')),
         final_op_id TEXT, snapshot_taken INTEGER NOT NULL, freed_bytes INTEGER, at INTEGER NOT NULL, detail TEXT)
base_cache(repo_id TEXT NOT NULL, tree_id TEXT NOT NULL, path TEXT NOT NULL, bytes INTEGER NOT NULL,
           files INTEGER NOT NULL,                                                            -- added by R1.02
           created_at INTEGER NOT NULL, last_used_at INTEGER NOT NULL, PRIMARY KEY (repo_id, tree_id))   -- R1.02
snapshots(op_id TEXT PRIMARY KEY, repo_id TEXT NOT NULL, workspace_id TEXT NOT NULL, commit_id TEXT NOT NULL,
          change_id TEXT NOT NULL, kind TEXT NOT NULL CHECK (kind IN ('watcher','explicit','final','move','shutdown')),
          local INTEGER NOT NULL DEFAULT 1, taken_at INTEGER NOT NULL)                                 -- R1.16
stores(repo_id TEXT NOT NULL, name TEXT NOT NULL, lockfile_hash TEXT NOT NULL, path TEXT NOT NULL,
       built_at INTEGER, sealed INTEGER NOT NULL DEFAULT 0, PRIMARY KEY (repo_id, name, lockfile_hash))  -- R1.20
meta(key TEXT PRIMARY KEY, value TEXT NOT NULL)   -- daemon_instance_id, created_at, last_backup_at, rematerialize_pending (R1.18)
```

`workspaces.attention` is a short machine-readable reason (`divergent`, `stale-recovered`, `final-snapshot-failed`, `store-hash-mismatch`, `store-missing`, `watch-failed`, `upper-missing`) or NULL, shown by `ws list` and `ws inspect`.

### Materialization backends

| Topic | Decision | Set by |
| --- | --- | --- |
| Trait | In `reef_daemon::backend`: `trait Backend: Send + Sync { fn kind(&self) -> BackendKind; fn materialize(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<(), BackendError>; fn disk_usage(&self, base: &BaseEntry, ws: &WorkspacePaths) -> Result<DiskUsage, BackendError>; fn swap_base(&self, ws: &WorkspacePaths, from: &BaseEntry, to: &BaseEntry) -> Result<SwapOutcome, BackendError>; fn tear_down(&self, ws: &WorkspacePaths) -> Result<(), BackendError>; }` plus `fn probe(dir: &Path, bases_dir: Option<&Path>) -> Vec<Probe>` free function returning which kinds can serve a directory (built from the base cache at `bases_dir`, which clone backends need on the same volume; R1.08 added the argument) and why not for the rest. `BackendKind = { Copy, ApfsClone, Reflink, Overlayfs, FuseOverlayfs }`, stored as the lowercase name in `workspaces.backend`. `SwapOutcome = { TreeReplaced, NeedsCheckout }`: overlay backends replace the lower layer and empty the upper (`TreeReplaced`, the daemon then calls `locked_wc.reset`); clone and copy backends return `NeedsCheckout` and the daemon applies the tree diff with `locked_wc.check_out`. | R1.02 |
| Selection | `workspace.backend = "auto"` picks the first available in the order overlayfs, fuse-overlayfs, apfs, reflink, copy for the workspace root's filesystem. A named backend that is unavailable is an `Unavailable` error naming the reason, never a silent fallback. | R1.02 |
| Base cache | `reef_daemon::backend::base_cache::BaseCache`: `ensure(tree_id, materialize) -> BaseEntry` (the closure runs the jj check-out where the repository's `Repo` lives) materializes the commit's tree into `bases/<tree-id>/` through a temporary `LocalWorkingCopy` (`check_out` from the empty tree) whose state dir becomes `bases/<tree-id>.state/`, writing into `bases/<tree-id>.tmp/` and renaming when complete. Entries are immutable after creation by convention, not by mode bits: the daemon never writes there after the rename, and R1.11 asserts a base is byte-identical after a workspace built from it is modified and torn down. Row in `base_cache`; `last_used_at` bumped on every materialize. Eviction in R1.17. | R1.02 |
| Incremental bases | R1.08 adds `ensure_from(tree_id, nearest, cloner, check_out_diff)`: clone the most recently used base with the backend's clone primitive and `check_out` the diff, for clone backends. Copy and overlay backends always do a full check-out. | R1.08 |
| Disk usage | `DiskUsage { allocated_bytes, files_changed }`. Overlay backends: allocated blocks (`st_blocks * 512`) of the upper directory. Clone and copy backends: allocated blocks of files whose mtime or size differs from the base's `tree_state`, by a stat-only walk (`walkdir`). It is an estimate and documented as one. Phase targets are measured by free-space delta (`statvfs`) in R1.12 and R1.23, not by this field. | R1.02 |
| macOS | `clonefile(2)` via `libc` on the base directory, one call for the whole tree (APFS clones directories recursively, preserving mtimes; measured at 1.0 to 1.2 s and 30 MiB for the 102,005-file `large` tree on an M2 Max, against 13 s for per-file clones), falling back to a per-file `reflink_copy::reflink` walk if the directory call fails with any other error. `EXDEV` and `ENOTSUP` mean the volume cannot clone at all (per-file clones fail the same way), so they are `Unavailable`. `man 2 clonefile` discourages cloning directories; the brief keeps it because nothing else meets the one-second criterion, and ADR 0015 records that. `libc` 0.2 lacks `CLONE_NOFOLLOW`; R1.08 defines it locally. Requires the workspace root and the base cache on the same APFS volume; both are under the state directory by default. A `--path` on another volume falls back to copy with a warning. | R1.08 |
| Linux reflink | `reflink_copy::reflink` per file (FICLONE) over a directory walk, creating directories and symlinks by hand and restoring mtimes with R1.07's `utimensat` helper. Available on btrfs and XFS with reflink; probed by cloning a probe file. | R1.10 |
| Linux overlayfs | Kernel overlayfs via `nix::mount::mount` (`lowerdir=<base>,upperdir=<ws>.upper,workdir=<ws>.work,redirect_dir=on,index=off,metacopy=off`, retried without `redirect_dir` on `EINVAL`) when the daemon has `CAP_SYS_ADMIN`; `fuse-overlayfs` (external binary named by `workspace.fuse_overlayfs`, default `fuse-overlayfs` on `PATH`, spawned daemonized with `-o lowerdir=...,upperdir=...,workdir=...` so mounts survive daemon restarts) otherwise, unmounted with `fusermount3 -u`, with a lazy `umount2(MNT_DETACH)` on `EBUSY`. Both report `BackendKind::Overlayfs` and `FuseOverlayfs` respectively. Mounts are recorded in `workspaces.backend` and re-established on daemon start for `active` workspaces. CI runs the kernel path under `sudo -E` in a dedicated Linux job and the FUSE path unprivileged. | R1.09 |
| Copy | `std::fs::copy` per file with mtimes restored afterwards (`utimensat` via nix, so read-only files work), as the baseline and the backend of last resort. | R1.07 |
| Tear-down | Clone and copy backends' `tear_down` renames the workspace root to a `.trash-<nanos>` sibling and deletes it in the background. Overlay backends unmount (lazily if busy), remove the then-empty root, and trash the upper and work directories the same way (a mountpoint cannot be renamed). Daemon start reconciles overlay mounts, then sweeps `*.trash-*` under `repos/*/ws/`. "No directory on disk" holds from the rename or the removal. | R1.07, R1.09 |

### Snapshots, staleness, and divergence

| Topic | Decision | Set by |
| --- | --- | --- |
| Trigger | One `notify` 8 `RecommendedWatcher` per daemon (one inotify instance on Linux, since `fs.inotify.max_user_instances` defaults to 128 and the target is 200 workspaces), watching every active workspace root recursively, events demultiplexed to workspaces by path prefix, debounced per workspace with `notify-debouncer-full` 0.7 at `watcher.debounce` (500 ms). A fallback timer snapshots every workspace with no event in `watcher.fallback_interval`. Overlay upper and work directories are never watched. | R1.13 |
| Scan then commit | Per snapshot: on the workspace thread, lock the working copy and `snapshot(&SnapshotOptions)` (jj-lib scans the merged directory; the tree is built by jj); on the repository thread, the transaction (rewrite the working-copy commit, `set_wc_commit`, `rebase_descendants`, `commit`); back on the workspace thread, `finish(new_op_id)`. `reef_lib::jj::Repo::snapshot` is split into these halves and the daemon drives them; the single-process `snapshot` stays for tests and benchmarks. | R1.13 |
| Staleness | Replace R0.04's op-id equality with `WorkingCopyFreshness::check_stale(locked_wc, &wc_commit, &repo)`. `Fresh` → proceed. `Updated(op)` → reload the repository at head and retry once. `SiblingOperation` → reload at head (jj-lib merges divergent operation heads) and retry once. `WorkingCopyStale` (the working-copy commit was rewritten by another operation) → the divergence rule below. The repository thread reloads at head after every transaction it commits, so `Updated` and `Sibling` only arise from the `jj` CLI used on the registered root. | R1.13 |
| Divergence rule | `WorkingCopyStale` with no unsnapshotted edits (the scanned tree equals the working copy's old tree) → adopt: `locked_wc.check_out` to the rewritten commit, no operation written. Only a workspace with local edits diverges: when a workspace's working-copy commit was rewritten under it (another workspace on the same change snapshotted, or someone ran `jj describe`/`rebase` on it), the workspace keeps its own edits and never adopts the rewrite silently: its next snapshot rewrites the commit it last snapshotted (`rewrite_commit(&old_wc_commit)`, same change id) into a new visible commit and sets it as the workspace's working-copy commit. The change becomes divergent in jj's sense; `workspaces.divergent = 1`, `attention = 'divergent'`, and `ws list` shows it. Nothing is lost, nothing blocks, and resolution is R2.03's (squash or abandon one side). A workspace whose working-copy commit is gone from the view (`jj workspace forget` on the registered root) recovers: a new commit from the scanned tree with the last-snapshotted commit's parents and a fresh change id, `attention = 'stale-recovered'`. The alternative, jj's recovery commit (`create_and_check_out_recovery_commit`), is recorded in ADR 0018 as rejected because it moves the workspace onto another agent's tree. | R1.13 |
| Explicit snapshots | `Snapshot` RPC; `Release`, `Move`, and `Inspect` (with `snapshot_first = true`, the CLI default) take one. `Heartbeat` never does. | R1.05, R1.13 |
| Operation descriptions | `reef: snapshot workspace <ws-id>` (watcher and explicit), `reef: final snapshot of workspace <ws-id> (<reason>)`, `reef: move workspace <ws-id> to <change>`, `reef: create workspace <ws-id>`, `reef: release workspace <ws-id>`. GC writes no operation of its own: the operations it keeps are rewritten with their descriptions (R1.17). `is_snapshot` is set on snapshot operations only. | R1.05, R1.13 |
| Ignores | The repository's `.gitignore` chain as in R0.04, plus `.reefignore` at the workspace root with the same syntax (`GitIgnoreFile::chain_with_file`), plus the `[snapshot] exclude` paths from `reef.toml` (R1.19). Shared store mount points are excluded automatically (R1.20). | R1.13, R1.19 |
| Local marking | Every watcher, final, and explicit snapshot operation is indexed in `snapshots` with `local = 1`. Nothing in Phase 1 clears it; R4's publish does. The `local` column and the `kind` are what the "never exported until published" rule reads. | R1.16 |
| Watchman | `watcher.backend = "watchman"` (or `auto` with `watchman` on `PATH` and the repository above `watcher.watchman_threshold_files`, default 50,000 tracked files) enables jj-lib's `watchman` feature: `fsmonitor.backend = "watchman"` in the jj settings so the scan visits only changed paths, and a Watchman subscription replaces the notify watcher as the trigger. Same `Watcher` trait as notify. | R1.14 |

### Leases and reclaim

| Topic | Decision | Set by |
| --- | --- | --- |
| Lease | Issued on `Create` with `length` (request, else the repository's `reef.toml` `[workspace] lease`, else `lease.default`) and `grace` (`lease.grace`). A request above `lease.max` is `InvalidArgument`; a `reef.toml` value above it is capped with a warning. `Heartbeat(workspace_id, lease_id)` extends `expires_at` to now + length and returns the new expiry. `Create` and `Heartbeat` responses carry `heartbeat_every` (the daemon's `lease.heartbeat`, clamped to at most length / 3); `ws run` heartbeats on that, not on a client constant. A heartbeat with a stale `lease_id` is `Conflict`. | R1.15 |
| Expiry | A tick every `lease.tick` finds leases with `expires_at + grace_ms < now` and reclaims each: final snapshot (`kind = 'final'`), `reclaims` row with `reason = 'expired'`, `remove_workspace` in jj, backend tear-down, `state = reclaimed`, lease `end_reason = 'expired'`. A heartbeat between `expires_at` and `expires_at + grace` revives the lease. A failed final snapshot is logged at `error`, the directory is kept, and `attention = 'final-snapshot-failed'`; the next tick retries. | R1.15 |
| Restart | On start, every active lease gets `expires_at = max(expires_at, now + lease.restart_grace)`; nothing is reclaimed on the first tick. | R1.15 |
| `ws run` | Creates (or attaches to `--workspace`), runs the command with cwd at the workspace root and `REEF_WORKSPACE`, `REEF_WORKSPACE_PATH`, `REEF_REPO`, `REEF_SOCKET` in its environment, heartbeats every `heartbeat_every` from the daemon, forwards SIGTERM and SIGHUP to the child always and SIGINT only when stdin is not a terminal (the child stays in `ws run`'s process group so an interactive agent keeps the tty), escalates to SIGKILL on a second signal within 5 s, and on child exit takes a final snapshot and releases unless `--keep`. Exit code is the child's. If the daemon becomes unreachable, `ws run` keeps the child running, retries heartbeats with backoff, and warns on stderr; the lease may expire and the directory may vanish under the child, which is the designed outcome. | R1.21 |
| GC | `Gc` RPC and `reef ws gc`: (1) jj's own GC on the repository thread, in an order derived from jj-lib's semantics (jj-cli's order was not checked): abandon the operations older than `keep_newer` by rewriting the newer ones onto the root operation with `jj_lib::op_walk::reparent_range` and moving the op heads with `update_op_heads` (the head operation is always kept, and active Reef working copies are rebound to the new head); then `op_store().gc(&head_ids, keep_newer)`; then `DefaultIndexStore::reinit` and reload at head (the rebuilt index holds every commit any reachable operation references, so without the abandonment nothing is freed); then `GitBackend::gc(index, keep_newer)`, which runs the external `git gc` named by `gc.git_executable`. `keep_newer = now - max(gc.undo_window, backup.retention if meta.last_backup_at + retention > now)` (`reparent_range`, `OpStore::gc`, and `GitBackend::gc` verified public in jj-lib 0.46.0), (2) base cache entries no active workspace references, oldest `last_used_at` first, down to `base_cache.max_entries`, (3) `released` and `reclaimed` workspace rows (and any `wc/` dir left behind) older than the undo window, (4) `stores` entries no `reef.toml` of a registered repo names. Reports bytes freed per category. Never runs automatically in Phase 1. | R1.17 |
| Backup | Order (ADR 0020): `meta.last_backup_started_at`; SQLite's online backup (`rusqlite::backup`, `backup.sqlite_pages_per_step` pages per step) from a second connection; per repository, under a brief pause of its thread, the op heads and `store/extra/heads` read first, then `op_store/`, `store/extra/`, `workspace_store/`, `submodule_store/`, and every `type` file copied; without the pause the git repository, refs before objects, mtimes kept; then the heads into `repo/op_heads/heads/` and `heads.json`, and the manifest rewritten with `complete: true`. `index/` keeps only its `type` file and empty directories. `meta.last_backup_at` is updated. Restore is offline: refuses an incomplete manifest, an existing database without `--force`, and a live repository newer than the backup without `--force`; keeps workspace records, gives every lease `lease.restart_grace`, and sets `meta.rematerialize_pending`; the next daemon start rematerializes every `active` workspace (a missing directory rebuilt instead of reclaimed, every working copy recreated), then clears the flag. | R1.18 |

### API

| Topic | Decision | Set by |
| --- | --- | --- |
| Toolchain | `tonic` 0.14, `tonic-prost` 0.14, `prost` 0.14; `tonic-prost-build` 0.14 in `build.rs` fed a `FileDescriptorSet` from `protox` 0.10 so neither CI nor contributors need `protoc`. Generated code is not checked in. Schemas in `crates/reef-daemon/proto/reef/daemon/v0/*.proto`, package `reef.daemon.v0`, exposed as `reef_daemon::proto`. Timestamps in messages are `google.protobuf.Timestamp` and durations `google.protobuf.Duration` (protox bundles the well-known types; `prost-types` supplies the Rust side). R2.01 replaces `v0` with the reviewed `v1` schema. | R1.05 |
| Transport | Unix socket: `tokio::net::UnixListener` with `tokio_stream::wrappers::UnixListenerStream` into `Server::serve_with_incoming`; client `Endpoint::try_from("http://[::]:0")` with `connect_with_connector(service_fn(UnixStream::connect))` wrapped in `hyper_util::rt::TokioIo`. `reef_daemon::client::Client` wraps the generated clients and is what the CLI and tests use. | R1.03, R1.05 |
| Services | `Daemon { Status }` (R1.03); `Repositories { Register, List, Forget }` and `Workspaces { Create, List, Inspect, Release, Snapshot }` (R1.05); `Workspaces.Heartbeat` (R1.15); `Workspaces.Move` (R1.06); `Maintenance { Gc }` (R1.17), `Maintenance { Backup (server streaming), Quiesce, Resume }` (R1.18); `Stores { Build (server streaming), List }` (R1.20). Restore is offline, in the CLI. `Heartbeat` returns `UNAVAILABLE` while quiesced; resume gives every lease at least `lease.grace` from then, and a quiesce auto-resumes after `backup.quiesce_max`. | as listed |
| Request shape | Every repository-scoped request carries `repo: string` accepting a repo id, id prefix, or a path inside a registered root. Every mutating response carries `op_id` of the operation it created (empty when none was). | R1.05 |
| Errors | `ErrorKind` → `tonic::Code`: `NotFound` → `NOT_FOUND`, `AlreadyExists` → `ALREADY_EXISTS`, `InvalidArgument` → `INVALID_ARGUMENT`, `Conflict` → `FAILED_PRECONDITION`, `Unavailable` → `UNAVAILABLE`, `PermissionDenied` → `PERMISSION_DENIED`, `Io` and `Internal` → `INTERNAL`. Status message is the `ErrorChain` text; details carry a `reef.daemon.v0.ErrorDetail { kind, chain: repeated string }` message. | R1.05 |

### CLI

| Topic | Decision | Set by |
| --- | --- | --- |
| Commands | `reef daemon run` (foreground), `reef daemon start|stop|status` (detached `reef-daemon` found next to `reef` or on `PATH`, else `reef daemon run` from the `reef` binary itself), `reef config show`, `reef repo register [path]|list|forget`, `reef ws create|list|inspect|release|move|run|gc`, `reef init [path]` (R1.27), `reef store build|list`, `reef backup <dir>`, `reef restore <dir>`, `reef completions <shell>`. Any command needing the daemon starts it when the socket is absent unless `--no-autostart`. From R1.22 on, subcommands whose ticket has not landed exist as stubs that print `not implemented until R1.xx` and exit 1 without contacting the daemon, so `--help` is complete. `--repo` defaults to the repository containing the current directory, except for `ws list`, which lists every repository's workspaces without it. | R1.22 |
| Output | JSON (one object, or one array for lists) when stdout is not a terminal or `-o json`; aligned text otherwise (`-o text` forces it). Errors go to stderr as `error: <chain>`; in JSON mode also `{"error": {"kind", "message"}}` on stdout, with `hint` and `next` when the fix is known (R1.26). | R1.22, R1.26 |
| Exit codes | 0 ok; from `ErrorKind`: `InvalidArgument` 2, `NotFound` 3, `AlreadyExists` 4, `Conflict` 5, `Unavailable` 6, `PermissionDenied` 7, `Io` 8, `Internal` 1. `ws run` exits with the child's code (or 128 + signal). | R1.22 |
| Agent contract | Never interactive (no prompt, pager, or editor; stdin is read only by `ws run`'s child). Every error with a known fix carries `hint` (a sentence) and `next` (an argv array) in the JSON error object and on stderr in text mode; the daemon sets them in ADR 0017's mapping layer, never inside message text. `reef` with no arguments and `reef guide` print `docs/agent-guide.md` (embedded; at most 1,600 bytes, short form at most 40 lines; text even on a pipe, JSON `{guide, commands}` only when asked); `reef --help` at most 60 lines. `reef here` answers where the agent is (by `REEF_WORKSPACE`, else by path; `Inspect` accepts a path inside a workspace and `skip_disk_usage`) without a snapshot, exits 0 outside a workspace, and names the next command (none inside `ws run`, which releases; `["reef","ws","run","--"]` outside, the one prefix). `infer_subcommands` and visible aliases (`workspace`, `new`, `ls`, `rm`, `agent-guide`); names are never renamed. `REEF_OUTPUT` defaults `-o`. `ws list --limit` (100, newest first; a cut is noted on stderr, the JSON stays an array). ADR 0025. | R1.26 |
| Agent files | `reef init [PATH]` registers (R1.25; not with `--check`, `--print`, or `--no-register`, which find the root as the nearest `.jj` or `.git` from PATH up, else PATH) and writes, from `docs/agent-guide.md`: a managed section (short form) between `<!-- reef:begin v1 -->` and `<!-- reef:end -->` lines in `AGENTS.md` always and `CLAUDE.md` for Claude Code (not when it imports `@AGENTS.md` and has no section); whole files (long form) `.claude/skills/reef/SKILL.md`, `~/.claude/skills/reef/SKILL.md` with `--user`, `.cursor/rules/reef.mdc`, replaced only when their frontmatter has `generated-by: reef init`. Detection by files at the root (`CLAUDE.md` or `.claude/`; `.cursor/`); `--agents auto|all|none|claude,cursor`. Bytes outside the markers never change; damaged markers or a newer section version skip the file; symbolic links are followed but not out of the repository (or home). Atomic writes. `--check` exits 3 listing missing or stale files, with `next` the `reef init` that fixes them. Every generated file names the `reef` version, so a version bump makes them stale until regenerated. R2.13 adds the `.mcp.json` renderer. | R1.27 |
| Repository onboarding | A command whose `--repo` is a path and gets `NotFound` ("repository containing") registers the path with `if_not_jj = INIT_ALONGSIDE_GIT`, prints `reef: registered <root>` on stderr, and retries once. A git checkout without `.jj` gets jj initialized alongside git (`Repo::init_in_git_repo`, `/.jj/` in `.git/info/exclude`); a `.git` file (worktree or submodule) is `InvalidArgument`; no `.git` at all stays `NotFound`. Off with `--no-register` or `[repo] auto_register = false`. `repo register --no-init` keeps the old behavior. ADR 0024. | R1.25 |
| `reef init` | Registers (R1.25), then writes managed `<!-- reef:begin v1 -->` sections into `AGENTS.md` (always) and `CLAUDE.md` (when `claude` is detected or named; skipped when it imports `@AGENTS.md`), and whole owned files `.claude/skills/reef/SKILL.md`, `.cursor/rules/reef.mdc`, and with `--user` `~/.claude/skills/reef/SKILL.md`, all rendered from `docs/agent-guide.md`. Never overwrites text it did not write; `--check` exits 3 when stale; `--print` for pasting. `.mcp.json` is R2.13's. | R1.27 |
| Distribution | A `v<version>` tag builds `aarch64-apple-darwin`, `x86_64-apple-darwin`, `x86_64-unknown-linux-musl`, `aarch64-unknown-linux-musl` (gnu on `ubuntu-22.04` as the recorded fallback), tarballs with both binaries and completions, `SHA256SUMS`, a GitHub Release with the `CHANGELOG.md` section, `install.sh` (POSIX, checksum-verified, `~/.local/bin`, no `sudo`, `REEF_VERSION` and `REEF_INSTALL_DIR`), a push of the formula to the Homebrew tap, and binstall metadata. `reef --version` prints version, commit, and target. Unsigned until R4.17. | R1.28 |
| Workspace spec | `reef.toml` at the repository root: `[workspace] lease = "30m"`, `[snapshot] exclude = ["target/", "node_modules/"]`, `[[store]] name, lockfile, mount, build` (an argv array, no shell) with optional `[store.env]` (R1.20). Read from the target commit's tree at workspace creation, never from disk, and the lockfile bytes for hashing come from the same tree. Every store mount is added to the excludes as an anchored pattern. Parsed by `reef_lib::spec`. Unknown keys are errors. | R1.19 |
| Shared stores | `stores/<name>/<blake3 of lockfile>/`, per daemon and keyed by `(name, hash)` (`stores.repo_id` records the first builder), built by the daemon running the declared `build` command once in a scratch workspace on the copy backend with the store directory writable, then sealed (`chmod -R a-w`; on Linux with privilege, a read-only bind mount into the workspace instead of a symlink). Mounted into workspaces as a symlink at `mount` (or the bind mount), excluded from snapshots and from the watcher. A lockfile whose hash has no sealed store is `Unavailable` with the `reef store build` hint, unless `Create` is sent with `missing_stores = SKIP` (`--skip-missing-stores`), which creates the workspace with `attention = 'store-missing'`; a mismatch found later sets `attention = 'store-hash-mismatch'`. `reef store build|list` are in R1.20. | R1.20 |

### Crates and versions

Verified against crates.io on 2026-10-16 with `cargo info`. Pin the major (or `0.x` minor) in `[workspace.dependencies]`; licenses are all on the `deny.toml` allow list (notify is CC0-1.0, blake3 CC0-1.0 OR Apache-2.0).

| Crate | Version | For | Set by |
| --- | --- | --- | --- |
| `rusqlite` | 0.40 (`bundled`, `backup`) | SQLite | R1.01 |
| `tonic`, `tonic-prost`, `prost` | 0.14 | gRPC | R1.03, R1.05 |
| `tonic-prost-build`, `protox`, `prost-types` | 0.14, 0.10, 0.14 (build-dependencies) | codegen without protoc | R1.03, R1.05 |
| `tokio-stream`, `hyper-util`, `tower` | 0.1, 0.1, 0.5 | Unix socket transport | R1.03 |
| `tokio-util` | 0.7 | `CancellationToken` for shutdown | R1.03 |
| `fd-lock` | 4 | single-instance lock | R1.03 |
| `tracing-appender` | 0.2 | daily log file | R1.03 |
| `toml`, `humantime-serde`, `etcetera` | 1, 1, 0.11 | config | R1.04 |
| `notify`, `notify-debouncer-full` | 8, 0.7 (9.0 is a release candidate; do not use it) | watcher | R1.13 |
| jj-lib `watchman` feature | pulls `watchman_client` 0.9 | Watchman | R1.14 |
| `libc` | 0.2 | `clonefile`, `statvfs` | R1.08 |
| `reflink-copy` | 0.1 | per-file FICLONE and clonefile | R1.08, R1.10 |
| `nix` | 0.31 (features `mount`, `fs`, `signal`, `process`) | `utimensat` in the copy walk, overlayfs mount, signals in `ws run` | R1.07, R1.09, R1.21 |
| `walkdir` | 2 | disk usage and copy walks | R1.02 |
| `blake3` | 1 | lockfile hash for stores (a verification digest, not an object identity; ADR 0011 is unaffected) | R1.20 |
| `assert_cmd` | 2 (dev) | CLI tests | R1.22 |
| `clap_complete`, `humantime` | 4, 2 | shell completions, duration flags | R1.22 |
| `sha2` | 0.11 (dev; 0.11 rather than 0.10 because jj-lib's dependencies already pull it in) | conformance tree digest in R1.11 (use `blake3` instead if R1.20 has landed) | R1.11 |

`uuid` is not used (ids are 64-bit hex). `rand` and `clap` are already workspace dependencies.

### ADR numbers

Allocated here so parallel tickets do not collide. The index row in `docs/adr/README.md` is added by the ticket that writes the ADR.

| ADR | Title | Ticket |
| --- | --- | --- |
| 0012 | Daemon process model and threading | R1.03 |
| 0013 | Daemon state directory and configuration | R1.04 |
| 0014 | Workspace model on jj | R1.01 |
| 0015 | Materialization backends and the base cache | R1.02 |
| 0016 | overlayfs on Linux: privilege tiers | R1.09 |
| 0017 | Daemon API v0 and error mapping | R1.05 |
| 0018 | Snapshot staleness and divergence policy | R1.13 |
| 0019 | Leases and reclaim | R1.15 |
| 0020 | Local backup format | R1.18 |
| 0021 | Workspace spec file | R1.19 |
| 0022 | Shared dependency stores | R1.20 |
| 0023 | Watchman as the large-repository watcher | R1.14 |
| 0024 | Repository onboarding: implicit registration and jj alongside git | R1.25 |
| 0025 | Agent-facing CLI contract | R1.26 |

### Platform and CI

| Topic | Decision | Set by |
| --- | --- | --- |
| Platforms | macOS and Linux. Windows is RX.05; nothing in Phase 1 may `cfg(windows)` beyond compiling. | all |
| Benchmarks | Phase 0's "benches live in `crates/reef-lib/benches/`" becomes "benches live in the crate that owns the code they measure"; fixtures are shared through `target/bench-fixtures/`. R1.12's bench is `crates/reef-daemon/benches/workspace_create.rs`. | R1.12 |
| CI jobs | The existing `check` matrix runs everything that needs no privilege. R1.09 adds a Linux job `overlayfs` that runs `cargo nextest run -p reef-daemon --features privileged-tests` under `sudo -E` with the toolchain's `PATH` preserved, and installs `fuse-overlayfs` from apt for the unprivileged path. R1.12 extends `bench.yml`. R1.23 adds `load.yml` on both OSes (weekly and on `workflow_dispatch`, not on PRs, 90 min). The gated 200-workspace run uses the `medium` preset (the runners' disk and the default inotify watch limit of 1,048,576 cannot hold 200 `large` trees); `large` times 200 is opt-in by `REEF_LOAD_PRESET=large` with the sysctl raised, run by hand before the phase closes, and every load run also creates and times twenty `large` workspaces for the creation criterion. | R1.09, R1.12, R1.23 |
| Linux limits | `fs.inotify.max_user_watches` and `max_user_instances` are read and logged at start; R1.23 records the values hit. One inotify instance per daemon (above). | R1.13, R1.23 |
| Test daemons | Integration tests use `TestDaemon` with `REEF_STATE_DIR` set to a `tempfile::tempdir()`; socket paths must stay under 104 bytes, so the temp dir name is short (`reef-XXXX`). Synthetic repos come from `reef_testing::synth` (`small` in tests, `large` in benchmarks and load tests). | R1.03 |

## Ticket file format

Each ticket has: Context, Decisions, Deliverables (file by file), Implementation notes, Acceptance criteria, Verification, Out of scope, Risks. Acceptance criteria are written so an agent can check each one mechanically before opening the PR. Briefs that depend on jj-lib or third-party crate behavior have a "What was verified while writing this ticket" subsection under Context, as R0.04 did; anything not verified is marked as such.

Process for each ticket is unchanged from Phase 0: branch `r1.xx-<slug>`, implement, Fable 5.1 review, fix, PR with the review table, `docs/dev-phases.md` updated if scope or dependencies changed.
