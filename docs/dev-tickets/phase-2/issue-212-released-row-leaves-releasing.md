# Issue #212 A released row leaves "releasing"

Phase 2 · Size S (no row; see "Size") · Fixes: issue #212 · Depends on: R2.20 (the workspaces page and its model, `web/src/views/fleet-model.ts`), R2.18 (ADR 0047, the shared streams), R2.09 (`SubscribeOperations`) · Unblocks: none listed · Related: #145 (workspace changes that publish no event)

## Context

The `web e2e` job fails in about 4 of the last 10 `ci.yml` runs, on `main` too, at `web/e2e/smoke.spec.ts:226`. That line waits up to 10 s for wsB's row to read `released` after `reef ws release`, and the row reads `releasing` the whole time. It fails in Chromium and Firefox alike. Its `title` reads "runner@… released a workspace just now", so the page did receive the release's operation.

The cause is in the product, not the test. A release records its last operation, `RELEASE_WORKSPACE` (the jj workspace forgotten), before it writes the workspace's terminal state. The workspaces page refreshes only when an operation arrives. So when the refresh that operation triggers lands between the two, `ListWorkspaces` still lists the workspace as `RELEASING`, and the page shows `releasing`. The terminal state then arrives with no operation, so nothing reads it until some later operation in the repository. On a quiet repository, which the test's is at that point, that never happens. The test's expectation is right: a released workspace's row should say `released`.

This ticket makes the workspaces page re-read any workspace it lists as `RELEASING`, paced as its refreshes are, until the workspace is no longer releasing. Then it refreshes once, and the row takes its ended form ("released", "reclaimed (lease expired)"), or comes back as active if the release rolled back. The change is in the web app only, with a comment-only correction to the proto and an amendment to ADR 0047. There is no API revision, no schema change and no daemon change.

### What was verified while writing this ticket

All of this was checked at 93b4c50 (main), with the installed `reef` 0.4.1 (93b4c50). Rust paths are under `crates/`, web paths under `web/`.

- **The release's order** (`reef-daemon/src/workspace/reclaim.rs`, `Manager::reclaim`, `:88-196`):
  1. `db.begin_reclaim` (`:108`; `db/workspaces.rs:323`) writes `RELEASING`, and the in-memory record follows (`:120`). No operation.
  2. The final snapshot (`:134`, `final_snapshot` at `:201`) records a `SNAPSHOT` operation if the tree changed.
  3. `forget` (`:145`, `forget_jj_workspace` at `workspace/manager.rs:850-873`) records `reef: release workspace <id>`, kind `RELEASE_WORKSPACE`, on the repository thread. The thread wakes subscribers after each job that moved the head (`subscribe/mod.rs:4-6`), so the operation goes out on `SubscribeOperations` now.
  4. `unmount_stores` (`:153`), the backend's `tear_down` on the blocking pool (`:154`), and `remove_state_dir` (`:155`). These record no operation.
  5. `db.finish_workspace` (`:169`) writes `RELEASED` (or `RECLAIMED`), the lease's end and the `reclaims` row. Then `presence.end_workspace` (`:170`) ends the entries naming the workspace, if there are any. No operation.

  A release that fails at step 2 or 3 goes back to `ACTIVE` through `back_to_active` (`:327`), maybe with `attention = final-snapshot-failed`. That records no operation either. Lease-expiry, GC and restore reclaims run the same pipeline.
- **Reproduced against the daemon.** A temporary integration test in `reef-daemon/tests` (deleted after the run) subscribed to the repository's operations, created a Small-preset workspace, and released it from a spawned task. When the `RELEASE_WORKSPACE` operation for it arrived, the test called `ListWorkspaces { states: [ACTIVE, RELEASING] }` at once. In 8 of 20 rounds the workspace was listed as `RELEASING` (debug build, macOS, APFS backend). The operation arrived 80 to 155 ms after the release started, and `ReleaseWorkspace` returned 0 to 17 ms after that. So the window is steps 4 and 5, and it is real on every backend.
- **Reproduced in the page's model.** A temporary Vitest case in `web/src/views/fleet-model.test.ts` (removed) did the same over `FakeFleetDaemon`. It listed `w2` as `RELEASING` for the refresh the `RELEASE_WORKSPACE` operation asked for, then stopped listing it and set its final state to `RELEASED`, with no further operation. After 60 s of fake time, `w2`'s row still had `state = RELEASING` and no `ended`, and `ListWorkspaces` had been called twice: the first load and the operation's refresh.
- **The page's model** (`web/src/views/fleet-model.ts`):
  - The header comment (`:1-23`) says that any operation in the repository asks for a refresh, and that "Nothing is read on a timer."
  - `LISTED = [ACTIVE, RELEASING]` (`:50`), so a releasing workspace is a listed row.
  - `onBatch` (`:259`) asks `Paced` for a refresh on every operation in the repository. `Paced` runs one read at a time, at least `max(250 ms, 4 × the last read)` apart (`live/paced.ts`).
  - `apply` (`:443`) turns a row that is no longer listed into an ended row, after one `GetWorkspace` for its final state (`readFinal`, `:488`, `skipDiskUsage: true`). A row still listed as `RELEASING` is left as it is.
  - `Fleet.tsx:245-247` renders ` releasing` after the id when `workspace.state === RELEASING && !ended`. That is the CI log's "0ff7f2f9 releasing…".
- **What R2.20 assumed.** R2.20's brief says (`R2.20-workspaces-and-fleet-panel.md:121`) "The daemon's reclaim records an operation, which refreshes the row into its ended form". Issue #145 says the same ("the reclaim records an operation"). The model test `keeps a workspace that ended in place, read once, until hidden` (`fleet-model.test.ts:150`) and the view test `keeps a released workspace's row in place` (`fleet.test.tsx:241`) build that order: the fake daemon already lists the workspace as gone when the operation arrives. Neither builds the order the daemon actually uses.
- **The CI logs agree.** In run 41941744392 (`web e2e`, chromium), the row's `title` is "… released a workspace just now" for all 23 polls, and its text keeps ` releasing` and the lease cell "expires in 30 min". Only the disk cell changes, from `0 B` to `not measurable`: the disk scheduler's own `GetWorkspace` answered without a figure, as it does for any workspace that is not `ACTIVE` (`workspace/manager.rs:517-521`). The model takes only disk figures from those answers. The same shape shows in runs 41781829011, 41921833577 and both attempts of 41977921144.
- **No other client is affected.** `reef ws release` returns after step 5. The SDKs and the CLI do not infer state from operations. The selection pane (`WorkspaceDetail.tsx`) shows the row's workspace, so it is fixed along with the row. The smartlog and the activity feed show jj state and operations, and those are consistent at the operation.
- **`GetWorkspace` with `skip_disk_usage` is cheap.** It reads only the database (`Manager::inspect`, `workspace/manager.rs:492-545`; no repository-thread job when `snapshot_first` is false). `ListWorkspaces` with `changed_file_counts` runs one repository read-thread job per refresh (`service/workspaces.rs:226-265`).
- **A proto comment change does not bump the API revision.** `API_DIGEST` hashes the v1 descriptors without comments (`reef-api/src/lib.rs:89`). Nothing checked in copies the two comments this ticket corrects: `web/src/gen` and the SDKs' generated code are built, not committed.
- **Why Linux CI hits it more often (hypothesis, not measured).** In the e2e harness on Linux, workspaces are fuse-overlayfs mounts (#128's CI notes), so step 4 includes an unmount and is slower than an APFS directory removal, which widens the window. This does not change the fix.

## Decisions

### Size

S, with no `dev-phases.md` row. The work is one method and one `Paced` in `fleet-model.ts`, the header comment, two proto comments, an ADR 0047 amendment, six Vitest cases and one view test.

### Where the fix goes: the page

The daemon cannot cheaply make "the operation arrived" imply "the terminal state is visible":

- Writing the terminal state before `forget` breaks the rollback. `RELEASED` is terminal (`model/mod.rs:111-116`), and a failed forget goes back to `ACTIVE`.
- Writing it right after `forget`, before the tear-down, only narrows the window. The operation is published inside the repository job, before `forget` returns, and the 200 ms op-heads poll can also find it. The tear-down's details also go into the `reclaims` row, so that row would need a second write. A crash between the two writes would leave a `RELEASED` row whose directory is still there.
- Holding the operation's publication back until step 5 cuts across the broker (R2.22), and the op-heads poll would still find it.

The real gap is on the page's side. The page treats operations as the only signal of change, but three of a release's state changes record none: to `RELEASING`, to the terminal state, and back to `ACTIVE`. While the daemon lists a workspace as `RELEASING`, that state is transient by definition, and the page cannot know when it ends without reading. So the page reads. The alternatives are open question 1.

### The re-read

In `FleetModel`:

- **A second `Paced`**, `settling`, built like `paced` (`new Paced(() => this.settle(), now)`, the default gate). It runs only for the live page. A frozen page (`?at=`) never starts it.
- **When it is asked for.** At the end of a live `apply`, if any listed workspace has `state === RELEASING`, call `this.settling.request()`. A request while one is pending or in flight costs nothing more (`Paced`'s rules).
- **`settle()`**:
  - It returns `undefined` (no run recorded) when the model is stopped, when the operation stream is not live, or when no listed workspace is releasing. A reconnect's live batch asks for a refresh anyway (`waiting`), and that refresh's `apply` asks again.
  - Otherwise it calls `GetWorkspace { repo, workspace, skipDiskUsage: true }` for each listed `RELEASING` workspace, in parallel.
  - If any answer's state is not `RELEASING`, or a read fails, it calls `this.paced.request()`. That full refresh then takes the row out of the list and into its ended form through `apply`'s existing `gone`/`readFinal` path, or lists it as `ACTIVE` again after a rollback, and it reports a failure the way every refresh does. A session-invalid failure calls `onSessionInvalid`, as `readFinal` does.
  - If every answer is still `RELEASING`, it calls `this.settling.request()`, which `Paced` defers until the run ends and spaces at `max(250 ms, 4 × this run)`.
- **`stop`** also stops `settling`.
- **No new state on the rows.** The re-read only decides when to refresh. The row's text comes from the refresh, so the selection pane, the "Hide n ended" count, and the `ended.operation` (the `RELEASE_WORKSPACE` cause already kept in `causes`) work as they do now. Passing the settle's answer to `apply` as the final, to skip `readFinal`'s second read, is allowed but not required.
- **Cost.** A typical release is in this state for tens of milliseconds, so one or two reads. A long one (a final snapshot on a large tree, or tearing down a full copy) costs one database-only read per releasing row every 250 ms or more, spaced further when the reads are slow. A refresh with `changed_file_counts` would cost a repository job instead, which is why the re-read is not a full refresh.

### The contract, written down

- **`fleet-model.ts`'s header** replaces "Nothing is read on a timer." with: "Nothing is read on a timer, but for one case. A release records its last operation before the workspace reaches its final state, which records none, and a release that fails goes back to active without one. So while a workspace is listed as releasing, it is read again (`GetWorkspace`, without disk), paced the same way, until it is not, and then the page refreshes (#212)."
- **`operations.proto`**, `OPERATION_KIND_RELEASE_WORKSPACE` (`:138`): "A workspace's jj workspace was forgotten as it was released or reclaimed. It is recorded before the workspace's directory is removed and before its state becomes RELEASED or RECLAIMED, which records no operation: read the workspace (GetWorkspace) for its final state."
- **`workspaces.proto`**, `WORKSPACE_STATE_RELEASING` (`:108`): "Being released or reclaimed. The change to the final state, or back to ACTIVE when a release fails, records no operation." These are comments only, so the digest and `API_REVISION` (14) do not change.
- **ADR 0047**, a new section `## Amendment, <date> (#212): a releasing workspace is read until it is not`. It says that decision 10's "never fetches on a timer" has one exception, the re-read above, and why: the daemon publishes no event for the end of a release. It points to #145 for the general event.

### No API change

No proto field, RPC or message changes. The daemon, its database schema (6), the CLI, the SDKs and the e2e test do not change. The e2e test's assertion stays as it is, with its 10 s default timeout.

## Deliverables

- **`web/src/views/fleet-model.ts`**: `settling`, `settle()`, the request at the end of a live `apply`, `stop`, and the header comment.
- **`crates/reef-api/proto/reef/api/v1/operations.proto`** and **`workspaces.proto`**: the two comments.
- **`docs/adr/0047-web-ui-transport-and-local-listener.md`**: the amendment.
- **Tests** 1 to 8 below.
- **`CHANGELOG.md`**, Unreleased, **Fixed**: "The web UI's workspaces page shows a released workspace as `released`. When the release's last operation reached the page before the workspace's final state, the row kept reading `releasing` until the next operation in the repository. The page now reads a releasing workspace again until it ends, and also shows a release that failed as active again ([#212](https://github.com/reef-vcs/reef/issues/212))."
- **`docs/dev-tickets/phase-2/README.md`**: the row `| #212 | [issue-212-released-row-leaves-releasing.md](issue-212-released-row-leaves-releasing.md) | S | R2.09, R2.18, R2.20 | none listed |`, after #190's.
- **Issue #145**: a comment saying that the release case is fixed by a re-read (#212), and that attention and watch-state changes still publish no event.
- **Unchanged**: `API_REVISION`, the schema, the daemon, `web/e2e/smoke.spec.ts`, `docs/design.md` (its "never fetches on a timer" is the goal; ADR 0047 records the exception).

## Tests

### `web/src/views/fleet-model.test.ts`

Each case is marked `// #212 test N`. Use the file's `setup`, `flush`, `push` and `FakeFleetDaemon`. `getWorkspace` answers from `workspaces` first and then from `finals` (`test/fleet-daemon.ts:149-157`), so a workspace listed as `RELEASING` reads back as `RELEASING`. Count the re-reads as `getCalls.filter((c) => c.workspace === id && c.skipDiskUsage)`; the disk scheduler's reads do not set `skipDiskUsage`.

1. **`settles a release whose operation arrives while it is releasing`**. This is the issue's order:
   - After the first load, list `w2` as `RELEASING` and push `RELEASE_WORKSPACE` for `w2` (agent `ann`). After `flush(MIN_SPACING_MS)`, `w2`'s row has state `RELEASING` and no `ended`.
   - Advance one more spacing: `w2` was read again with `skipDiskUsage` at least once.
   - Then stop listing `w2` and set `finals` to `RELEASED`, with no operation. After two more spacings, `w2`'s row has `ended.word === "released"` and `ended.operation?.id === "o-rel"`, it is still in place (`["w2", "w1"]`), and `listCalls` grew by exactly one.
   - Then `flush(10_000)`: neither `getCalls` for `w2` nor `listCalls` grows. The re-reads stop.
2. **`shows a release that rolled back as active`**. `w2` is listed as `RELEASING` after an operation, then listed as `ACTIVE` with `attention: FINAL_SNAPSHOT_FAILED`, with no operation. After two spacings the row has state `ACTIVE`, the attention, and no `ended`, and the re-reads stop (`flush(10_000)` adds none).
3. **`reads a reclaim's final word too`**. As test 1, but the final is `RECLAIMED`: `ended.word === "reclaimed (lease expired)"`.
4. **`spaces the re-reads and stops them with the model`**. `w2` stays `RELEASING`. Over 2,000 ms of fake time (in 250 ms steps), its `skipDiskUsage` reads number at least 2 and at most 9. After `t.stop()`, another 2,000 ms adds none. Hold one read with `holdGet` across a step, and check that no second read starts while it is held (one in flight).
5. **`does not re-read while the stream is not live, or with nothing releasing`**:
   - With only active rows, `flush(60_000)` adds no `skipDiskUsage` read. This guards decision 10 for the common case.
   - With `w2` releasing, `t.operations.reset()` (the stream starts over, so `live` is false; `live/operations.ts:70-76`) stops the re-reads. The next live frame's refresh starts them again.
6. **`a frozen page never re-reads`**. `setup({ at: "abcd" })` over a daemon that lists `w2` as `RELEASING` at the operation. After `flush(10_000)`, `getCalls` holds only what `selectAt` asked for (none here).

### `web/src/views/fleet.test.tsx`

7. **`turns a releasing row into released without another operation`** (`// #212 test 7`), beside `keeps a released workspace's row in place`, with fake timers only (the file's rule: a faked `Date` stretches the spacing). The row first reads ` releasing` after its id. With no further operation, it then reads `released` and no longer contains ` releasing`, and the row count is unchanged.

### Existing tests

8. Unchanged and passing: every other case in `fleet-model.test.ts`, including `keeps a workspace that ended in place, read once, until hidden`, whose "read once" still holds because a workspace that is not listed as `RELEASING` is never re-read. Also `fleet.test.tsx`, `paced.test.ts`, `crates/reef-api`'s `api_digest_matches_revision` at revision 14, `reef-web`'s bundle budget (`crates/reef-web/tests/assets.rs`), and the e2e smoke test, unmodified.

## Implementation notes

- **Order:** the model change with tests 1-6; the view test 7; the proto comments (`cargo nextest run -p reef-api` shows the digest unchanged); ADR, CHANGELOG, README; then the e2e runs in "Verification".
- **Fake timers.** In `fleet-model.test.ts`, `setup` fakes `setTimeout`, `clearTimeout` and `Date`. A read held across `advanceTimersByTime` stretches the spacing (`lastDuration`). Test 4's held read should account for that, or use the view tests' timers-only setup.
- **`Paced` inside its own run.** Calling `settling.request()` from inside `settle()` sets `again`, so the next run starts after this one ends, spaced. Do not call `start` directly.
- **A row listed as releasing from the first load** (the page opened mid-release) settles the same way, because the first load is a live `apply`.
- **Bundle.** The change is a few hundred bytes. The JS budget is 500 KiB (R2.20), and the current bundle is reported by `just web-check`.
- **Do not lengthen the e2e timeout or add a retry.** The test was right. If it still fails after this change, the cause is somewhere else and needs its own diagnosis.

## Acceptance criteria

- [ ] A workspace whose `RELEASE_WORKSPACE` operation reaches the page while the daemon still lists it as `RELEASING` shows `released` (or `reclaimed …`) once the release ends, with no further operation (tests 1, 3, 7).
- [ ] A release that fails and goes back to `ACTIVE` stops reading `releasing` (test 2).
- [ ] The page reads nothing more while no listed workspace is releasing, while the stream is not live, at an operation, or once stopped. Re-reads are one at a time and spaced at least 250 ms apart (tests 4, 5, 6).
- [ ] The two proto comments, the model's header and ADR 0047 say that a release's last operation comes before its final state. `API_REVISION` stays 14 and the digest test passes.
- [ ] `web/e2e/smoke.spec.ts` is unchanged and passes repeatedly (see "Verification"). `just web-check` and `just ci` pass. CHANGELOG, the tickets README and #145 are updated.

## Verification

```sh
cd web && npx vitest run src/views/fleet-model.test.ts src/views/fleet.test.tsx src/live/paced.test.ts
just web-check
cargo nextest run -p reef-api
just web-e2e --project=chromium --repeat-each=20
just web-e2e --project=webkit --repeat-each=10
```

Firefox cannot launch under Playwright on macOS 27 ("Could not find profile folder"; #128), so locally use Chromium and WebKit, and let Linux CI cover Firefox. The local e2e race is narrower than on Linux CI, so a local pass alone is weak evidence. The deterministic evidence is a mutation check: with the request at the end of `apply` removed, tests 1, 2, 3 and 7 fail and the rest pass. Report it in the PR.

On the PR, the `web e2e` job must pass on its first run. Then re-run it (`gh run rerun <run id> --job <job id>`) until it has passed five times. Before the fix it failed in about 4 of 10 runs, so five passes in a row would happen by chance about 8% of the time. Report the run ids in the PR.

## Out of scope

- **An event for a workspace's state, attention or watch state** (#145): the daemon-side, timer-free fix for this whole class. It is an API decision (ADR 0036 or 0037), Phase 3.
- **Reordering the daemon's release** so that the terminal state is written before the operation is published. "Where the fix goes" explains why that does not close the gap.
- **The `ACTIVE` → `RELEASING` change, which also records no operation.** The page sees it on the release's first operation (the final snapshot or the forget), which is enough to show ` releasing` while it lasts.
- **Other pages.** No other view lists workspaces by state.

## Decisions by the user

Decided 2026-11-05: the page re-reads releasing workspaces (question 1), and active rows with attention are not re-read (question 2).

1. **Where the fix goes.** Decided: the page re-reads releasing workspaces, paced (as written). It is S, web-only, and covers the rollback too. Its cost is one stated exception to ADR 0047's "never fetches on a timer", bounded to a state the daemon says is transient. Alternatives:
   - **A daemon `PROGRESS` presence entry for every release**, published as soon as the release begins and ended after its final write (or rollback). The page would refresh when such an entry for a releasing row ends. That needs no timer and no proto change. But it waives ADR 0037 decision 13's 250 ms delay for releases (a quick release would otherwise publish nothing), puts an entry on every presence subscriber for every release, and shows the owner on the agent cards while it lasts. Size M, with an ADR 0037 amendment.
   - **A workspace event in the API** (#145's general fix): an API revision, the SDKs, Phase 3. This ticket would wait for it, and the e2e job would keep flaking until then.
2. **Should the re-read also run for an `ACTIVE` row with `attention` set?** Decided: no. #145 owns attention, and an active row is not transient, so a re-read would be a real timer.
