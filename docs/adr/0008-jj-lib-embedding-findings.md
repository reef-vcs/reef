# 0008. jj-lib embedding findings

- Status: accepted
- Date: 2026-10-16
- Ticket: R0.04

## Context

[0002](0002-build-on-jj-lib.md) decided to build on jj-lib without testing it. The R0.04 spike embedded `jj-lib = "=0.46.0"` in `reef-lib` as the `reef_lib::jj` module, which opens a jj workspace, snapshots it, lists heads, reads the operation log, and undoes the last operation. `crates/reef-lib/tests/jj_spike.rs` exercises it against internal-git repos, colocated repos, and a repo created by the `jj` 0.46.0 CLI. The flows copy jj-cli 0.46.0 (`cli_util.rs` `snapshot_working_copy` and `base_ignores`, `commands/undo.rs`).

### Exposed and usable as-is

- Loading: `Workspace::load` with the default backend and working-copy factories; `RepoLoader::load_at_head` and `ReadonlyRepo::reload_at_head`. Internal-git, colocated, and jj-CLI-created repos all open.
- Snapshot: `start_working_copy_mutation`, `LockedWorkingCopy::snapshot(&SnapshotOptions)`, `LockedWorkspace::finish`. In-tree `.gitignore` files are read by the working copy itself; untracked too-large files come back in `SnapshotStats::untracked_paths`.
- Transactions: `start_transaction`, `set_is_snapshot`, `set_workspace_name`, `rewrite_commit(..).set_tree(..).write()`, `set_wc_commit`, `rebase_descendants`, `commit(description)`.
- Operation log: `op_walk::walk_ancestors` with `OperationMetadata` (time range, description, host, user, `is_snapshot`, workspace name).
- Undo: `Operation::parents`, `Operation::view`, `MutableRepo::set_view`, `Workspace::check_out`.
- Diffs: `MergedTree::diff_stream` with matchers. Repository creation without the CLI: `Workspace::init_internal_git` and `init_colocated_git`.

### Not exposed or not present

- Leases, ownership, or expiry of any kind.
- Copy-on-write materialization. `LocalWorkingCopy` is a full checkout.
- Filesystem watching. `jj_lib::fsmonitor` is a Watchman client behind the `watchman` feature, not a notify-based watcher.
- A conflict API beyond `Merge<TreeId>` and `MergedTree`; R3.25 has to map these to Reef's conflict objects.
- Hooks or events for "snapshot taken" or "operation written".
- Parent-directory search for `.jj`: `Workspace::load` only looks in the given directory. jj-cli's `find_workspace_dir` does the search, and `Repo::open` copies it.
- Per-workspace locking is a file lock at `.jj/working_copy/working_copy.lock`.
- The `testing` feature only serves jj's own tests. There is no `testutils` crate on crates.io, and fixtures are built with `init_internal_git` and `init_colocated_git`. Naming the hash kind for those needs `gix` as a direct dev-dependency, pinned to jj-lib's `=0.88.0`, because jj-lib does not re-export `gix`.

### Gotchas

- **Async with blocking I/O inside.** The futures are runtime-agnostic, but the I/O inside them blocks, and sync helpers such as `Store::get_commit` call `pollster::block_on` themselves. The daemon must run jj-lib on a dedicated thread pool or `spawn_blocking` (R1.03).
- **Stale working copies are the caller's job.** `Repo::snapshot` returns `Stale` whenever the working copy's operation differs from the loaded one. jj-cli is more lenient: when the working copy is *newer* than the loaded repo (another process snapshotted), it silently reloads at the working copy's operation. R1.13 must choose between that and reloading at head.
- **Nothing comes from the user's jj config unless we pass it.** `max_new_file_size`, ignores, and identity are all supplied. jj-lib's defaults leave `operation.hostname` and `operation.username` empty, so operations Reef writes record neither until the daemon sets them.
- **Lock discipline.** jj-lib writes `.jj/working_copy/` on `finish`, so two processes on one working copy need jj's lock discipline. `undo_last` must always call `check_out`, even when the working-copy commit is unchanged, or the next snapshot sees a stale working copy.
- **Undo semantics.** `undo_last` restores the parent's view except git refs and git `HEAD`, as jj-cli does. It does not walk jj's undo stack, refuse operations from another workspace, or reset git `HEAD` in colocated repos (R2.03). A file changed on disk since the last snapshot is overwritten by the checkout, so callers snapshot first.
- **Divergent operation heads** are merged silently on `load_at_head` (a "reconcile divergent operations" operation).
- **Colocated repos.** jj-cli imports and exports git refs around snapshots and creates a new commit when the working-copy commit is immutable. The spike does neither, so Reef and `jj` can disagree about bookmarks in a colocated repo (likely fix: Reef runs `git::import_refs` on open, R1).
- **Build cost.** `cargo build -p reef-lib` with jj-lib and gix takes 21 s cold (debug, 12-core M2 Max).
- **Version churn.** The crate is 0.x. 0.46 moved to edition 2024, raised the MSRV to 1.97.1, and split out `jj-core`, so every bump is its own ticket and Dependabot ignores jj-lib.

### Measurements

Release build, `timing_report` in `jj_spike.rs`, Apple M2 Max (12 cores, 32 GB), macOS 27.0.1, APFS. The tree is 10,000 files of about 200 bytes in 100 directories, generated inline because R0.05 had not landed; internal-git repo.

| Step | Time |
| --- | --- |
| open | 0.9 ms |
| first snapshot (10,000 new files) | 2,146 ms |
| no-op snapshot | 11.1 ms |
| snapshot, 50 files modified | 59.6 ms |
| snapshot, 1 file modified (mean of 100) | 46.5 ms |
| of which: scan | 11.8 ms |
| of which: write commit and rebase | 5.6 ms |
| of which: commit operation and save working-copy state | 28.9 ms |
| `op_log(100)` | 2.3 ms |
| `undo_last` | 25.5 ms |

Scanning a 10,000-file tree with nothing changed costs about 11 ms. A one-file change adds about 35 ms, mostly in committing the operation and saving the working-copy state; that phase was not split further. Saving the state is O(tracked files): `TreeState::save` serializes every file's state into `.jj/working_copy/tree_state` on each snapshot. This is well within the phase one target of a snapshot within 5 seconds, but it is the first place to profile if per-snapshot latency on large trees matters.

### jj-lib types that leak through `reef_lib::jj`

- `UserSettings`: `settings::user_settings` returns it and `Repo::open` takes it. R1.03 wraps it in the daemon's config.
- `Commit`: `Repo::wc_commit` returns it.
- jj-lib error types, as `#[source]` inside Reef's error enums (`WorkspaceLoadError`, `RepoLoaderError`, `SnapshotError`, `BackendError`, `TransactionCommitError`, `OpStoreError`, `CheckoutError`).

Ids (`OperationIdHex`, `CommitIdHex`, `ChangeIdHex`), `ChangeSummary`, `SnapshotOutcome`, `ChangedPath`, `OpEntry`, and `UndoOutcome` are Reef types. R1.01 settles the id types for storage; R2.01 wraps everything that crosses the API.

## Decision

We will continue building on jj-lib 0.46.0 through the `reef_lib::jj` module, which stays the only place that names jj-lib types. Upstream contributions worth proposing, in order:

1. A hook or event when a snapshot or operation is written, so the daemon does not have to poll the op heads.
2. A public, notify-based fsmonitor alongside Watchman, which R1.13 needs anyway.
3. A cheaper staleness check that does not require taking the working-copy lock.

## Consequences

- Phase one builds on a tested path: open, snapshot, op log, and undo work against repos the `jj` CLI creates.
- Reef owns behavior jj-cli provides implicitly: parent-directory search, ignore assembly, stale-working-copy policy, undo-stack semantics, and colocated git ref import and export. Each needs a ticket (R1.13, R2.03, R1).
- The daemon must keep jj-lib off its async executor threads.
- `gix` is pinned in two places and must move in lockstep with jj-lib.
- We revisit this if a jj-lib bump breaks more than the `jj` module, or if the snapshot hook is refused upstream and polling proves too costly at the phase one target of 200 workspaces.

## Alternatives considered

**Shell out to the `jj` CLI.** No API churn, and the CLI is what users run. Rejected: it means parsing text output, a process per call, and no access to transactions, which design principle 8 forbids.

**Pin jj-lib 0.45.1** (MSRV 1.89, pre-`jj-core`). It would have built on the toolchain the machines had at the time. Rejected: 0.46.0 builds on the pinned 1.99.1, and starting on an older minor only adds a bump ticket.

**Wrap every jj-lib type now.** Rejected for the spike: the wrappers would be guesses until R1.01 and R2.01 know what crosses storage and the API.
