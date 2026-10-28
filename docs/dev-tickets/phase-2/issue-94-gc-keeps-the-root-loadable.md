# Issue #94 GC keeps the user's jj workspaces loadable

Phase 2 · Size M (no row; see "Size") · Fixes: issue #94 · Depends on: R1.17 (`Repo::gc`, `gc_jj`, the rebind of Reef's workspaces), R2.09 (`gc_with`'s callback, which moves subscriptions) · Unblocks: #95 (its jj step may run on a timer once this lands)

## Context

`reef ws gc` abandons the operations older than the undo window by rewriting the newer ones onto the root operation (`reparent_range`), so every kept operation gets a new id. Reef moves its own active workspaces to the new head. It leaves the jj workspaces it does not own alone: the registered root's `default`, and any workspace the user added with `jj workspace add`. Their `.jj/working_copy/checkout` still names the old id of the operation they last recorded. The operation store's GC then prunes unreachable operation files by mtime. If that old operation is younger than `keep_newer`, it survives, but its older ancestors are deleted. The operation then exists but cannot be loaded. `jj st` fails with "Could not read working copy's operation". `jj workspace update-stale` loads the repository at that operation, fails building the index, and never reaches jj's recovery for a *missing* operation. A root that ran `jj` within the undo window, in a repository with older operations, is broken by its first GC. That is the normal case for a colocated checkout. The `reef ws gc` help and `Repo::gc`'s documentation both say `update-stale` recovers the root, and it does not.

This ticket makes GC move every jj workspace Reef does not own along with the rewrite, under jj's working-copy lock:

- **A workspace whose operation was rewritten** moves to that operation's rewrite, which has the same view. jj's staleness check then gives exactly the answer it gave before GC.
- **A workspace whose operation GC abandons, or that an earlier GC stranded,** moves to the head when its files are its commit's there. Otherwise its operation is made missing, so that jj's own recovery applies, as after `jj op abandon` plus `jj util gc`.
- **While a `jj` command holds a workspace's lock,** GC abandons nothing and says so.

There is no schema change: `API_REVISION` stays 5.

### What was verified while writing this ticket

Reef at `26189c7` (#93 merged); jj-lib 0.46.0 and jj-core 0.46.0 (`Cargo.lock`), read in the registry sources; the `jj` 0.46.0 binary for jj's own behavior. Paths are under `crates/` unless they start with `docs/`.

- **The failure, reproduced with the jj CLI.** In a scratch colocated repository with `jj` 0.46.0:
  1. Four commits in `default`, then every file under `.jj/repo/op_store` is backdated 30 days (`touch -t`) and the newest operation is noted as the cut.
  2. A `jj st` with a new file in the root gives `default` a recent operation X.
  3. `jj workspace add ../ws2` and a commit there add newer operations.
  4. From `ws2`: `jj op abandon ..<cut>`, which is `reparent_range`, then `jj util gc`, which is the operation store's GC by mtime with the default two weeks.

  In the root, `jj st` then exits 1 with "Could not read working copy's operation", and `jj workspace update-stale` exits 255 with "Failed to load an operation … Object <cut> of type operation not found". This is the issue's failure: the issue's message is "Failed to read index" because there a deeper ancestor was missing. jj's own `op abandon` moves only the workspace it runs in, so jj has the same gap for other workspaces.
- **The fix, by hand.** In the same setup, the root's 66-byte `checkout` file was rewritten with X's rewritten id: field 2, after `12 40`; the rewritten id was found by matching end time and description in `jj op log`. `jj st` then exits 0 and shows `fr` added. `@` keeps its commit id, and `jj st` records no operation. A further edit snapshots normally. Variant: the root's operation was older than the cut and its files matched. Pointing the checkout at the new op head gave the same clean `jj st` with `@` unchanged.
- **Option 2's own path, by hand.** The root's operation was older than the cut and another workspace had since rebased `default@` with a tree change (`jj abandon` of `c3` from `ws2`). The pruned operation then made `jj st` fail with the `update-stale` hint. `jj workspace update-stale` printed "Failed to read working copy's current operation; attempting recovery" and created "RECOVERY COMMIT FROM `jj workspace update-stale`", which re-adds `f3` on top of the rebased `@`. The result is lossless and visible, and it is jj's documented behavior.
- **Reef's GC.** `Repo::gc_unlogged` (`reef-lib/src/jj/gc.rs:172`):
  1. Loads at head (`:180`).
  2. Finds the cut with `first_older_than`, the newest strict ancestor of the head that ended before `keep_newer` (`:350`).
  3. Calls `reparent_range(op_store, [cut], [head], root)` (`:190`).
  4. Moves the op heads (`:200`) and calls the caller's `abandoned(old head, new head)` (`:205`).
  5. Runs `op_store().gc(heads, keep_newer)` (`:219`), then `reinit` and reload (`:221`, `:227`), then `git gc` (`:240`).

  The doc comment says the caller "leaves others (the user's `default`) for `jj workspace update-stale`" (`:127` to `:129`). The help says the same (`reef-vcs/src/cli/ws.rs:98`).
- **`reparent_range` does not return its map.** `jj_lib::op_walk::reparent_range` (`op_walk.rs:403`) builds `rewritten_ids: HashMap<old, new>` (`:422`, `:433`) but returns only `ReparentStats { new_head_ids, rewritten_count, unreachable_count }` (`:384`). The issue's "`reparent_range` knows the old→new id map" holds only inside jj-lib, so Reef needs its own copy to learn a non-head operation's rewrite. The function is about 40 lines over public API: `walk_ancestors_range` (`:290`), `Operation::store_operation` (`operation.rs:156`), and `OpStore::write_operation`. Operation ids are content hashes, so Reef's copy writes byte-identical operations to jj's.
- **The operation store's GC is by mtime.** `SimpleOpStore::gc` (`simple_op_store.rs:274`) keeps what is reachable from the given heads (`:306`). It deletes any other operation or view file whose mtime is not newer than `keep_newer` (`remove_file_if_not_new`, `:283`). Old copies of rewritten operations that are younger than the window survive; their older ancestors do not.
- **Why the existing tests missed it.** `reef-daemon/tests/gc.rs` and `reef-lib/tests/jj_gc.rs` run GC with `undo_window = "0s"` or `keep_newer = now`, after `past_this_second()`. Every old copy is then pruned, the root's included, so its operation is *missing* and jj's recovery would apply. No test runs the root's `jj` after GC with a nonzero window. There is no "GC test clock" to backdate operations, contrary to the issue's step 3. Tests backdate with jj-lib's `debug.operation-timestamp` (`settings.rs:174`, used for every transaction's end time, `transaction.rs:73`, `:182`) plus `File::set_modified` on the operation and view files, as `maintenance/backup.rs:738` backdates a file.
- **Staleness.** `WorkingCopyFreshness::check_stale` (`working_copy.rs:361`) is `Fresh` when the working copy's operation is the repository's. When that operation is an ancestor, it compares the recorded tree with the view's commit's tree: `Fresh` or `WorkingCopyStale`. Otherwise it is `SiblingOperation`. Loading the working copy's operation or its ancestors fails with `ObjectNotFound`. So pointing a workspace at its operation's rewrite (same view, still an ancestor of the head) preserves the answer exactly. Pointing it at the head is exact only when the recorded tree is the head view's commit's tree. That is the rule `Repo::rebind_if_stranded` (`gc.rs:282`) and `working_copy_matches_view` (`:318`) already apply to Reef's own workspaces.
- **jj's checkout file and its lock.** `CheckoutState` (`local_working_copy.rs:2678`) is the `Checkout` proto (`protos/local_working_copy.proto:69`: `operation_id = 2`, `workspace_name = 3`). It is written by `save` (`:2705`) through a `NamedTempFile` in the state directory and `persist_temp_file` (jj-core `file_util.rs:235`). The only writer jj-lib exposes is `LockedWorkingCopy::finish` (`:3035`, which saves the checkout only when the operation changed, `:3057`), reached through `start_mutation` (`:2759`). `start_mutation` takes `FileLock::lock(state_path/working_copy.lock)`, which **blocks** until the lock is free. `jj_lib::lock::FileLock` also has `try_lock` (`lock/unix.rs:39`, an `flock` with `LOCK_NB`), and `jj_lib::lock` and `jj_lib::protos` are public (`lib.rs:67`, `:78`). `flock` locks belong to an open file description, so a second open of the lock file in the same process conflicts. Checked with Python's `fcntl.flock`: a second fd gets `EWOULDBLOCK`. Reef therefore cannot call `start_mutation` while it holds `try_lock`, and a test can hold the lock in-process to stand in for a running `jj`. `LocalWorkingCopy::load(store, wc_path, state_path, settings)` (`:2835`) reads the checkout and, lazily, the tree state without locking.
- **Where the user's workspaces are.** The registered root's state is `<root>/.jj/working_copy` (`workspace.rs:592`; type file `:159`, `"local"` = `LocalWorkingCopy::NAME`). jj records every workspace's path in the repository's workspace store: `Workspace::init_workspace_with_existing_repo` calls `workspace_store.add` (`workspace.rs:411`), and `get_workspace_path` reads it back (`simple_workspace_store.rs:224`, through `RepoLoader::workspace_store`, `repo.rs:778`). `DefaultWorkspaceLoaderFactory.create(path)` resolves a workspace's `.jj/repo` (a directory, or a file naming one) to the canonical repository path (`workspace.rs:573` to `:596`). Reef's workspaces are named `reef-<id>` (`REEF_WORKSPACE_PREFIX`, `reef-lib/src/jj/workspace.rs:40`). Their working-copy state lives in Reef's state directory, not under their own `.jj`, and release forgets them from the view and the workspace store (`:701` to `:723`).
- **The daemon.** `RepoHead.repo` is "the user's `default` workspace, loaded at head" (`reef-daemon/src/repo/mod.rs:46`), and `gc_job` calls `head.repo.gc_with` on it (`workspace/gc.rs:264`). Its callback rebinds the active Reef workspaces to the new head (`:270` to `:283`, `rebind` `:297`) after `settle` made each match its commit (`:148`). Workspaces that cannot be settled stop GC from abandoning anything ("no operations were abandoned: …", `:95`). The module doc says `default` is not touched (`:22` to `:24`), as do `maintenance/gc.rs:33` to `:35` and R1.17's brief (`docs/dev-tickets/phase-1/R1.17-garbage-collection.md:29`, `:34`). A dry run skips the jj step (`maintenance/gc.rs:228`). Nothing in `reef-daemon`, `reef-vcs` or `reef-client` reads the root's working-copy state; the only callers of `rebind_if_stranded` and `working_copy_matches_view` act on Reef workspaces (`workspace/gc.rs:229`, `:319`, `workspace/manager.rs:704`). So the failure is the `jj` CLI's, not Reef's own commands'.
- **Subscriptions read old copies.** A subscription behind a GC's rewrite first reads up to the head the rewrite replaced: "its old operations outlive the rewrite by the undo window" (`reef-daemon/src/subscribe/session.rs:86` to `:90`, ADR 0036 item 9). The issue's option 2 deletes those old copies whatever their mtime, which would end such sessions with `OPERATION_LOG_REWRITTEN` on every GC. It would also break a `jj` command that loaded the old head before the rewrite and commits a child of it afterwards. Option 2 is therefore not a general fallback. It is used only for an operation that no session or head can still reach (below).
- **The report.** `GcReport.jj_error` is "why the jj step stopped short" (`reef-api/proto/reef/api/v1/maintenance.proto:80`). The CLI prints it and exits 8 (`reef-vcs/src/cli/ws.rs:407` to `:420`). `API_REVISION` is 5 (`reef-api/src/lib.rs:49`).
- **Tests that assume the root is left alone.** `jj_gc.rs::gc_now_prunes_hidden_commits_and_keeps_the_view` asserts the working copy still names an abandoned operation and rebinds it by hand (`:101` to `:106`). `stranded_working_copy_is_rebound_only_when_stranded` (`:167`) strands `default` by running GC. Both run GC on `default`. `jj_writes.rs:366` and `reef-daemon/tests/gc.rs::stranded_working_copy_is_repaired_at_start` (`:543`, which strands a Reef workspace) do not depend on it. The jj CLI is gated in tests on `jj 0.46.x` being on `PATH` (`reef-lib/tests/jj_workspace.rs:508` to `:530`). CI does not install it (`.github/workflows/ci.yml` has no jj step), so CLI checks run locally only.
- **ADRs.** R1.17 has no ADR; its decisions are in its brief. ADR 0031's consequence says "the registered root's `default` workspace is never written by Reef" (`docs/adr/0031-writes-over-jj-state.md:31`), and accepted ADRs are edited only for status or a superseded note (`docs/adr/README.md:7`). #80 set the convention of a new ADR plus an "Amended by" header line (ADR 0031 already has one for 0039). The next free number is 0040: the index ends at 0039, and there are no open PRs.

## Decisions

### Size

M, with no `dev-phases.md` row. It fixes behavior R1.17 shipped in 0.1.0, as #80 fixed R2.03's. The work:

- in reef-lib: a mapped copy of `reparent_range`, a pass over the user's jj workspaces with a classification and a locked checkout write, and `GcOutcome` fields;
- in the daemon: two report strings and logs;
- about ten tests, ADR 0040, and help, doc-comment and CHANGELOG edits.

No schema change, no new reason, no migration. The phase README's ticket table gains a `#94` row.

### Which workspaces GC moves itself

**Foreign workspaces** are every name in the head view's working-copy commits that does not start with `REEF_WORKSPACE_PREFIX`: the root's `default`, and anything the user added with `jj workspace add`. `Repo::gc` handles them itself, in reef-lib. `reef-*` workspaces stay the caller's (the daemon rebinds the active ones in the callback, as now). A `reef-*` name in the view with no active row is left as today (see "Out of scope").

A foreign workspace's directory is the `Repo`'s own root when the name is this `Repo`'s workspace (the daemon's `RepoHead.repo` is `default`); otherwise it is `workspace_store().get_workspace_path(name)`. It is **held** only when all of these hold:

- the directory has `.jj/working_copy/checkout`;
- `DefaultWorkspaceLoaderFactory.create(dir)` succeeds, its `repo_path()` equals this repository's canonical `repo_path()`, and its working-copy type is `"local"`;
- the checkout decodes and names this workspace.

Anything else is **skipped**. Skipping means: no lock, no write, a `warn` log naming the workspace, its path and why, and GC goes on. The cases are a workspace with no recorded path, a directory the user deleted without `jj workspace forget`, a directory now used by another repository, and a non-local working copy. A skipped workspace can still break as today, but refusing GC for every forgotten directory would block it forever. The repository check keeps Reef from writing into another repository's workspace through a stale store entry.

### Taking the locks

Before loading at head, GC takes each held workspace's lock, in name order, with `jj_lib::lock::FileLock::try_lock(<dir>/.jj/working_copy/working_copy.lock)`. It polls every 50 ms until 2 s have passed in total (`CHECKOUT_LOCK_WAIT`).

- **Busy.** If any lock is still busy, nothing is abandoned in this run: GC releases what it took and goes on to the operation store's GC, the index and `git gc` without a rewrite. Those touch nothing still reachable, so a workspace whose lock was busy is safe. `GcOutcome.abandon_blocked` names the first busy workspace and its path. The issue suggested skipping only the busy workspace and reporting it. That would abandon its operation's ancestors and strand it, which is the bug itself; refusing is what GC already does for a Reef workspace it cannot settle.
- **Why before loading at head.** No `jj` command can snapshot in a held workspace between GC's load and its write, so the operation each checkout names, read under the lock, is the one GC classifies. A `jj` command that wants the lock while GC holds it blocks (jj's `FileLock::lock`) and proceeds once GC releases it. (Review correction: one that loaded the repository at the old head fails with jj's sibling-operation error instead, and one that committed before GC's load writes its old id when it checks out; the implementation checks every checkout again before collecting. See ADR 0040.)
- **Release.** GC holds the locks through the rewrite, the op heads move and the checkout writes, and releases them before the callback and the collection. A `jj` command in the root waits for at most that long: the reparent writes one file per kept operation.
- **No `start_mutation`.** Reef writes the checkout itself while it holds the lock (below), because `start_mutation` would block on Reef's own lock.

### Classifying each held workspace

GC holds the locks, loads at head H, and finds the cut C as now. If there is a cut, it lists the range R = `walk_ancestors_range([H], [C])`, the operations it will rewrite. For each held workspace with checkout operation X:

| X | Files match H's commit for it | GC does | `CheckoutOutcome` |
| --- | --- | --- | --- |
| in R (H included) | either | after the rewrite, writes X's rewrite M[X] | `Rewritten { to }` |
| the root operation, or an ancestor of H with no cut | either | nothing | `Unchanged` |
| an ancestor of C, or C itself (GC abandons it) | yes | after the rewrite, writes the new head H′ | `ToHead { to }` |
| an ancestor of C, or C itself | no | deletes X's operation file after the rewrite | `LeftForRecovery` |
| not an ancestor of H, or X or one of its ancestors missing: stranded before this GC (a 0.1 GC, a crash) | yes | writes the head (H′ after a rewrite, else H) | `ToHead { to }` |
| the same | no | deletes X's operation file if it exists | `LeftForRecovery` |

- **"Files match".** The workspace's recorded tree (`LocalWorkingCopy::load(..).tree()`, read under the lock) equals the tree of its working-copy commit in H's view, which is H′'s view too. It is computed only for the rows that need it. An error reading the tree state counts as "no", so GC never hides staleness.
- **"Ancestor of H".** `closest_common_ancestors([X], [H])` is X. An `ObjectNotFound` loading X or walking its ancestors means stranded.
- **`Rewritten` is the fix for #94.** The view is the same, so whatever jj would have said before GC (fresh, or stale and asking for `update-stale`), it says after. `update-stale` then works, because M[X]'s ancestors are the kept ones.
- **`ToHead` with matching files** is what `check_stale` would call `Fresh` anyway (verified by hand above). It also keeps a git-only user's colocated root clean: that root's operation is usually its registration, older than any window, and today that user gets jj's recovery commit at their first `jj` command after GC.
- **`LeftForRecovery`.** GC cannot move the workspace exactly, so it hands it to jj's own path for a missing operation: `jj st` points at `update-stale`, which makes a recovery commit with the files as last recorded. Nothing is lost, and it is what `jj op abandon` plus `jj util gc` do (verified above). The operation file is deleted explicitly because an mtime newer than `keep_newer` (a copied repository, or an operation stranded by a recent 0.1 GC) would otherwise keep it loadable-but-broken, the #94 state.
  - Deleting X is safe. X is unreachable from every op head: abandoned by this rewrite or stranded before it. No subscription position can be at or behind it, since a position outside the undo window already ends with `OPERATION_LOG_REWRITTEN` (ADR 0036 item 9), and the deletion happens under the workspace's lock.
  - Only `<repo>/op_store/operations/<hex>` is removed; its view goes with the next collection.
  - It is rare: the root's last `jj` command must be older than the undo window, and its commit's tree must have changed since (another workspace's rebase, or a git ref import that rebased `@`).
  - Refusing to abandon in this case was considered and rejected. A git-only user who never runs `jj` would block GC forever.

### Writing a checkout

`write_checkout(state_path, name, op)` writes what jj's `CheckoutState::save` writes, with Reef's own code:

1. Encode `jj_lib::protos::local_working_copy::Checkout { operation_id: op.to_bytes(), workspace_name: name }` with `prost::Message::encode_to_vec`.
2. Write it to a `tempfile::NamedTempFile::new_in(state_path)`.
3. Give the temporary file the old checkout's owner and mode when they differ (`std::os::unix::fs::fchown`, `set_permissions`). A root daemon (ADR 0027) must not leave a root-owned `0600` file that the user's `jj` cannot read.
4. `jj_lib::file_util::persist_temp_file` it to `checkout`.

Only the operation pointer changes. The view, the working-copy commit, `tree_state` and the files are untouched, as with `rebind_working_copy`. `tempfile` moves from reef-lib's dev-dependencies to its dependencies; it is already in the workspace and the lock file.

When the written workspace is this `Repo`'s own (`default` in the daemon), GC reloads `self.workspace` afterwards with `Workspace::load` and the same factories `Repo::open` uses (`repo.rs:421`), so `working_copy_op_id` and anything else reading the cached `LocalWorkingCopy` sees the new operation.

**A failed write** (an I/O error after the op heads moved) leaves that workspace stranded. GC still runs the callback, so Reef's own workspaces move, and releases the locks. It then skips the collection, the index rebuild and `git gc`, and returns `GcError::Checkout { workspace, path, source }` (kind `Io`). The unpruned ancestors keep X loadable for `jj workspace update-stale`, and the next GC's stranded rows repair or hand it over. The daemon reports the error in `jj_error` as it does any `GcError`.

### The order in `gc_unlogged`

1. Hold the foreign workspaces (skip, or lock with the 2 s wait). If one is busy, release them all and record `abandon_blocked`.
2. `load_at_head` → H; `first_older_than` → C, as now.
3. Not blocked: classify each held workspace. With a cut:
   1. `reparent_mapped` (Reef's copy of `reparent_range`, returning the map and the new head H′);
   2. `update_op_heads([H], H′)`;
   3. write `Rewritten` and `ToHead` checkouts and delete `LeftForRecovery` operations;
   4. reload `self.workspace` if written;
   5. release the locks;
   6. call `abandoned(H, H′)`.

   Without a cut: write the stranded rows' `ToHead` (to H) and deletions, then release.
4. Operation store GC, reinit, reload, `git gc`, as now (skipped after a failed write).

`gc_with`'s callback keeps its signature and its contract: it is called right after the op heads move, before anything is collected.

### Reef's own workspaces

Reef's own workspaces are unchanged. `settle` snapshots and matches each active Reef workspace first, and the callback rebinds each to H′ (`rebind`). Their operation is always the old head, whose rewrite is H′, so the general map would give the same answer. The lock pass does not touch `reef-*` names, and a Reef workspace's lock (in its `wc/<id>` state directory) is never one GC tries. A Reef workspace whose settle fails still blocks abandoning, as now.

### What GC reports

- **`reef_lib::jj::GcOutcome`** (Rust, not the API) gains:
  - `checkouts: Vec<CheckoutMove>`, where `CheckoutMove { workspace: String, path: PathBuf, from: Option<OperationIdHex>, outcome: CheckoutOutcome }` and `CheckoutOutcome { Rewritten { to }, ToHead { to }, Unchanged, LeftForRecovery, Skipped { reason: String } }`;
  - `abandon_blocked: Option<AbandonBlocked>`, where `AbandonBlocked::Locked { workspace, path }` is `#[non_exhaustive]`, with `Display` "jj workspace `default` at /path is locked by another process (a running `jj` command)".

  `operations_abandoned` is 0 when blocked.
- **The daemon** (`gc_jj`):
  - A blocked abandon sets `JjStep.error` to `no operations were abandoned: <AbandonBlocked>; run reef ws gc again when it finishes`. This is the same prefix as the settle refusal; the CLI prints it and exits 8.
  - Each `CheckoutMove` is logged, by reef-lib where the move happens (so a failed GC's moves are logged too): `info` "jj workspace moved onto the rewritten operation log" for `Rewritten` and `ToHead` (`workspace`, `path`, `from_op_id`, `op_id`); `warn` "jj workspace left for `jj workspace update-stale`" for `LeftForRecovery` (with a hint naming the command and the directory); `warn` for `Skipped`; `debug` for `Unchanged`.
- **No `GcReport` field.** A list of moved jj workspaces would be a schema change (API revision 6, a digest, a CLI column) for information the user needs only when something goes wrong. The two cases that need the user say so in `jj_error` or in jj's own message. Reef's own rebinds are not reported either. `API_REVISION` stays 5; `api_digest_matches_revision` passes unchanged. See open question 1.

### Text

- **`reef ws gc --help`** (`reef-vcs/src/cli/ws.rs:94` to `:99`):

  ```
  /// Delete old operations, unused bases, and released workspace records.
  ///
  /// Removes what is older than `gc.undo_window` (7 days) and bases no
  /// active workspace uses, after snapshotting every active workspace.
  /// `jj` keeps working in your checkout: GC moves it onto the rewritten
  /// operation log. Exits 8, report printed, if the jj step stopped short,
  /// such as while a `jj` command held your checkout.
  ```

- **`Repo::gc`'s doc** (`gc.rs:116` to `:134`): replace "a working copy last updated at an abandoned operation no longer loads, so the caller moves each one it owns to the new head and leaves others (the user's `default`) for `jj workspace update-stale`" with: the caller moves Reef's own (`reef-*`) working copies; GC moves every other jj workspace it can find itself, under its working-copy lock (the table above, in prose); a busy lock means nothing is abandoned (`GcOutcome::abandon_blocked`). Call it on the registered root's `Repo`. The module doc's first paragraph gains one sentence to the same effect.
- **`reef-daemon/src/workspace/gc.rs`'s module doc** (`:22` to `:24`): "The user's own jj workspaces (`default` at the root, any `jj workspace add`) are moved by `Repo::gc` itself, under jj's working-copy lock; while a `jj` command holds one, nothing is abandoned (ADR 0040)."
- **`maintenance/gc.rs:33` to `:35`**: "GC never touches a workspace directory, a base an active workspace uses, or the user's `default` workspace" becomes "… or a base an active workspace uses, and writes nothing of the user's jj workspaces but the operation their working copy names (ADR 0040)."

## Deliverables

### `crates/reef-lib/`

- `Cargo.toml`: `tempfile.workspace = true` under `[dependencies]` (removed from `[dev-dependencies]`).
- `src/jj/gc.rs`:
  - `CheckoutMove`, `CheckoutOutcome`, `AbandonBlocked`, and `GcOutcome.checkouts` and `.abandon_blocked`;
  - `GcError::Checkout` and its `Classify` arm (`Io` by source);
  - `CHECKOUT_LOCK_WAIT`;
  - the private `HeldCheckout { name, dir, state_path, op: OperationId, _lock: FileLock }`, `hold_checkouts`, `classify`, `reparent_mapped`, `write_checkout`, `remove_operation_file` and the `self.workspace` reload;
  - the new order in `gc_unlogged`, and the doc comments above.

  `reparent_mapped` carries a comment that it copies jj-lib 0.46.0's `op_walk::reparent_range` (`op_walk.rs:403`) to return the map, and must be re-checked on a jj-lib upgrade, as must `write_checkout` against jj's `CheckoutState::save`.
- `src/jj/mod.rs`: re-export `AbandonBlocked`, `CheckoutMove`, `CheckoutOutcome`.

### `crates/reef-daemon/`

- `src/workspace/gc.rs`: `gc_jj` maps `abandon_blocked` into `JjStep.error`; the module doc.
- `src/maintenance/gc.rs`: the module doc line.

### `crates/reef-vcs/`

- `src/cli/ws.rs`: the help text above.

### Documentation

- **`docs/adr/0040-gc-and-the-users-jj-workspaces.md`** (accepted, ticket issue #94), indexed in `docs/adr/README.md`:
  - **Context:** #94, the scratch reproduction, jj's own `op abandon` having the same gap, and why option 2 conflicts with ADR 0036.
  - **Decision:** foreign workspaces and how they are found and checked; the try-lock before the load and "busy means no abandon"; the classification table; Reef writing jj's checkout format itself; a failed write skipping the collection; no schema change.
  - **Consequences:**
    - amends ADR 0031's "never written by Reef" (GC writes the operation pointer of the user's jj workspaces, nothing else) and R1.17's safety rule;
    - a `jj` command in the root waits out GC's rewrite;
    - Reef depends on jj's `checkout` format and op store layout (a jj-lib upgrade re-checks both, with the reparent copy);
    - a stale root whose operation GC abandons gets jj's recovery commit;
    - roots stranded by a 0.1 GC are repaired or handed to jj at the next GC.
  - **Alternatives:**
    - option 2 as the general fallback (breaks subscriptions catching up and a concurrent `jj` writer);
    - skipping only a busy workspace (strands it);
    - blocking on jj's lock with `start_mutation` (a stopped `jj` would wedge the repository thread);
    - moving every foreign workspace to the head (hides staleness);
    - refusing to abandon past a stale root (blocks GC forever for a git-only user);
    - not abandoning past the root's operation at all (the same, for any root idle longer than the window);
    - a `GcReport` field.
- **ADR 0031** gains one header line, `- Amended by: [0039](…), [0040](0040-gc-and-the-users-jj-workspaces.md)`, replacing its current "Amended by" line, and nothing else.
- **`CHANGELOG.md`**, under Unreleased:
  - In "Fixed": "`reef ws gc` no longer leaves `jj` unusable in the repository's own checkout ("Could not read working copy's operation", with `jj workspace update-stale` failing too). GC now moves the operation your checkout, and any workspace you added with `jj workspace add`, last recorded onto the rewritten log, under jj's working-copy lock. While a `jj` command holds that lock, GC abandons no operations and says so (#94, [ADR 0040](../../adr/0040-gc-and-the-users-jj-workspaces.md))." (In the CHANGELOG the link is `docs/adr/…`, from the repository root.)
  - In "Upgrading from 0.1": "A checkout where `jj` fails after a 0.1 `reef ws gc` is repaired by the next `reef ws gc` when its files match its commit. Otherwise that GC removes the stranded operation, and `jj workspace update-stale` there recovers it with a recovery commit."
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #94 | [issue-94-gc-keeps-the-root-loadable.md](issue-94-gc-keeps-the-root-loadable.md) | M | R1.17, R2.09 | #95 (the jj step on a timer) |`.
- **Unchanged**: `docs/dev-phases.md`, R1.17's brief (a record of what was decided then), `docs/quickstart.md:250`, and the threat model. The workspace store and `.jj` are not repository content, since jj refuses `.jj` paths in a checkout; Reef already writes the root's `.jj`, and the repository check above keeps writes inside this repository's workspaces.

## Tests

### Fixtures

Fixtures for `crates/reef-lib/tests/jj_gc.rs`, beside `init()` and `settings()`:

- `settings_at(t: SystemTime) -> UserSettings`: `settings().config().clone()`, plus a `ConfigLayer::parse(ConfigSource::User, "debug.operation-timestamp = \"<t as RFC 3339>\"")`, into `UserSettings::from_config_and_home_dir(config, None)`.
- `age_op_store(root, t)`: `File::open(p)?.set_modified(t)` for every file under `.jj/repo/op_store/operations` and `views`.
- `old_history() -> (TempDir, Repo)`: `Repo::init_internal_git` with `settings_at(now − 10 d)`, then `two_snapshots` on that `Repo`, then `age_op_store(root, now − 10 d)`. It returns the root reopened with `settings()`, so later operations are recent. `KEEP` is `now − 7 d`.
- `strand_like_0_1(root, keep_newer)`: what `Repo::gc` did before this ticket, with jj-lib directly. It loads at head, finds the cut as `first_older_than` does, runs `op_walk::reparent_range`, `update_op_heads`, `op_store().gc(heads, keep_newer)` and the index `reinit`, and touches no checkout. It produces #94's state. The `reinit` is needed: without it the old operation's index segment survives and loading it does not walk the pruned ancestors.
- `jj_cli()`: `Some(())` when `jj --version` starts with `jj 0.46.`, else prints "skipped: …" and returns `None`, as `jj_workspace.rs:511` does. CLI calls set `JJ_CONFIG` to a missing file.

### `crates/reef-lib/tests/jj_gc.rs`

1. **`gc_moves_the_root_to_its_rewritten_operation`**, #94's exact shape (fails on main).
   - Setup: `old_history()`; write `root.txt` and `snapshot` on the root, giving the recent operation X (the root's checkout). Then `create_workspace("reef-x", ..)` and a snapshot in it, giving recent operations above X. Record the root's `@` commit and X's parent P.
   - Run `gc(KEEP)`.
   - Assertions on the result:
     - `operations_abandoned >= 3`;
     - `checkouts == [CheckoutMove { workspace: "default", from: Some(X), outcome: Rewritten { to: X′ } }]`, with X′ ≠ X;
     - X′ is in `op_log` from the head;
     - `op_store/operations/<X>` still exists (young) while `<P>` does not: the #94 precondition, which holds on main too.
   - Assertions on a fresh `Repo::open(root)`:
     - `working_copy_op_id() == X′`;
     - `reload_at_working_copy_op()` succeeds, which is `jj workspace update-stale`'s `load_at`; on main it fails with an `ObjectNotFound` for an ancestor;
     - after reloading at head, `rebind_if_stranded()` is false;
     - `wc_change()` names the recorded `@`;
     - writing `after.txt` and snapshotting keeps `@`'s change.
   - With `jj_cli()`: `jj st` in the root exits 0, and `op_log` has the same length before and after it.
2. **`gc_moves_a_clean_root_from_an_abandoned_operation_to_the_head`**: `old_history()` (the root's checkout is its last old snapshot), then `reef-x` created and snapshotted (recent). `gc(KEEP)` gives `ToHead { to: new_head }` for `default`, and a fresh open's `working_copy_op_id() == new_head`. With `jj_cli()`, `jj st` exits 0 and `@` is unchanged.
3. **`gc_leaves_a_stale_root_to_jj_recovery`**:
   - Setup: `old_history()`; then a recent `edit_commit` that adds `added.txt` to the root's `@` commit. This moves `default@` to a new tree the root's files do not have, the stale root of ADR 0031's consequence. Then `File::set_modified(now)` on X's operation file, so the mtime rule alone would keep it.
   - `gc(KEEP)` gives `LeftForRecovery` for `default`; X's operation file is gone; the checkout still names X; a fresh open's `reload_at_working_copy_op()` fails with `ObjectNotFound` for X itself, which is jj's recovery path.
   - With `jj_cli()`:
     - `jj st` exits 1 with "update-stale";
     - `jj workspace update-stale` exits 0 and prints "recovery commit";
     - `jj st` then exits 0;
     - `added.txt` is not on disk;
     - the root's other files are unchanged.
4. **`gc_abandons_nothing_while_the_root_is_locked`**:
   - Setup: as test 1, then the test holds `jj_lib::lock::FileLock::lock(root/.jj/working_copy/working_copy.lock)`.
   - `gc(KEEP)` returns `Ok`, and:
     - `operations_abandoned == 0`;
     - `abandon_blocked == Some(Locked { workspace: "default", path: root })`;
     - `checkouts` is empty;
     - `op_log` has the same length;
     - the checkout still names X;
     - the call took at least `CHECKOUT_LOCK_WAIT` and less than 10 s.
   - After dropping the lock, `gc(KEEP)` abandons operations and gives `Rewritten`.
5. **`gc_moves_a_users_jj_workspace`**:
   - Setup: `old_history()`; then `Workspace::init_workspace_with_existing_repo(<tmp>/mine, repo_path, &head, &*default_working_copy_factory(), "mine")` and a snapshot there through `Repo::open(<tmp>/mine)` (recent). Then the same for `gone`, whose directory is then removed with `remove_dir_all`.
   - After `gc(KEEP)`:
     - `mine` is `Rewritten`;
     - `Repo::open(<tmp>/mine)`'s `reload_at_working_copy_op()` succeeds;
     - `gone` is `Skipped` with a reason naming the missing checkout;
     - `default` is `ToHead`;
     - `operations_abandoned > 0`.
6. **`gc_repairs_a_root_an_earlier_gc_stranded`**: as test 1's setup, then `strand_like_0_1(root, KEEP)`. A fresh open's `reload_at_working_copy_op()` fails (the #94 state). Then `repo.gc(KEEP)`:
   - `operations_abandoned == 0` (nothing older is left);
   - `default` is `ToHead { to: head }`;
   - the checkout names the head;
   - `reload_at_working_copy_op()` succeeds;
   - with `jj_cli()`, `jj st` exits 0.
7. **Updated `gc_now_prunes_hidden_commits_and_keeps_the_view`**: lines 101 to 106 become "GC moved the working copy itself". The assertions are `outcome.checkouts == [default, from the head before, Rewritten { to: outcome.new_head }]` (the root's operation was the head) and `repo.working_copy_op_id() == outcome.new_head` on the same `Repo` (the reload). The snapshot after GC stays.
8. **Updated `stranded_working_copy_is_rebound_only_when_stranded`**: it strands `default` with `strand_like_0_1(root, SystemTime::now())` instead of `repo.gc(now)`, then asserts as now. It still tests `rebind_if_stranded`, which Reef workspaces use.

A unit test in `src/jj/gc.rs`, `reparent_mapped_matches_jj`: on a repository with five operations, `reparent_mapped` and `op_walk::reparent_range` over the same range and destination return the same new head id and the same counts. Every value in the map is an operation whose view id equals its key's.

### `crates/reef-daemon/tests/gc.rs`

The root is built by the test before registration: `TestDaemon::start_with` keeping the default `undo_window` of 7 days, then `synth::generate(<tmp>/repo, Small)`, `Repo::init_internal_git` with backdated settings and two snapshots, `age_op_store`, a recent root snapshot of `root.txt` (the root's checkout), and `client.register_repository(&root, IfNotJj::Fail)`. The backdating helpers are copied into the test file. The watcher is paused as in `fixture_with`.

9. **`root_jj_works_after_gc`**, the issue's scenario end to end (fails on main):
   - Setup: one Reef workspace created, written and snapshotted. Record the root's `@` (`open(root).wc_change()`) and `root.txt`'s bytes.
   - `gc` reports `jj_error == ""`.
   - On a fresh `Repo::open(root)`:
     - `reload_at_working_copy_op()` succeeds;
     - `wc_change()` is the recorded `@`;
     - `root.txt` has its bytes;
     - `op_count` is the recent operations plus the root.
   - The Reef workspace's next write snapshots, as test 10 asserts.
   - With `jj_cli()`: `jj st` and `jj op log` in the root exit 0, and `op_count` is the same after `jj st`.
10. **`gc_abandons_nothing_while_the_root_is_locked`**:
    - As test 9, with the test holding the root's `working_copy.lock`.
    - `gc` reports a `jj_error` containing "no operations were abandoned", "jj workspace `default`", the root's path and "locked".
    - `op_count` is unchanged, and the Reef workspace still snapshots.
    - After dropping the lock, `gc` reports `jj_error == ""` and `op_count` drops.

Every other existing test passes unchanged, in particular `stranded_working_copy_is_repaired_at_start`, `active_workspace_snapshots_after_gc`, both subscription GC tests, `jj_writes.rs`'s attributes test, and `hostile_repo.rs`.

## Implementation notes

- **Order:**
  1. `reparent_mapped` with its unit test;
  2. the hold, classify and write pass with tests 1 to 6;
  3. the two updated tests;
  4. the daemon's report and logs with tests 9 and 10;
  5. help, doc comments and documents.
- **No `start_mutation` or `rebind_working_copy` on a held workspace.** Both take jj's blocking lock, which conflicts with Reef's own `try_lock` in the same process and would hang the repository thread. Read with `LocalWorkingCopy::load` and write with `write_checkout`.
- **The lock wait** runs on the repository thread, in the GC job: `std::thread::sleep` between tries is fine there and nowhere else.
- **`closest_common_ancestors`** for a stranded X may walk the whole history before it fails or finds the root operation. It runs only for workspaces outside the range, once per GC.
- **The operation file path** is `<repo_path>/op_store/operations/<hex>`, SimpleOpStore's layout. Reef already depends on that store's layout in backup (`maintenance/backup.rs:545`). Remove it only when the store is `SimpleOpStore` (`op_store().name() == "simple_op_store"`); otherwise log and leave it.
- **Linux:** nothing is `cfg(target_os = "linux")`. Run `cargo nextest run -p reef-lib --test jj_gc` and `-p reef-daemon --test gc` in the privileged `ubuntu:24.04` container once before pushing (`docs/benchmarks.md`, "Linux"); `flock` semantics are what the lock test relies on.
- **The jj CLI** is not in CI, so its assertions run where `jj` 0.46 is on `PATH`, as `jj_cli_lists_reef_workspace` does. The `reload_at_working_copy_op` assertions are what CI enforces.

## Acceptance criteria

- [ ] After `reef ws gc` with the default undo window, `jj` in a root that ran `jj` within the window works with no `update-stale`, and its `@` and files are unchanged (tests 1, 9; manually with `jj st`).
- [ ] A root whose operation GC abandons moves to the head when clean, and is left to jj's recovery when stale (tests 2, 3); a root a 0.1 GC stranded is repaired or handed to jj by the next GC (test 6).
- [ ] A user's `jj workspace add` workspace is moved the same way; one whose directory is gone does not stop GC (test 5).
- [ ] While a `jj` command holds a workspace's lock, GC abandons nothing and says so in `jj_error` (tests 4, 10).
- [ ] Reef's own workspaces are rebound as before (existing tests 10 and `stranded_working_copy_is_repaired_at_start`).
- [ ] No schema change: `API_REVISION` 5, digest unchanged.
- [ ] ADR 0040 written and indexed, ADR 0031's "Amended by" line, help text, doc comments, CHANGELOG and the phase README updated; `just ci` passes; the Linux container run passes.

## Verification

```sh
cargo nextest run -p reef-lib --lib gc
cargo nextest run -p reef-lib --test jj_gc --test jj_writes --test jj_workspace
cargo nextest run -p reef-daemon --test gc --test backup --test hostile_repo
cargo nextest run -p reef-api
just ci
# By hand, in a registered checkout with history older than a week:
reef ws gc && jj st && jj op log --limit 3
```

## Inherited scope

- **From R1.17:** the four steps, `keep_newer`, the rewrite onto the root operation, `settle` and the callback's rebind of Reef's workspaces, and "no operations were abandoned" when a workspace cannot be kept safe. This ticket adds the user's jj workspaces to what GC keeps safe, and drops R1.17's "the user's `default` workspace is not touched".
- **From R2.09 and ADR 0036:** the callback's position and subscriptions' reading of old copies, which is why option 2 is used only for operations nothing can reach.
- **From ADR 0031:** `default` is never written by Reef's writes; GC now writes its operation pointer only (ADR 0040).

## Out of scope

- A `jj` command that writes an operation while GC runs, outside the working-copy lock (its transaction commits between its snapshot and its check-out). It can still make a child of the old head after the op heads move, and lose that child's old ancestors to the collection. This race predates the ticket, and jj's own `jj op abandon` plus `jj util gc` have it too.
- `reef-*` jj workspaces in the view with no active row (a release whose forget failed); they are left as today.
- Upstream: jj's `op abandon` moves only the workspace it runs in, so jj has #94 for its own secondary workspaces. Not filed.
- Running the jj step on a timer (#95).
- Installing the `jj` CLI in CI (open question 2).

## Known limits

- **A stale root whose operation GC abandons** gets jj's recovery commit at its next `update-stale` instead of an ordinary stale update. This needs the root idle in jj for longer than the undo window and its `@`'s tree changed meanwhile.
- **A workspace jj's store has no path for** (created before jj kept one, or moved by hand) is skipped and can still break as before; GC logs it.
- **A `jj` command in the root waits** while GC rewrites the log: one file write per kept operation.
- **A busy lock defers the whole abandon**; on #95's timer that means the next interval.
- **Reef depends on jj's on-disk `checkout` format and op store layout** and on a copy of `reparent_range`; a jj-lib upgrade re-checks all three (the unit test catches a drift in the copy).

## What later tickets inherit

- **#95** can run the jj step on a timer: a run that meets a busy lock abandons nothing and logs why, and the next run tries again.
- **Any later rewrite of the operation log** (a future `jj op abandon` equivalent, R4's backend) moves the user's workspaces the same way by reusing `hold_checkouts`, `classify` and `write_checkout`.

## Open questions

1. Should `GcReport` list the jj workspaces GC moved or left for recovery (a repeated message, API revision 6)? Default: no. Logs carry them, `jj_error` carries the case the user must act on, and jj's own message covers recovery.
2. Should CI install `jj` 0.46.0 so the CLI assertions run there? Default: not in this ticket; the jj-lib assertions already reproduce #94's failure (`load_at` of the working copy's operation).
3. Should GC refuse to abandon past a stale root instead of handing it to jj's recovery? Default: no, since that blocks GC forever for a root nobody runs `jj` in. The cost is a recovery commit in a rare case.
4. Should the lock wait (2 s) be configurable? Default: no; it bounds a `jj` snapshot in progress, not a policy.
