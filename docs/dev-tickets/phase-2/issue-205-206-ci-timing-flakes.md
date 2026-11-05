# Issues #205 and #206 Three timing flakes in the overlayfs job

Phase 2 · Size S (no row; see "Size") · Fixes: issues #205 and #206, and one flake with no issue (`files::edit_files_moves_workspaces`) · Depends on: R2.16 (the history index and its follower), R1.32 (background inotify attaches, ADR 0028), R2.03 and R2.04 (the write tests) · Unblocks: none listed

## Context

Three daemon tests failed once each in CI's `overlayfs (ubuntu-24.04, privileged)` job, on trees that did not touch them. None of them has failed on macOS.

| Test | Where it failed | What it saw |
| --- | --- | --- |
| A. `reef-daemon::history::throttle` (#205) | PR #204, the unprivileged step, [job 126415860873](https://github.com/reef-vcs/reef/actions/runs/41781873547/job/126415860873) | `history.rs:507`: `paths_changed` 0, expected 1 |
| B. `reef-daemon::writes::abandon_change` (#206) | main at f3c4f8c, attempt 2, the root step, [job 126462451795](https://github.com/reef-vcs/reef/actions/runs/41791823524/job/126462451795) | `writes.rs:440`: `ops[1].snapshot_kind()` `Watcher`, expected `Explicit` |
| C. `reef-daemon::files::edit_files_moves_workspaces` (no issue) | main at 93b4c50, attempt 1, the unprivileged step, [run 42065913051](https://github.com/reef-vcs/reef/actions/runs/42065913051) (job 127379808911); attempt 2 passed | `files.rs:815`: the two operations before the edit were V's snapshot then W's (newest first), expected W's then V's |

A and B/C have different causes. Both are in the tests, not the daemon:

- **A**: the test's last wait accepts the history index's row from before the 20 writes. The follower learns of the writes later.
- **B and C**: the fixtures in `writes.rs` and `files.rs` say their watcher "never snapshots on its own", but on Linux it does, once. A created workspace's inotify watch attaches in the background, and when it comes up the watcher sends an `everything` event straight to the scheduler. That event does not go through the debouncer, so the fixtures' 1 h debounce does not hold it back. The scheduler then takes a full watcher snapshot 1 s (`min_interval`) after the workspace's last snapshot. When that lands between the test's unsnapshotted write and its write RPC, it records the write, and the RPC's own explicit snapshot finds nothing to record.

This ticket fixes A's wait, and gives every "no automatic snapshots" fixture the one setting it is missing (`notify_attach_background = false`), through one shared helper. No daemon code changes, apart from two doc comments.

### What was verified while writing this ticket

All of this was checked at afce658 (main). Paths are under `crates/reef-daemon/` unless they say otherwise.

#### A. `history::throttle` (#205)

- **The test** (`tests/history.rs:486-509`). It sets the throttle to 5 s quiet and 60 s stale (`:493`), creates a workspace, and waits for the create's change to be `Current` (`:496`). It then makes 20 writes, each an explicit `SnapshotWorkspace` (`fx.write`, `:85-94`), and waits again for `Current` (`:504-506`), then asserts `paths_changed == 1` (`:507`).
- **What `Current` means.** `service/history.rs:214-218`: `Current` when the index row's `touched_commit` equals its `commit_id`. The touch worker picks rows where they differ (`history/touch.rs:57-95`, `next_due`) and sets `touched_commit` only for the commit it computed (`:236-249`).
- **The index learns of snapshots afterwards.** The follower reads the operation log when `OpEvents` wakes it, then sleeps `FOLLOW_GATHER` (200 ms) before it reads (`history/mod.rs:41-48`, `:293-352`). `SnapshotWorkspace` does not wait for it. Until the follower has read at least one of the 20 snapshots, the row still holds the create's commit, which the settle step touched. That row is `Current` with 0 paths, and the wait takes it on its first poll.
- **The CI timing agrees.** The failed run took 5.357 s. A passing run takes about 11 s (11.195 s in job 127379808911): 5 s quiet for the settle, the writes, then 5 s quiet for the last write's commit. 5.36 s is the settle alone. The last wait returned at once.
- **The field to wait on** is `HistoryChange.commit_id`, "the change's latest commit the index saw" (`crates/reef-api/proto/reef/api/v1/history.proto:163-164`). `fx.write` returns `SnapshotWorkspaceResponse.commit_id`, "the working-copy commit afterwards" (`workspaces.proto:534-535`), which is set even when the snapshot changed nothing.
- **The other waits in `history.rs`** were read for the same race. Only `throttle`'s last wait has it:

  | Line | Test | Wait | Follows writes? | Verdict |
  | --- | --- | --- | --- | --- |
  | `:192` | `follows_operations` | `operations >= 3` | yes | safe: needs the writes' activity |
  | `:204` | `follows_operations` | `operations >= 2` | yes | safe |
  | `:254` | `follows_operations` | bot-3 in `activity` | a describe | safe |
  | `:260` | `follows_operations` | `commit_id != child.commit_id` | a describe's rebase | safe: names the commit |
  | `:276` | `outcomes` | `!activity.is_empty()` | yes | accepts the create's row, but asserts only `Open`, which holds either way |
  | `:296`, `:305`, `:336`, `:350`, `:378` | `outcomes` | an outcome | ref moves, abandon, undo | safe: the outcome is the change |
  | `:417` | `symbol_touches` | `Current && symbols_changed > 0 && paths_changed == 2` | yes | safe (its comment names the race) |
  | `:467` | `symbol_touches` | `Current && symbols_changed >= 3` | yes | safe: the row before has 2 |
  | `:472` | `symbol_touches` | `Current && symbols_changed == 2` | yes | safe: the row before has 3 or more |
  | `:496` | `throttle` | `Current` | the create | harmless: any `Current` row will do (the issue says so too) |
  | **`:505`** | **`throttle`** | **`Current`** | **20 writes** | **the race** |
  | `:683`, `:687` | `backfill_and_restart` | `!activity.is_empty()` | a restart | safe: the assertions hold of any row with activity |
  | `:706`, `:719` | `gc_rewrite` | `operations == 2`, `>= 3` | yes | safe: counts the writes |
  | `:743` | `symbols_disabled` | `Current && paths_changed == 1` | yes | safe (its comment names the race) |
  | `:830`, `:851` | `recreate_and_forget` | `!activity.is_empty()` | yes | safe: only checks the row exists |

  No other test file waits on `TouchState::Current`.
- **One thing noticed in passing.** When computing a change's touches fails, the worker sets `touched_commit` and `touched_at` but leaves the old counts (`history/touch.rs:242-247`). So the row reads `Current` with the previous commit's `paths_changed`. That cannot cause this failure, which ended before a 5 s quiet time could pass, and the worker logs a warning when it happens. It is listed under "Out of scope" and filed as #214.

#### B and C: the extra watcher snapshot

- **What each test needs.** Both tests create W, write a file and snapshot W explicitly, create V on W's commit, write and snapshot V, then leave an unsnapshotted write in each. Then they call a write RPC that snapshots both first, descendants first (V, then W): `Manager::rewrite`'s step 3, `snapshot_moved` (`workspace/rewrite.rs:298-335`), in the order `rewrite::descendants_first` gives (`crates/reef-lib/src/jj/rewrite.rs:243-266`), as `SnapshotKind::Explicit`.
  - B (`tests/writes.rs:386-440`) expects W's newest operation before the abandon to be that explicit snapshot (`:439-440`).
  - C (`tests/files.rs:742-815`) expects the two operations before the edit to be W's snapshot, then V's, newest first (`:803-815`).
- **A snapshot that changes nothing records no operation.** `Manager::record` (`workspace/snapshot.rs:853-897`) returns `Recorded::Unchanged` with no commit when the scan changed nothing, and `snapshot_moved` skips it (`rewrite.rs:322-324`).
- **So one watcher snapshot of W explains both.** If a watcher snapshot of W ran after the test's unsnapshotted write in W and before the RPC paused W, it recorded that write as `Watcher`. The RPC's explicit snapshot of W then changed nothing and recorded nothing.
  - In B, W's operations, newest first, are then the abandon, the watcher snapshot and the setup snapshot. That is the failure: `ops[1]` is `Watcher`. `assert_ne!(w_snapshotted.commit_id, w_row.commit_id, "the snapshot ran")` (`:420`) still passes, because the watcher snapshot moved the commit.
  - In C, the two operations before the edit are then V's explicit snapshot and W's watcher snapshot, which is the order the failure shows. C's `w_ops.iter().any(... Explicit)` (`:790-794`) passes anyway, from W's setup snapshot.
  - A watcher snapshot of V instead does not fail either test: V's explicit snapshot is then skipped, and the order stays W's, then V's.
  - The other order the failure would also fit, the edit snapshotting W before V, is ruled out by `descendants_first`: V's commit has W's as an ancestor and W's has none of V's.
- **Where the watcher snapshot comes from: the post-attach `everything` event** (R1.32, ADR 0028):
  1. `Manager::create` tracks the workspace with `TrackReason::Created` (`workspace/create.rs:333`), so `defer` is true (`snapshot/mod.rs:1091`).
  2. On Linux, with `watcher.notify_attach_background` true (the default, `config.rs:390`, `:406`), `NotifyWatcher::watch` queues the attach and returns `Attach::Pending` (`watcher/notify.rs:278-292`). The scheduler marks the journal `NotWatched` (`snapshot/mod.rs:1156`; `snapshot/journal.rs:253-257`).
  3. A snapshot that takes the journal while the watch is not up leaves it `NotWatched` (`journal.rs:358-384`). Once the watch is up, the journal stays stale until the next full scan (`journal.rs:259-268`). So if the test's explicit snapshot of W ran before the attach, W's journal is still stale when the attach ends.
  4. The attach thread attaches W, reports it up, and queues an `everything` event (`notify.rs:459-505`; the comment at `:483-486` says why: "a scan that ran before it cannot vouch for a write made after"). The event is held until no attach is queued (V's, here) and is then sent by `Shared::send` on the scheduler's channel (`notify.rs:436-440`, `:517-531`, `:546-560`). It never passes through the debouncer, so `watcher.debounce` does not apply to it.
  5. The scheduler marks W dirty (`record_event`, `snapshot/mod.rs:1281-1299`). `start_due` starts a watcher snapshot of a dirty workspace once `min_interval` has passed since its last snapshot ended (`:1303-1343`, the arm at `:1328`). The fixtures leave `min_interval` at 1 s.
  6. `scheduled_snapshot` (`workspace/snapshot.rs:410-461`) does not skip it: there is an event, and the journal is stale (`:423-433`). It runs a full scan as `SnapshotKind::Watcher` (`:435`). An explicit snapshot that ran in between would have cleared the event (`take_trigger`, `take_reported`, `snapshot/mod.rs:881-910`), and a paused workspace is skipped (`:419`), so the window runs from the event to the RPC's pause.
- **Why only Linux.** macOS always attaches inline (`config.rs:383-389`; the module docs at `watcher/notify.rs:12-25`), and an inline attach sends no `everything` event (the unit test at `notify.rs:1530-1532`). Both failing steps ran on Linux: C as the runner user on fuse-overlayfs, B as root on kernel overlay. The CI root step runs exactly these two tests again as root (`.github/workflows/ci.yml:323`), which gives B and C a second chance each run.
- **Why rarely.** W's watch normally attaches before the test's first explicit snapshot of W. That scan then clears the journal, and the later event finds nothing to do. The failure needs the attach to finish after that scan, on a loaded runner, and then needs the snapshot, 1 s after W's last one, to land between the unsnapshotted write and the RPC. The steps in between (V's create, write and snapshot) take about that long on overlay.
- **The other candidates are ruled out:**
  - **A short retry after a failed snapshot.** There is no retry interval shorter than `fallback_interval`. A failed snapshot marks the journal `SnapshotFailed` (`journal.rs:387-393`). A stale journal is snapshotted when `fallback_interval` (60 s) has passed since the last snapshot ended (`snapshot/mod.rs:1319-1324`). The "short interval" in `scheduled_snapshot`'s doc comment (`workspace/snapshot.rs:407-409`) is that 60 s, short next to `watched_fallback_interval`. Both tests finish in 12 to 20 s and snapshot W several times on the way.
  - **A watch that fails to attach.** That marks `watch-failed` and leaves the workspace to the same 60 s timer (`snapshot/mod.rs:1139-1149`; `notify.rs:497-503`). It sends no event.
  - **Another path past the debouncer.** The tap journals paths before the debouncer but sends no event (`notify.rs:635-711`). An inotify overflow becomes an `everything` event inside the debounced batch (`route_batch`, `notify.rs:1040-1083`), so it waits for the debounce. `Scheduler::inject_event` is test-only. A move's rewatch attaches inline. The post-attach event is the only one.
- **Reproduced in the Linux container.** A temporary test (added to a scratch copy of `tests/writes.rs` in the `reef-linux-dev` container, never in this tree) used the `writes.rs` fixture's settings. It held attaches (`Scheduler::hold_attaches`), created W, wrote and snapshotted a file, wrote `late.txt`, released the hold, and polled W's operations for 8 s:
  - With `notify_attach_background` at its default, a `Watcher` snapshot of W appeared 1.20 s after the release as root (kernel overlay) and 1.09 s after as the `dev` user (fuse-overlayfs).
  - With `notify_attach_background = false`, none appeared in 8 s, as root or as `dev`.
  - The unchanged tests also failed on their own. As `dev`, 25 runs of a filter of 11 `writes.rs` and `files.rs` tests at once (`abandon_change`, `edit_files_moves_workspaces`, and the other abandon, undo and edit tests beside them, for load) gave 24 passes and one failure: `abandon_change` at `writes.rs:440`, `left: Watcher`, `right: Explicit`, the CI failure exactly. `edit_files_moves_workspaces` passed all 25. The fixed tests were not looped (it took about 75 s a run), so that is the implementer's to do (see "Verification").
  - `throttle` was not reproduced. The CI durations above are the evidence for A.
- **This is not a daemon bug.** The full scan after a background attach is ADR 0028's promise that a write made after `Create` returns is in the workspace's first watcher snapshot. It must not wait for a debounce, since no event may ever name the write. `watcher.debounce`'s doc comment ("Quiet time after the last event before a snapshot", `config.rs:335-336`) does not mention it, and neither does the fixtures' comment. That is the gap.
- **Three fixtures already turn it off for this reason.** `tests/narrowed.rs:54-57` ("A Linux background attach (R1.32) ends in a full scan, which would take the first write after a create"), `tests/status_index.rs:56-58`, and `tests/snapshots.rs:640-642`.
- **The same gap elsewhere.** Every daemon config below, a file's fixture or one test's own `TestDaemon`, sets `debounce` and `watched_fallback_interval` to 1 h, leaves `fallback_interval` at 60 s and `notify_attach_background` at true, and so can see one watcher snapshot after a create on Linux. `api_v1.rs:365` even says why it sets them: "No watcher snapshot races the explicit ones." None of these tests is Linux-only, and none waits for an attach or reads `watch_state` while attaching (`replay.rs:329` reads it at an operation, where it is unset). So all of them already pass with inline attaches, on macOS:
  - `crates/reef-daemon/tests/`: `api_v1.rs:366`, `broker.rs:33`, `files.rs:36`, `history.rs:38`, `presence.rs:34`, `reads.rs:38`, `remote.rs:746`, `replay.rs:37`, `subscribe.rs:34`, `symbol_diff.rs:46`, `symbols.rs:59`, `web_ui.rs:612`, `writes.rs:34`.
  - `crates/reef-vcs/tests/`: `mcp.rs:60`, `presence.rs:44`, `remote.rs:110`, `schema.rs:233`, `vcs.rs:32`, `vcs.rs:2315`.

  Some of these also assert snapshot kinds (`reads.rs:162`, `api_v1.rs:344` in `writes_carry_their_operation`). B and C are the two that leave an unsnapshotted write in a workspace created a second or so before, then check what recorded it, which is the window the post-attach scan lands in. The others have not failed so far.
- **The fixtures also break a config rule.** `validate` requires `debounce < fallback_interval` (`config.rs:1740-1747`). A 1 h debounce with the 60 s default breaks it. `TestDaemon::start_with` (`src/testing.rs:50-70`) does not validate, so nothing has noticed.

## Decisions

### Size

S, with no `dev-phases.md` row. One test's wait (two lines), one helper in `src/testing.rs`, 19 daemon configs (in 18 test files) switched to it, two doc comments, and two new tests.

### A: the wait names the commit

In `throttle`, keep the last write's commit and wait for the row at that commit:

```rust
let mut last = String::new();
for i in 0..20 {
    last = fx.write(&ws, "a.txt", &format!("{i}\n")).await;
}
...
let row = fx
    .change_when(&ws.change_id, |c| {
        c.commit_id == last && c.touches_state() == TouchState::Current
    })
    .await;
assert_eq!(row.paths_changed, 1);
```

Not `paths_changed == 1` in the wait: as #205 says, a regression would then show as a 20 s timeout, not as this assertion. The settle wait (`:496`) stays as it is. No other wait in `history.rs` changes (see the table).

### B and C: one helper for "no automatic snapshots"

Add to `reef_daemon::testing` (`src/testing.rs`, under the `testing` feature like the rest of it):

```rust
/// Turns off every automatic snapshot, so a test's snapshots are its own:
/// no debounced watcher snapshot, no fallback or full-scan timer, and no
/// full scan after a Linux background attach (R1.32, ADR 0028), which is
/// not debounced. Watches attach inside `Create`, as on macOS.
pub fn no_automatic_snapshots(config: &mut Config) {
    let watcher = &mut config.file.watcher;
    watcher.debounce = Duration::from_hours(1);
    watcher.fallback_interval = Duration::from_hours(2);
    watcher.watched_fallback_interval = Duration::from_hours(2);
    watcher.notify_attach_background = false;
}
```

- **`notify_attach_background = false`** is the fix. An inline attach sends no `everything` event, and the journal starts fresh, so nothing makes the workspace dirty until the debouncer does.
- **`fallback_interval = 2 h`** closes the 60 s timer too. With an inline attach that comes up, a fixture's journal is fresh and that timer does not apply. It applies only after a watch fails or a snapshot fails, both of which log. These tests take 12 to 20 s, so 60 s has not been hit, but a loaded runner has taken 20 s already. 2 h also keeps `debounce < fallback_interval`, so the helper's config would pass `validate`.
- **`watched_fallback_interval`** goes to 2 h to match (the scheduler raises it to `fallback_interval` anyway, `snapshot/mod.rs:1308-1311`).
- **Does this hide a real bug?** No. The post-attach scan is covered where it is the subject: `tests/snapshots.rs`'s `write_right_after_create_is_snapshotted` (`:846`), `write_before_the_watch_attaches_is_snapshotted` (`:932`) and `release_and_move_during_a_pending_attach` (`:994`), and `tests/integration.rs`'s `ws_run_write_before_the_watch_attaches_is_snapshotted` (`:535`). The fallback timer is covered by `snapshots.rs` and `integration.rs` with their own short intervals. These fixtures test writes, reads and history, and on macOS they already run with inline attaches.
- **Every config in the list above** calls the helper in place of its two lines, beside whatever else it sets (`remote_on(config)` in `remote.rs:745`, the remote listener in `reef-vcs/tests/schema.rs`). A fixture comment that says "whose watcher never snapshots on its own" can stay, since it is now true. Tests that build their own daemon with short timers on purpose (`reads.rs:838`, `presence.rs:855`, `writes.rs:346`, `subscribe.rs:1017`), and `narrowed.rs`, `status_index.rs` and `snapshots.rs`, stay as they are.

The alternative, the two lines only in `files.rs` and `writes.rs`, was turned down (see "Decisions by the user").

### The doc comments

- **`WatcherSection::debounce`** (`config.rs:335-336`) gains: "The full scan after a Linux background attach (`notify_attach_background`) does not wait for it."
- **`WatcherSection::notify_attach_background`** (`config.rs:383-389`) gains: "That scan runs `min_interval` after the workspace's last snapshot, whatever `debounce` is."

These are doc comments in the daemon's config, not the shipped `reef.toml` reference. Check whether `docs/configuration.md` (or wherever `[watcher]` is documented) says the same as `debounce`'s comment, and add the sentence there too if it does.

### No daemon change

No daemon behavior, API, schema, CLI or CI workflow change.

## Deliverables

- **`crates/reef-daemon/tests/history.rs`**: `throttle`'s last wait.
- **`crates/reef-daemon/src/testing.rs`**: `no_automatic_snapshots`.
- **The 19 daemon configs** listed above call it.
- **`crates/reef-daemon/src/config.rs`**: the two doc comments.
- **`crates/reef-daemon/tests/snapshots.rs`**: tests 1 and 2 below.
- **`CHANGELOG.md`**: none. Tests only.
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #205, #206 | [issue-205-206-ci-timing-flakes.md](issue-205-206-ci-timing-flakes.md) | S | R1.32, R2.03, R2.04, R2.16 | none listed |`, after #190's (added with this brief).
- **GitHub**: the PR closes #205 and #206 and names C, the failure with no issue, with its run link. No new issue for C.

## Tests

### `crates/reef-daemon/tests/snapshots.rs`

1. **`the_attach_scan_does_not_wait_for_the_debounce`** (Linux only, `// #206 test 1`). It pins the behavior the fixtures were missing. Use `fixture_with` with `debounce = 1 h`, `fallback_interval` and `watched_fallback_interval` at 2 h, the notify backend, and `notify_attach_background` at its default:
   - Hold attaches, create W, write `a.txt`, and snapshot W explicitly (changed).
   - Write `late.txt`, then drop the hold.
   - Within `min_interval + 3 s`, W has a `Watcher`-kind snapshot operation whose commit has `late.txt`.
2. **`no_automatic_snapshots_takes_none`** (both platforms, `// #206 test 2`). It guards the helper. Use `TestDaemon::start_with(no_automatic_snapshots)` and a synthetic repository:
   - Hold attaches (a no-op once attaches are inline), create W, write and snapshot a file, write `late.txt`, drop the hold, and write another file 1 s later.
   - After 4 s (past `min_interval` and the scheduler's tick), W has no `Watcher`-kind snapshot operation, and its journal's dirty paths name the two late files (`Journals::journal_of`), so the watch is up and saw them.

   The temporary reproduction above is these two tests, minus the second write. On Linux, test 2 fails with `notify_attach_background = false` removed from the helper.

### Existing tests

3. Unchanged and passing on macOS and in the Linux container: every test in those 18 files, `throttle` with its new wait, `narrowed.rs`, `status_index.rs` and `snapshots.rs`.

## Implementation notes

- **Order:** the helper and test 2, then the fixtures, then test 1 and the doc comments, then `throttle`. Run each crate's tests after the fixtures move (`cargo nextest run -p reef-daemon`, `-p reef-vcs`).
- **Checking A's fix.** To see the old wait fail, raise `FOLLOW_GATHER` (`history/mod.rs:48`) to 10 s in a scratch copy: the old `throttle` should fail every run at `:507`, and the new one should pass, waiting about 15 s. Do not commit that change. Report it in the PR.
- **Linux code.** Test 1 is `cfg(target_os = "linux")`, so the Mac never compiles it. Build and lint it in the `reef-linux-dev` container with `CARGO_BUILD_JOBS=1`: at 3, linking the `reef-daemon` test binaries in parallel was OOM-killed while writing this ticket.
- **reef-vcs.** Its tests import `reef_daemon::testing` already. If one of them builds its `Config` some other way, set the four fields there instead.

## Acceptance criteria

- [ ] `throttle` waits for the row at the last write's commit, and asserts `paths_changed == 1` of it.
- [ ] `no_automatic_snapshots` exists, and every config listed above uses it.
- [ ] Test 1 shows a watcher snapshot after a background attach under a 1 h debounce. Test 2 shows none under the helper, and fails on Linux without `notify_attach_background = false`.
- [ ] The two doc comments say that the post-attach scan does not wait for the debounce.
- [ ] In the Linux container, each of the three tests passes 50 times in a row as the `dev` user (fuse-overlayfs), and `abandon_change` and `edit_files_moves_workspaces` pass 50 times in a row as root (kernel overlay), with `--retries 0` (see "Verification").
- [ ] `just ci` passes, and the PR's `overlayfs (ubuntu-24.04, privileged)` job passes on its first run.

## Verification

```sh
# macOS
cargo nextest run -p reef-daemon -p reef-vcs

# Linux container, as dev (fuse-overlayfs), then as root (kernel overlay)
for i in $(seq 1 50); do
  cargo nextest run -p reef-daemon --features privileged-tests --retries 0 \
    -E '(binary(history) & test(=throttle)) | (binary(writes) & test(=abandon_change)) | (binary(files) & test(=edit_files_moves_workspaces))' \
    || { echo "failed on run $i"; break; }
done
```

As root, drop `binary(history)` from the filter, as CI's root step does. Load the machine as the runner was: widen the filter with the other abandon, undo and edit tests (`| (binary(writes) & test(/abandon|undo/)) | (binary(files) & test(/edit_files/))`), which is how the failure was reproduced. Before the fix that loop failed 1 run in 25, so 50 clean runs would happen by chance about 13% of the time: useful, not proof. The deterministic evidence is tests 1 and 2 and the `FOLLOW_GATHER` check. Report all three, and the loop counts, in the PR.

In the container, use `CARGO_BUILD_JOBS=1` and build only the binaries the loop needs (`--test writes --test files --test history`). A loop run took about 75 s, most of it outside the tests.

## Out of scope

- **A failed touch reads as `Current`.** `touch.rs:242-247` marks a commit touched when its computation fails and keeps the previous commit's counts, so `QueryChanges` reports `Current` with stale `paths_changed` and `symbols_changed`. Filed as #214 (Phase 3).
- **Validating `TestDaemon`'s config.** Running `validate` in `start_with` would catch fixtures like these, but other tests may set rule-breaking values on purpose. Not checked here.
- **The post-attach scan's timing.** It is ADR 0028's design and stays as it is.
- **Other flakes** in `docs/` or memory (for example `subscribe::frames_and_keepalives`, which failed in attempt 1 of B's run). They have their own causes.

## Decisions by the user

Made on 2026-11-05.

1. **The helper goes in all 19 configs**, not just in `files.rs` and `writes.rs`.
2. **A failed touch reading as `Current` gets its own issue**, filed as #214 (Phase 3).
