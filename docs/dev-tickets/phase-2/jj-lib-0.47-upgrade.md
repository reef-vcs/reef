# jj 0.47 Reef builds on jj-lib 0.47.0

Phase 2 · Size S (no row; see "Size") · Fixes: no issue yet · Depends on: RX.01 (the tree-name guard, S-18), R1.17 (GC), R2.02 (revsets and diffs), R2.04 (file edits) · Unblocks: none listed (a follow-up can narrow snapshots through jj-lib's new `replace_fsmonitor_settings`, see "Out of scope") · Ships as 0.4.2, a patch release cut as soon as this merges (user, 2026-11-04), for the revset crash below

## Context

Reef pins `jj-lib = "=0.46.0"` and the `gix` it depends on, `=0.88.0` (`Cargo.toml:38-41`; ADR 0002 decision 3, ADR 0008). jj 0.47.0 was released on 2026-11-04 and jj-lib 0.47.0 reached crates.io the same day. It carries the two upstream fixes this project made after RX.01: jj-vcs/jj#10467 (`GitBackend::read_tree` returns an error instead of unwrapping `RepoPathComponentBuf::new` on a tree entry name with `/` or an empty one; merge `79b2c22`) and jj-vcs/jj#10501 (`get_copy_records` returns an error instead of panicking on a rename path it cannot represent; merge `22ac96b`). It also moves jj from chrono to jiff, splits four crates out of jj-lib (`jj-default-index`, `jj-dsl`, `jj-git-backend`, `jj-local-util`), removes `Workspace::load`, turns `MergedTree`'s iterators into streams, and rejects deeply nested revsets before they overflow the stack (jj-vcs/jj#10071).

This ticket moves Reef to jj-lib 0.47.0 and gix 0.90.0, ports every call site the compiler names (13 API changes, about 80 changed lines in `reef-lib/src` and 200 in tests across both crates), replaces Reef's one use of chrono with jiff, and rewrites what Reef says about the 0.46 panics: the doc comments, the three hostile-repository tests that asserted a contained panic, and threat-model row S-18. It adds two tests for what 0.47 now does instead of panicking (rename detection, and a deeply nested revset). The RX.01 guard stays (question 1). There is no API, schema, CLI or output change: `API_REVISION` stays 14 and `reef.sqlite3` stays at schema 6. On-disk formats under `.jj` do not change either (see below), so the user's `jj` 0.46 CLI keeps working in a repository Reef 0.47 writes to.

### What was verified while writing this ticket

All of this was checked at f3c4f8c (main, 0.4.1), with jj-lib 0.47.0 from crates.io and jj's tags `v0.46.0` and `v0.47.0`. A trial port in a scratch worktree (not committed) compiled with `cargo check --workspace --all-targets`, passed `cargo clippy --workspace --all-targets --all-features -- -D warnings` and `cargo deny check licenses bans sources` on macOS, and ran the test suites named under "Tests". Paths are under `crates/` unless they start with `docs/`.

- **The versions.** jj-lib 0.47.0 declares `rust-version = 1.97.1` (Reef's, `Cargo.toml:23`), `gix ^0.90.0` (jj's lockfile has 0.90.0, so the pin is `=0.90.0`), and `jiff ^0.2` with `serde` in place of `chrono ^0.4.44`. Its `default` feature is `git` (`dep:gix`, `dep:jj-git-backend`); `watchman` is unchanged. Its gix features are those of 0.46 with `max-performance` (now `= ["max-control"]` in gix 0.90, no C code) instead of `max-performance-safe`, and `auto-chain-error` added.
- **The lockfile.** `cargo update -p jj-lib -p gix` after the two pins moves every `gix-*` crate to its 0.90 release, adds `jj-default-index`, `jj-dsl`, `jj-git-backend`, `jj-local-util` (all 0.47.0) and `gix-parallel` 0.50.1, removes `gix-features`, and changes nothing outside the jj and gix families. `jiff` 0.2.38 was already in the lockfile (through `gix-date`). `chrono` stays in it after reef-lib drops it: `rmcp` and `schemars` depend on it (`cargo tree -i chrono`). `cargo deny check licenses bans sources` passes. `cargo deny check advisories` fails on `pulldown-cmark` 0.13.4 (through `prost-build`), identically on main: not this ticket.
- **Module paths do not move.** jj-lib 0.47 re-exports the split-out modules under their old names (`pub use jj_core::backend`, `pub use jj_default_index as default_index`, `pub use jj_git_backend::git_backend`, `pub use jj_local_util::file_util`, …, `jj-lib-0.47.0/src/lib.rs:21-119`), so `jj_lib::…` imports keep working. The one item that moved without a re-export at its old path is `MergeOptions`, from `jj_lib::tree_merge` to `jj_lib::files`, and Reef no longer needs to name it (row 9 below).
- **Every compile error, and its fix.** In the order `cargo check` reports them. Line numbers are main's.

  | # | 0.46 | 0.47 | Reef's call sites | Port |
  | --- | --- | --- | --- | --- |
  | 1 | `RepoLoader::load_at_head()`, `ReadonlyRepo::reload_at_head()` | take `Option<&WorkspaceName>`, the workspace a merge of divergent operation heads is attributed to (jj `63d5231`) | `reef-lib/src/jj/gc.rs:280`, `:383`, `:420`, `:464`, `:1115` (test); `restore.rs:119`; `repo.rs:427`, `:501`; tests below | `None`. 0.46's body passed `None` itself (`jj-lib-0.46.0/src/repo.rs:808`), so behaviour is unchanged |
  | 2 | `Workspace::load(settings, root, store_factories, wc_factories)` | removed (jj `871bd06`) | `repo.rs:421-426`; `reef-daemon/src/workspace/create.rs:1066` (test); tests below | `default_workspace_loader_factory().create(root)?.load(settings, &default_backend_factories(), &default_working_copy_factories())`, the removed function's body |
  | 3 | `Workspace::init_internal_git`, `init_colocated_git`, `init_external_git` | take `&WorkspaceName` before the last argument (jj `ad7435a`) | `repo.rs:280`, `:304`, `:344`; `reef-lib/tests/telemetry.rs:89`, `jj_spike.rs:41`, `:47` | `WorkspaceName::DEFAULT`, the name 0.46 used inside `init_with_factories` (`workspace.rs:369`) |
  | 4 | `Workspace::init_workspace_with_existing_repo(…, name)` | adds `operation_workspace_name: Option<&WorkspaceName>` (jj `d61348f`) | `reef-lib/tests/jj_gc.rs:603`, `:720` only | `None`: 0.46 attributed the operation to no workspace |
  | 5 | `GitBackend::gc(&dyn Index, keep_newer)` (inherent, sync) | `Backend::gc(&[CommitId], keep_newer)` (trait, async; jj `8498d8f`, `3a8b5b2`) | `gc.rs:396` | collect `self.repo.readonly_index().as_index().all_heads_for_gc()` (mapping `IndexError` to `BackendError::Other`, as 0.46's body did) and `Backend::gc(git, &heads, keep_newer).await?`, in a new private `Repo::git_gc` (inline, the port puts `gc_unlogged` at 104 lines, over clippy's `too_many_lines` limit of 100). 0.46 filtered the root commit out inside; 0.47 still does (`git_backend.rs:1564-1567`) |
  | 6 | `RevsetParseContext::date_pattern_context` (from a `chrono::DateTime<Local>`) | `current_time: jiff::Zoned` (jj `fd68202`) | `revset.rs:349`, `:361`, `:393` | `now: &jiff::Zoned`, `current_time: now.clone()`, `jiff::Zoned::now()`. One instant for both probe parses, as today |
  | 7 | `MergedTree::diff_stream`, `diff_stream_with_copies` | renamed `diff`, `diff_with_copies` (jj `c5752e5`) | `diff.rs:664`, `:704`, `:730`; `files.rs:1462`; `snapshot.rs:1036`, `:1140` | rename |
  | 8 | `MergedTree::entries()`, `entries_matching()`, `conflicts()` return iterators | return streams that read trees concurrently (jj `4360614`, `03ff3b3`, `3ee3b29`) | async callers `files.rs:408`, `:756`; sync callers `files.rs:1437` (`walk_files_in`), `repo.rs:488` (`tracked_file_count`), `diff.rs:806` (`conflicted_paths`) | async: `let mut entries = …; while let Some((path, value)) = entries.next().await`. Sync: `futures::executor::block_on_stream(…)`, which keeps the early `break`s; reef-lib already depends on `futures`. See "Porting rules" |
  | 9 | `jj_lib::tree_merge::MergeOptions::from_settings(settings)` | `UserSettings::merge_options()` (jj `58e08ed`, `29eda03`) | `diff.rs:26`, `:352` | `settings.merge_options()`; drop the import |
  | 10 | `Commit::is_empty(&dyn Repo)`, `parent_tree(&dyn Repo)` | take `&dyn Index` (jj `86b0b4d`, `0b32255`) | `log.rs:189`; `snapshot.rs:1034` | `repo.index()`, `self.repo.index()` |
  | 11 | `MergedTreeBuilder::set_or_remove(path, MergedTreeValue)` | `set_or_remove(path, Option<TreeValue>)`; the merge form is `set_or_remove_merge(…) -> Result<(), ConflictArityMismatchError>`, which refuses a conflicted value whose arity differs from the base tree's and asserts the path is not the root (jj `340fc76`, `35df433`, `5a114b9`) | `files.rs:1172-1187` (`apply`); `reef-lib/tests/jj_writes.rs:189`, `jj_reads.rs:534`, `jj_files.rs:177`, `:1184`, `materialize.rs:169`; `reef-daemon/tests/reads.rs:318` | `set_or_remove` with `None` for `Pending::Absent` and `Some(value)` otherwise: every value `apply` writes is resolved, and `check_path` (`files.rs:61-79`) already refuses an empty path. `materialize.rs:169` built a three-term conflict over an empty base, which the arity rule now refuses: build the two sides with `set_or_remove` and join them with `MergedTree::merge_no_resolve(Merge::from_vec(vec![(left, label), (empty, label), (right, label)]))` |
  | 12 | `repo.rs:558` names `jj_lib::ref_name::WorkspaceName::new` in full | `unused_qualifications` once row 3 imports `WorkspaceName` | `repo.rs:558` | `WorkspaceName::new` |
  | 13 | `JJ_LIB_VERSION = "0.46.0"` | `jj::tests::jj_lib_version_matches_the_lockfile` fails | `reef-lib/src/jj/mod.rs:15` | `"0.47.0"`. It is recorded in backup manifests (`reef-daemon/src/maintenance/backup.rs:239`); restore does not compare it |

  The test call sites of rows 1, 2 and 11 outside reef-lib are all fixtures that open the repository with jj-lib directly: `reef-daemon/tests/broker.rs:188`, `:195`, `:214`; `reads.rs:61`, `:68`, `:318`; `replay.rs:63`, `:70`; `stores.rs:1087`, `:1094`; `subscribe.rs:221`, `:228`, `:437`, `:444`, `:722`; `workspaces.rs:262`, `:269`; and in reef-lib `jj_gc.rs:92`, `:181`, `:216`, `:606`, `:723`; `jj_reads.rs:1858`; `jj_replay.rs:39`; `jj_workspace.rs:122`, `:131`; `jj_writes.rs:811`; `materialize.rs:149`, `:156`. Each `Workspace::load` port leaves its `use jj_lib::workspace::Workspace;` unused (`reads.rs:59`, `workspaces.rs:259`, `create.rs:1059`, `jj_workspace.rs:25`, `materialize.rs:24`), and row 11 leaves `use jj_lib::merge::Merge;` unused in `jj_reads.rs:514`, `jj_files.rs:17` and `reads.rs:307`. `reef-vcs`, `reef-testing`, `reef-client`, `reef-api`, `reef-broker`, `reef-symbols` and `reef-web` need no change. With the ports (row 5's in its own function), clippy with `--all-features -D warnings` and `cargo fmt` are clean.
- **Code Reef copied from jj-lib.** `write_checkout` (`gc.rs:997-1023`) mirrors `CheckoutState::save`, now at `local_working_copy.rs:2708` in 0.47. The only difference in 0.47 is that jj creates the temp file with default permissions (`new_temp_file_with_default_permissions`, jj `9a26de2`) instead of 0600; Reef already sets the old file's mode and owner, so nothing changes. `reparent_mapped` (`gc.rs:1035-1095`) mirrors `op_walk::reparent_range`, now at `op_walk.rs:260`: the function is byte-identical to 0.46's, and `walk_ancestors_range`, which it calls, moved to jj-core with only a closure extracted. `reparent_mapped_matches_jj` passes.
- **On-disk formats are unchanged.** The only `.proto` files that moved are `default_index.proto` and `git_store.proto`, renamed into their new crates at 100% similarity. The commit index's format version is 6 in both (`COMMIT_INDEX_SEGMENT_FILE_FORMAT_VERSION`, `default_index/readonly.rs:131` and `jj-default-index-0.47.0/src/readonly.rs:131`), and the changed-path index's 0 in both. The op store now serializes map fields and view heads in sorted order (jj `688ca39`, `a6d4a5e`): the bytes, and so a new operation's id, can differ from what 0.46 writes for the same content, and 0.46 reads them as before. With the trial on jj-lib 0.47 and the jj 0.46.0 CLI on `PATH`, the CLI-gated tests ran and passed (`jj_gc`'s `jj st`, `jj workspace update-stale` and `jj op log` after a Reef GC; `jj_workspace::jj_cli_lists_reef_workspace`). Files jj 0.47 creates under `.jj` get the umask's mode instead of 0600 (jj `9a26de2`); Reef's state directory is 0700 (S-01), and a user's `.jj` was the user's own already.
- **The two upstream fixes, as Reef sees them.** With the trial, the hostile tree of `hostile_repo` test 12a (an entry `a/reef-escaped-canary` in a root tree) fails `GetDiff` with `INTERNAL` and the message `failed to read the trees being compared: Error when reading object 1efe…32d8 of type tree: Invalid repo path input "a/reef-escaped-canary"`, every time, with no panic and no reopen (`RepoHandle::generation()` stays 0). A rename into an entry named `/big.txt` inside a directory `d`, diffed with `paths: ["README"]` so the diff itself never reads `d`, fails `GetDiff` with `… of type commit: Invalid repo path input "d//big.txt"` from `get_copy_records`, again with no reopen. 0.46's panic on that second case is from S-18's text and jj-vcs/jj#10501, not rerun here.
- **What the hostile tests do with 0.47 unchanged.** 11 of 13 `hostile_repo` tests pass. Test 12 (`tree_entry_with_a_slash_does_not_take_the_repository_down`) and 12c (`registering_a_checkout_whose_head_jj_cannot_read_fails_cleanly`) pass: the guard refuses first. 12a (`a_read_of_a_tree_jj_cannot_read_fails_alone`, `:268`) fails on `contains("panicked")`, as its comment (`:243-244`) says it will. 12b (`a_panicked_job_reopens_the_repository`, `:320`) fails because `spec_at(&hostile)` no longer panics, so the job returns `Ok(())`.
- **Other 0.47 library changes that reach Reef.**
  - *Deeply nested revsets are refused while parsing* (jj `7642ab0`, `a86e122`; `MAX_TREE_DEPTH = 256`, `dsl/src/revset_parser.rs:63`). In 0.46 such an expression recursed in the parser until the stack overflowed (jj-vcs/jj#10071), and a stack overflow aborts the process. Checked: on main, `reef_lib::jj::revset::parse` of `"~".repeat(100_000) + "root()"` aborts the test process (`has overflowed its stack`, `SIGABRT`); with the trial, it and the two other shapes of test 3 return `InvalidArgument` and a flat union of 1,000 terms parses. Reef parses client revsets and revisions in the daemon (`reef-lib/src/jj/revset.rs:383`, and `resolve_revision` at `:596`) for `ListChanges` and `reef log -r`, `GetChange`, `GetDiff`, the file reads, `GetStatus`'s revision, a workspace's start revision and the MCP tools (not `QueryChanges`, which takes no revset; corrected in review), so on 0.46 one request can take the whole daemon down, including a remote reader's (S-40 bounds "unbounded revsets" only by call limits). The daemon-level abort was inferred from the library one, not run.
  - *Date patterns* that name a time on a day with a daylight-saving change (`after:"2024-03-10 03:30"`) use that time's UTC offset rather than the one at the start of the day, which put times after the change an hour off (jj `fd68202`).
  - *A merge commit with the root commit as a parent*: the git backend now drops the root parent instead of refusing to write the commit (jj `bf4ab70`). This reaches R2.03's abandon of a child of the root that a merge depends on, which now succeeds; no Reef test pins the old failure.
  - *Git ref import* compares the refs jj last imported with git's, rather than treating an export failure as an external change (jj `095c1ca`). Reef imports (`reef-lib/src/jj/git.rs:107`, `repo.rs:376`) but never exports, and R2.03's undo refuses to revert an import, so `import; undo; import` is unaffected.
  - *`LockedLocalWorkingCopy::replace_fsmonitor_settings`* (jj `79f0a0b`) lets a caller inject the changed paths of one snapshot. That is the upstream route ADR 0023 and ADR 0026 named for narrowing notify snapshots without Reef's own `TreeState` path; see "Out of scope".
- **The CLI-only breaking changes do not reach Reef.** Reef never runs `jj git fetch`, `push` or `export`, renders no jj template, and lists no bookmarks through jj's templater, so the release notes' four breaking changes (non-colocated fetch and push, non-colocated import after undo, `Timestamp.format()` on jiff's strftime, absent targets in bookmark lists) change nothing Reef prints.
- **chrono in Reef.** One production use, `revset.rs:349` and `:393` (row 6), and two test uses that print an RFC 3339 stamp for `debug.operation-timestamp` (`reef-lib/tests/jj_gc.rs:92`, `jj_replay.rs:39`). jj-lib 0.47 parses that setting with `time_util::parse_from_rfc3339`, which takes jiff's `Timestamp` display (`2026-11-04T18:00:00Z`) as it took chrono's `+00:00`; the two tests pass with `jiff::Timestamp::try_from(at).unwrap().to_string()`. No other crate names chrono.
- **Behaviour across the suites with the trial.** `cargo nextest run -p reef-lib`: 164 run, all pass once row 13 is applied. `-p reef-daemon`: 701 run, 700 pass; the one failure, `config::tests::empty_env_is_unset_and_relative_is_absolutized`, is `SocketPathTooLong` from the scratch worktree's 150-character path, not the bump. `workspaces::abandoned_create_leaves_its_base` was slow under the full run (65 s) and took 17 s alone. `hostile_repo`, with the rewrites below: 14 of 14. `reef-testing`'s `crate_graph`: 11 of 11. `RUSTDOCFLAGS="-D warnings" cargo doc --workspace --no-deps --document-private-items` is clean. `reef-vcs`'s tests were compiled, not run.
- **Linux-only code was not compiled.** macOS skips `cfg(target_os = "linux")`. That leaves `reef-daemon/src/backend/overlay/`, `backend/reflink.rs`, `backend/probe.rs`, `store/mount.rs`, `watcher/notify.rs`, `symbols/mod.rs`, `workspace/abandoned.rs`, `daemon.rs`, `reef-vcs/src/cli/holder.rs`, and the Linux-gated parts of `reef-daemon/tests/backend_conformance.rs`, `backup.rs`, `common/spawned.rs`, `disk_full.rs`, `inotify_instances.rs`, `integration.rs`, `overlay_restart.rs`, `snapshots.rs`, `status_index.rs` and `stores.rs`. None of the Linux-only blocks names a jj-lib item or calls any API in the table (searched by name across the tree, whatever the `cfg`), so none is expected to break; the implementation still checks them in the Linux container.

## Decisions

### Size

S, with no `dev-phases.md` row. It is a dependency bump the compiler drives: two pins, one dependency swapped, 13 mechanical ports (about 80 lines in `reef-lib/src`, under 10 in `reef-daemon/src`), about 200 lines of test fixtures ported, three hostile tests rewritten and two added, and five documentation edits. No design work, and nothing in the daemon's behaviour changes except that two hostile inputs now fail as errors rather than contained panics.

### The pins

`Cargo.toml`:

```toml
jj-lib = "=0.47.0"
# Only to name gix::hash::Kind when creating repositories (`Repo::init_internal_git` and test
# fixtures); jj-lib does not re-export gix. Must match the gix version jj-lib depends on exactly.
gix = { version = "=0.90.0", default-features = false }
```

and the chrono entry (`:52-54`) becomes:

```toml
# Revset date patterns (R2.02): jj-lib's `RevsetParseContext` takes the current time as a
# `jiff::Zoned`. The version jj-lib already uses, so the lockfile gains nothing.
jiff = "0.2"
```

with `reef-lib/Cargo.toml:37` `chrono.workspace = true` replaced by `jiff.workspace = true`. Default features, as jj-lib's: `Zoned::now()` needs the system time zone, which jiff's defaults read. Then `cargo update -p jj-lib -p gix`, and nothing else in the lockfile moves. Dependabot's ignore for both (`.github/dependabot.yml:44-50`) stays.

### Porting rules

- **Keep 0.46's behaviour wherever 0.47 adds a parameter.** Every new `Option<&WorkspaceName>` is `None` and every new `&WorkspaceName` is `WorkspaceName::DEFAULT` (rows 1, 3, 4), which is what 0.46 passed itself. Attributing a reconcile merge to a workspace, as jj's CLI does, would add a workspace name to operations Reef writes on load; the daemon loads outside any workspace, so it would be wrong anyway, and R2.03's `reef.*` attributes are not written on a reconcile.
- **Async callers await the stream; sync callers stay sync.** `walk_files_in` (the symbol index's walks, on their own threads), `tracked_file_count` (a repository job) and `conflicted_paths` (inside `GetStatus`) keep their signatures and drive the stream with `futures::executor::block_on_stream`. That is the same blocking as 0.46's iterator, which read each tree with `block_on` inside. Making them async is a refactor of their callers and is not part of this ticket. Reef never nests `futures::executor` (it drives jj with `pollster`), so `block_on_stream` cannot meet its re-entrancy panic.
- **No new `unwrap` outside tests.** `set_or_remove_merge`'s `Result` is avoided by row 11's `set_or_remove`; row 5 maps `IndexError` into `BackendError::Other`, which `GcError::Backend` already carries.
- **No wrapper for `Workspace::load`.** The two-call replacement appears once in `reef-lib/src` and eleven times in tests; a test helper in `reef-testing` would need jj-lib there, which it does not depend on. Each fixture spells it out.

### The RX.01 guard stays, as defense in depth

Question 1, decided A: `tree_names.rs`, `CheckedTrees` and their four call sites (`workspace/create.rs:635`, `workspace/stores.rs:435`, `:485`, `repo/registry.rs:376`) stay as they are. With 0.47 the guard no longer prevents a panic; it still gives a create, move, store build, GC start and registration `InvalidArgument` with "jj cannot read" and the entry's name before any work starts, where jj-lib would fail partway with an `Internal` read error. Only its documentation changes:

- **`tree_names.rs:1-11`** becomes: "Refusing trees jj-lib cannot read (RX.01). A git tree entry whose name is empty or contains `/` cannot become a jj path. git's porcelain never writes such a name, but a fetched commit can carry one. jj-lib before 0.47 unwrapped `RepoPathComponentBuf::new` on it in `GitBackend::read_tree`, panicking while it held the git backend's mutex and poisoning the store (jj-vcs/jj#10466); 0.47 returns `BackendError::ReadObject` instead (jj-vcs/jj#10467). [`Repo::check_tree_names`] walks a commit's trees with gix, outside jj's lock, so the daemon refuses such a commit with `InvalidArgument`, naming the entry, before a create, move, store build, GC or registration reads it, rather than failing partway with jj's read error."
- **`tree_names.rs:59-60`** (`readable`'s doc): "Whether jj-lib reads a tree entry named `name`: `RepoPathComponentBuf::new` refuses an empty name or one with `/`. …" (the rest unchanged).
- **`reef-daemon/src/repo/mod.rs:456-458`**: "jj-lib 0.46.0 declares" becomes "jj-lib (0.47.0) declares". The assertion compiles with 0.47.
- **`gc.rs:997-999`, `:1035-1038`**: the citations become jj-lib 0.47.0's `CheckoutState::save` (`local_working_copy.rs:2708`) and `op_walk::reparent_range` (`op_walk.rs:260`).

The repository thread's reopen after a panicked job (`repo/thread.rs:146-150`, `repo/mod.rs:51-102`, #63) also stays: it was always a backstop for any panic, not only this one.

### What the hostile tests assert now

- **Test 12** and **12c**: unchanged (the guard), but for test 12's comment, which says 0.46 panicked on the tree and 0.47 fails the read.
- **12a** (`a_read_of_a_tree_jj_cannot_read_fails_alone`): a read the guard does not cover fails with jj's error and takes nothing down. Keep `kind == Internal` (question 2, decided A), replace `contains("panicked")` with `!contains("panicked")` and `contains("Invalid repo path input")`, and add, after the loop, that the repository was not reopened: `f.daemon.handle.state().repos.get(id).unwrap().generation() == 0`. Its comment (`:239-245`) becomes: "12a: reads take any revision without the guard (R2.02). jj-lib 0.47 fails to read the tree (jj-vcs/jj#10467): the read fails with `INTERNAL`, nothing panics, the repository is not reopened, and the next read and create work."
- **12b** (`a_panicked_job_reopens_the_repository`): the backstop, with the panic injected. jj-lib no longer has a known panic to borrow, and nothing public holds the git backend's lock from outside, so the job panics on its own: keep `let _ = head.repo.spec_at(&hostile).await;` (it now reads the hostile tree and errors) and add `panic!("injected");` after it. Everything else stays and passes: `Panicked`, the next job, a create, the pre-panic workspace's snapshot on a fresh store, and GC after a second panic. Since nothing is poisoned now, those would pass without a reopen, so the test also asserts `handle.generation()` is 1 after the first panic's next job and 2 after the GC that follows the second (added in review; with the reopen disabled, the test fails there). Its comment (`:288-292`) becomes: "12b: the backstop. A job that panics (here on purpose, after reading the hostile tree) leaves no stale store behind: …". The `CANARY` tree stays, so the test still reads hostile content first.
- **12d** (new, `a_diff_whose_renames_jj_cannot_read_fails_alone`): the `get_copy_records` path S-18 describes, which no test covered. See test 7.

### Threat model

- **S-18** (`docs/threat-model.md:104`). Severity stays Medium: the input is the same, and the guard is what makes it clean. Rewrite the row's threat as of 0.46 ("panicked jj-lib 0.46 …; the poisoned mutex made every later read panic … until the daemon restarted"), and its mitigation to: the guard as now; then "Since Reef 0.4.2 (jj-lib 0.47, jj-vcs/jj#10467 and #10501), jj-lib returns an error for such a tree in `read_tree` and in `get_copy_records`, so a read RPC that meets one fails with `INTERNAL` (`Invalid repo path input`) without panicking, and nothing is reopened; the guard stays as defense in depth, for its `InvalidArgument` and the entry's name before any work starts." Keep the backstop sentences (reopen and #63) as the protection against any other panic. Drop "No jj release has it yet (0.46.0 is the latest); the guard stays until …" and "A fix for the `get_copy_records` unwraps is prepared but not yet submitted upstream." Tests: add `hostile_repo::a_diff_whose_renames_jj_cannot_read_fails_alone`.
- **Line 45** (the trust model's repository-content paragraph): "and a read RPC that meets one fails without taking the repository down (S-18)" stays true; no change.
- **S-40** (`:126`): add "a revset nested deeper than jj's parser accepts (256 levels) is refused as `InvalidArgument` before it is evaluated (jj-lib 0.47, jj-vcs/jj#10071); before 0.47 a deeply nested one overflowed the parser's stack, which aborts the daemon" and add test 3 to its tests. It is a fix to list, not a new row: S-40's severity and bounds otherwise stay.

### Documentation

- **`docs/quickstart.md:28`**: "If you use [jj 0.47](https://github.com/jj-vcs/jj/releases/tag/v0.47.0), the version Reef is built on, or 0.46, it works in your checkout alongside Reef."
- **`docs/quickstart.md:325`**: "Reef is built on jj-lib 0.47. A `jj` CLI from 0.46 or 0.47 works in a repository Reef uses; a newer `jj` may upgrade the repository's on-disk formats when you run it there ([ADR 0008](../../adr/0008-jj-lib-embedding-findings.md))." The 0.46 half is what the trial ran; a CLI older than 0.46 was not tried.
- **ADR 0008**: add `- Amended by: [jj 0.47](../dev-tickets/phase-2/jj-lib-0.47-upgrade.md) (the pin)` to its header, and in "Decision" change "We will continue building on jj-lib 0.46.0" to "We will continue building on the pinned jj-lib (0.46.0 at R0.04, 0.47.0 since this ticket)". Its revisit trigger ("a jj-lib bump breaks more than the `jj` module") is not met: outside `reef_lib::jj`, only test fixtures that use jj-lib directly and one comment changed. No new ADR. ADRs 0002, 0012, 0014, 0020, 0023, 0024, 0026, 0039 and 0040 cite 0.46 as what was true when they were written and stay as they are; ADR 0020's list of what jj-lib writes under `.jj/repo` is still right for 0.47.
- **`CHANGELOG.md`**: add `## [Unreleased]` above `## [0.4.1]`, with:
  - **Changed**: "Reef is built on jj-lib 0.47.0 (from 0.46.0). Nothing to migrate: jj 0.47 keeps 0.46's on-disk formats, so a `jj` 0.46 CLI keeps working in a repository Reef uses, and the API revision (14) and database schema (6) are unchanged. Date patterns in revsets that name a time on a day with a daylight-saving change (`after:"2024-03-10 03:30"`) use that time's UTC offset, not the one at the start of the day, which put times after the change an hour off."
  - **Fixed**: "A revset nested more than 256 levels deep is refused as an invalid argument; before, one could overflow the parser's stack and crash the daemon. A diff whose rename detection meets a git tree entry name jj cannot represent fails with an error instead of panicking the read, which reopened the repository ([jj-vcs/jj#10501](https://github.com/jj-vcs/jj/pull/10501)); so does a read of a tree with such an entry, which also panicked inside jj-lib, while holding its git backend's lock, before the repository was reopened ([jj-vcs/jj#10467](https://github.com/jj-vcs/jj/pull/10467)). Both now fail with `INTERNAL` and the path jj could not read."
- **This README's table**: the row `| jj 0.47 | [jj-lib-0.47-upgrade.md](jj-lib-0.47-upgrade.md) | S | RX.01, R1.17, R2.02, R2.04 | none listed |`.

### The jj CLI tests follow `JJ_LIB_VERSION`

Three tests run the `jj` CLI only when its version starts with `jj 0.46.` (`reef-lib/tests/jj_gc.rs:220-235`, `jj_workspace.rs:512-518`, `reef-daemon/tests/gc.rs:973-986`); otherwise they print "skipped" and pass. CI installs no `jj`, so they only run on a machine that has one. Question 3, decided A: each accepts the CLI whose minor is `JJ_LIB_VERSION`'s or the one before it, derived from the constant so the next bump does not touch them (one helper per file, since they are three crates' tests: `reef_lib::jj::JJ_LIB_VERSION` is public). The doc comments become "unless `jj` at jj-lib's minor or the one before is on `PATH`: its checkout and operation formats are what Reef writes for". The one-before case is the common one between a Reef release and Homebrew's `jj` catching up, and it is what the trial ran.

## Deliverables

- **`Cargo.toml`**, **`reef-lib/Cargo.toml`**, **`Cargo.lock`**: the pins and the jiff swap above.
- **`reef-lib/src/jj/`**: rows 1-13 in `gc.rs`, `restore.rs`, `repo.rs`, `revset.rs`, `diff.rs`, `files.rs`, `log.rs`, `snapshot.rs`, `mod.rs`; the doc comments of `tree_names.rs` and `gc.rs` above.
- **`reef-daemon/src/repo/mod.rs:457`** (comment) and **`reef-daemon/src/workspace/create.rs:1059-1073`** (a test module).
- **Tests**: the ports listed under "What was verified", the three hostile rewrites, tests 3 and 7 below, and the version gates.
- **Docs**: `docs/threat-model.md` (S-18, S-40), `docs/quickstart.md:28` and `:325`, ADR 0008, `CHANGELOG.md`, this README's row.
- **Unchanged**: the proto and `API_REVISION` (14), `reef.sqlite3`'s schema (6), the CLI and its JSON, `reef-vcs` (no jj-lib in its production code; its tests' `jj-lib` dev-dependency compiles unchanged), the guard's code, the SDKs, `dev-phases.md`.

## Tests

### `crates/reef-lib`

1. **`jj::tests::jj_lib_version_matches_the_lockfile`** passes with `JJ_LIB_VERSION = "0.47.0"`.
2. **Every existing reef-lib test passes with its fixture ported** (rows 1-4, 11 and the chrono swap), with no assertion changed. In particular `materialize::materialize_conflicted_tree_has_joined_key` (`tests/materialize.rs:141`) still asserts a three-term tree key from the conflict built with `merge_no_resolve`, `jj_gc`'s `reparent_mapped_matches_jj` passes unchanged, and `jj_replay`/`jj_gc`'s stamped settings load.
3. **`tests/jj_reads.rs`, new `deeply_nested_revsets_are_refused`**: on the existing fixture, `revset::parse(&head, text, &fx.scope())` for `"~".repeat(100_000) + "root()"`, `"(".repeat(100_000) + "root()" + &")".repeat(100_000)` and `"root()" + &"+".repeat(100_000)` each returns `RevisionError::Parse` with `kind() == InvalidArgument`, and a flat union of 1,000 `root()` (`vec!["root()"; 1000].join(" | ")`) parses. The test runs on a default test thread, so a regression aborts the test binary rather than passing. The trial ran it on 0.47 (passes) and on main (aborts).
4. **`jj::tree_names::tests::names_jj_cannot_read`**: unchanged, and passes.
5. **The CLI-gated tests** (`jj_gc`'s tests that call `jj(…)`, `jj_workspace::jj_cli_lists_reef_workspace`): pass with the jj 0.46.0 CLI on `PATH`, as the trial ran them, and with 0.47 once it is installed. Each prints which CLI it used, or why it skipped.

### `crates/reef-daemon/tests/hostile_repo.rs`

6. **12a and 12b** as rewritten above. Both passed in the trial with exactly those edits.
7. **12d `a_diff_whose_renames_jj_cannot_read_fails_alone`**: on `fixture()`, write `big` (a blob of `"renamed content\n"` × 200, so git's rename detection pairs it), a commit `base` on `main` whose tree is `README` and `big.txt` = `big`, and a commit `hostile` on `base` whose tree is `README` with new content and a directory `d` holding one file entry named `/big.txt` = `big` (with `raw_tree`, at `refs/heads/base` and `refs/heads/hostile`). Create on `base` (succeeds) and on `hostile` (refused by the guard, which imports the bookmark). Then `GetDiff { from_revision: "base", to_revision: "hostile", paths: ["README"] }` fails twice with `kind == Internal`, a message containing `Invalid repo path input "d//big.txt"` and not `panicked`; `generation()` is still 0; `GetDiff` from `main` to `base` succeeds; `f.assert_no_canary()`; `f.assert_alive()`; and a create on `main` works. The trial ran this fixture and got that message.
8. **Tests 11, 12, 12c, 13 and the rest of the file**: unchanged, and pass.

### Elsewhere

9. **Every reef-daemon test file with a ported fixture** (`broker`, `reads`, `replay`, `stores`, `subscribe`, `workspaces`, the `create.rs` unit test) passes with no assertion changed. `reef-daemon/tests/gc.rs`'s CLI check follows the version rule.
10. **`reef-testing`'s `crate_graph`** passes: the new `jj-*` crates hang only under jj-lib. `NOT_IN_THE_CLIENT`, `NOT_IN_THE_BROKER` and `NOT_IN_SYMBOLS` also list `jj-core`, `jj-default-index`, `jj-dsl`, `jj-git-backend` and `jj-local-util`, so a direct dependency on a split-out crate cannot bypass the `jj-lib` entry (added in review).
11. **Linux**: `cargo clippy --workspace --all-targets --all-features -- -D warnings` and the `privileged-tests` suites in the privileged `ubuntu:24.04` container (`docs/benchmarks.md`), since macOS does not compile the files listed under "What was verified".

## Implementation notes

- **Order:** the pins and `cargo update -p jj-lib -p gix`; `cargo check -p reef-lib` and rows 1-13 in table order; `cargo check -p reef-lib --all-targets` and its fixtures; `cargo check --workspace --all-targets` and the daemon's fixtures; the hostile tests and tests 3 and 7; the doc edits; then the Linux container. The compiler finds every site; the table is the map.
- **Build in the workspace's own target directory** or a fresh `CARGO_TARGET_DIR`, not a target directory shared with an older checkout: a shared one can keep stale paths and generated code (`crate_graph`'s "manifest path does not exist", a stale `--version`). Check free disk first.
- **Timing tests**: the daemon's lease, snapshot and `ws_run` tests flake under load on main too; compare a failure with main before blaming the bump.
- **No `cfg(target_os = "linux")` code changes** in this ticket, but it is compiled in the container (test 11).

## Acceptance criteria

- [ ] `Cargo.toml` pins `jj-lib = "=0.47.0"` and `gix = "=0.90.0"`; reef-lib depends on jiff, not chrono; `cargo update` moved only the jj and gix families (tests 1, 10).
- [ ] Every port keeps 0.46's behaviour; every existing test passes with fixtures ported and no assertion changed except the three hostile tests (tests 2, 8, 9).
- [ ] A read of a tree jj cannot read, and a diff whose rename detection meets one, fail with `INTERNAL` and no panic, and nothing is reopened (tests 6, 7). A deeply nested revset is `InvalidArgument` (test 3).
- [ ] The guard's behaviour is unchanged (tests 4, 8); its docs, S-18, S-40, the quickstart, ADR 0008 and the CHANGELOG say what 0.47 does.
- [ ] No API, schema or output change: `api_digest_matches_revision` passes at revision 14. `just ci` passes on macOS, and clippy and the privileged tests pass in the Linux container.

## Verification

```sh
cargo update -p jj-lib -p gix
cargo nextest run -p reef-lib
cargo nextest run -p reef-daemon --test hostile_repo
cargo nextest run -p reef-daemon
cargo nextest run -p reef-testing --test crate_graph
just ci
```

By hand, with the built daemon and the `jj` 0.46 CLI: in a scratch colocated repository, `reef ws run -- sh -c 'echo x > f'`, then `jj st`, `jj op log --limit 5` and `jj workspace list` in the root work and show the Reef workspace's change.

## Out of scope

- **Removing the guard** (question 1's option B) and changing the kind of an unreadable-tree read (question 2's option B).
- **Narrowing notify snapshots through `replace_fsmonitor_settings`** (jj `79f0a0b`), which could replace ADR 0026's `TreeState` path inside Reef's own lock with jj's `LockedLocalWorkingCopy`. A follow-up issue, with its own measurement against R1.30's load test.
- **Making `walk_files_in`, `tracked_file_count` and `conflicted_paths` async**, to read trees concurrently on their callers' runtimes.
- **The `jj` 0.47 CLI's own changes** (templates, `git fetch`/`push` in non-colocated repositories): Reef does not use them.
- **`pulldown-cmark`'s advisory**, which fails `cargo deny check advisories` on main as well.

## Decisions for the user

All three were decided by the user on 2026-11-04, each as recommended (A). The user also decided that this ships as 0.4.2 as soon as it merges, because a nested revset can abort a 0.4.x daemon.

1. **The RX.01 guard after 0.47.**
   - A (recommended): **keep it as defense in depth**, as "The RX.01 guard stays" describes. A create, move, store build, GC or registration on such a commit stays `InvalidArgument`, "jj cannot read" and the entry's name, before any work. It costs one gix walk of each tree not seen before, cached per repository (`MAX_CHECKED`, about 30 MiB); that cost was not measured on the 277k-file monorepo of #188. jj-lib may have other unwraps on hostile input that nobody has found; the guard does not cover those, but the reopen backstop does.
   - B: **remove it** (`tree_names.rs`, `CheckedTrees` and its plumbing in `repo/mod.rs`, `repo/thread.rs`, `workspace/create.rs`, `workspace/stores.rs`, `repo/registry.rs`, about 250 lines with the error variants). Those operations then fail with jj's `INTERNAL` read error partway through: a create after the base checkout started, a registration after jj's init began (`init_in_git_repo` removes the `.jj` it made when a step fails, so 12c's "no `.jj`" should still hold, but not checked). Tests 12 and 12c would assert `Internal` and `Invalid repo path input`, `names_jj_cannot_read` goes, and S-18's mitigation becomes jj's fix alone.
2. **The error kind of a read that meets an unreadable tree.**
   - A (recommended): **`INTERNAL`, as today.** 0.46's contained panic was `INTERNAL` too, so clients see no change, and the message now names the path.
   - B: **`INVALID_ARGUMENT`**, matching the guard: classify a `BackendError::ReadObject` whose source is jj's invalid-path error as `InvalidArgument` in `reef-lib`'s `Classify` impls for `DiffError` and the read errors. Clearer for an agent ("this revision's content cannot be read", not "Reef is broken"), but it changes an error kind clients may branch on, and the match is on a jj-lib error's type that may move again.
3. **Which `jj` CLI the CLI-gated tests accept.**
   - A (recommended): **jj-lib's minor or the one before** (0.47 and 0.46 now), from `JJ_LIB_VERSION`. Covers the pairing users have until Homebrew catches up, and it is what the trial verified.
   - B: **jj-lib's minor only** (0.47). Tests what Reef writes for exactly, but they skip on this machine until `jj` 0.47 is installed, and the 0.46 CLI against a 0.47 repository is then never exercised.
   - C: **leave `0.46.` hard-coded** until the next bump. No code, but the tests stop running once `jj` is upgraded.
