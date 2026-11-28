---
name: workspace-release
description: Model-check how a workspace's life ends (release, lease expiry, the reclaim pipeline, heartbeats, quiesce, crash and restart) with the TLA+ model in formal/workspace-release. Use before and after changing workspace/reclaim.rs, the lease code, begin_reclaim/finish_workspace/renew_lease, load_active, or quiesce, or when a workspace gets stuck `releasing` or `active` without its directory.
---

# Workspace release model

`formal/workspace-release/WorkspaceRelease.tla` is a TLA+ model of one
workspace from `active` to `released` or `reclaimed`. TLC checks every order
in which these can happen, within the bounds below:

- a release (`release_workspace`)
- the lease expiry tick (`tick_once`)
- heartbeats
- watcher snapshots
- a quiesce and resume
- failed SQLite writes, snapshots, forgets and tear-downs
- the daemon dying and starting again (`load_active`)

Tests reach a few of those orders. This bug class (#212, #227, #229) lives in
the rest.

## What it checks

Each property is one sentence about what a user would lose if it broke.

| Property | In plain words |
|---|---|
| `PipelineHoldsLock` | Only the holder of the scan lock runs the release pipeline, so two reclaims never both snapshot, forget and tear down one workspace. |
| `EntryMatchesRow` | When no one holds the scan lock, the daemon's entry and the `workspaces` row agree on the state. |
| `LeaseEndsWithWorkspace` | A finished workspace's lease has ended, and only a finished workspace's lease. |
| `ActiveHasDirectory` | A workspace Reef calls `active` has its directory, so an agent told it holds the workspace can write to it. |
| `ActiveHasJjWorkspace` | A workspace Reef calls `active` has its jj workspace, so its snapshots have a working copy. |
| `ActiveIsTracked` | A loaded `active` workspace is tracked, so its writes keep being snapshotted. |
| `WatcherOnlyOnActive` | No watcher snapshot runs on a workspace that is being released. |
| `ExpiryHonorsRenewal` | An expiry reclaim gets past `begin_reclaim` only while the lease is still past its grace. A heartbeat that landed after the tick read the lease wins. |
| `NoRenewalWhileReleasing` | A heartbeat never revives the lease of a workspace that is being released. |
| `ReleasingResolves` | No workspace stays `releasing`: each one ends, or comes back to `active` (#229). |
| `AbandonedIsReclaimed` | A workspace whose holder is gone is reclaimed and its disk freed. |

`TypeOK` checks the model's own types.

`ExpiryHonorsRenewal` and `NoRenewalWhileReleasing` restate the guards in
`begin_reclaim` and `renew_lease`, so they catch an edit to those guards and
nothing more. `NoRenewalWhileReleasing` sees only a lease revived from past to
live, not a renewal of a lease that is still live.

## Run it

From the repository root, or from a Reef workspace of it:

```sh
just formal              # every property, about 1 min
just formal ReleasingResolves ActiveHasDirectory   # only these
just formal --mutants    # also prove each property can still fail, about 1.5 min
```

`just formal` runs `formal/workspace-release/check.sh`. The script needs one
of these:

- a Java runtime, version 11 or later
- a running Docker daemon, used with the pinned `eclipse-temurin:21-jre` image

The first run downloads `tla2tools.jar` 1.7.4 into `~/.cache/reef-formal` and
checks its SHA-256.

The script runs TLC once for each row of `formal/workspace-release/expected.tsv`.
Each row is one property in one scope:

- `nocrash` scope: up to 2 failures and 1 quiesce, and the daemon never dies.
- `crash` scope: the same, plus 1 daemon death and restart.

The script does three things:

1. **On the model:** it compares each outcome with the row's `expected`
   column.
2. **With every fix applied:** for each row expected to `fail`, it runs the
   check again on the model with every edit in `fixes/` applied, and compares
   the outcome with the `fixed` column. A row that fails on a known finding
   could otherwise hide a new violation of the same property. With no open
   findings, `fixes/` is empty or absent, and this step prints `none`.
3. **With `--mutants`:** it applies each mutant in `mutants/` and expects the
   property the mutant names to fail. If that property already fails on the
   model, it applies the mutant on top of the fixes.

The script exits 1 in any of these cases:

- an outcome differs from `expected.tsv`
- TLC errors
- a mutant survives
- a fix or mutant no longer applies (shown as `stale`)

It exits 2 on a property name that is not in `expected.tsv`. Output goes to
`target/formal/workspace-release/`, and the fixed model's output goes to
`fixed/` under it. Each failing run leaves a `<property>.<scope>.trace`, which
lists one step per line: the action that ran, then the variables that
changed. A liveness trace ends in a `loop:` line, and the steps from that
state on repeat forever.

## When to run it

Run it when a change touches any of the covered code below.

- **For a committed change:** run this command from the root checkout. Git
  works there because the repository is colocated.

  ```sh
  git log --oneline a1fb611d..HEAD -- \
    crates/reef-daemon/src/workspace/reclaim.rs crates/reef-daemon/src/workspace/manager.rs \
    crates/reef-daemon/src/workspace/snapshot.rs crates/reef-daemon/src/snapshot/ \
    crates/reef-daemon/src/lease/mod.rs crates/reef-daemon/src/db/workspaces.rs \
    crates/reef-daemon/src/db/leases.rs crates/reef-daemon/src/model/mod.rs \
    crates/reef-daemon/src/maintenance/quiesce.rs \
    crates/reef-daemon/src/service/workspaces.rs crates/reef-daemon/src/daemon.rs
  ```

  The first commit it lists is the fix for F1 to F5 (#311 to #315), which
  the model already describes.

- **For pending edits in a Reef workspace** (no `.git` there): run
  `reef diff` from inside the workspace. In the root checkout, use
  `git diff`.

A change to one of these files does not always touch the model. Check whether
it changes one of the functions in the coverage record.

## Coverage record

Last confirmed against the code at **`a1fb611d`** (main, "Formal check of
the workspace release path (TLA+) (#310)"; full id
`a1fb611d89db8acea9887ce1b615e92141ff54f0`) with the fix for F1 to F5
(#311 to #315) on top. Each model action names its code in a comment. Paths
are under `crates/reef-daemon/src/`.

| Code | Model |
|---|---|
| `workspace/reclaim.rs` `Manager::reclaim` (and its `entry.jj.take()`) | `Check`, `Retry`, `Begin`, `Final`, `Forget`, `TearDown`, `Finish` |
| `workspace/reclaim.rs` `lock_for_reclaim`, `Manager::begin_reclaim` | `Entry`, `WaitLock`, `Begin`, `MissingBegin` |
| `workspace/reclaim.rs` `reclaim_unloaded` | `MissingRow`, `MissingBegin`, `MissingForget`, `MissingFinish` |
| `workspace/reclaim.rs` `final_snapshot`, `forget`, `tear_down`, `back_to_active` | `Final`, `Forget`, `TearDown`, `RollBack` |
| `workspace/manager.rs` `load_active`, `reopen`, `reclaim_missing`, `forget_jj_workspace`, `remove_state_dir` | `Start`, `MissingForget`, `MissingFinish`, `Forget`, `TearDown` |
| `workspace/manager.rs` `jj_workspace_exists` (`reef_lib` `Repo::has_workspace`) | `Retry`, `Forgotten` |
| `workspace/snapshot.rs` `scheduled_snapshot`, `run_snapshot` (`NotLoaded` without a jj handle) | `WatchStart`, `WatchLock`, `WatchSnap`, `Final` |
| `snapshot/` the scheduler's `track`, `untrack`, `is_paused`, `pause_all` | `tracked`, `Begin`, `RollBack`, `Start`, `WatchLock` |
| `lease/mod.rs` `heartbeat`, `tick_once`, `apply_restart_grace`, `pause_ticks`, `extend_after_pause` | `HbStart`, `HbCommit`, `TickList`, `Start`, `Quiesce`, `Resume` |
| `db/workspaces.rs` `set_workspace_state`, `begin_reclaim`, `finish_workspace` | `Begin`, `MissingBegin`, `Finish`, `MissingFinish`, `RollBack`, `Start` |
| `db/leases.rs` `renew_lease`, `leases_expired_before`, `extend_all_active_leases_to` | `HbCommit`, `TickList`, `Resume`, `Start` |
| `model/mod.rs` `WorkspaceState::can_become` | the state values the actions allow |
| `maintenance/quiesce.rs` `quiesce`, `resume` | `Quiesce`, `Resume` |
| `service/workspaces.rs` `release_workspace`, `renew_lease` (refused during a quiesce) | `ReleaseCall`, `HbStart` |
| `daemon.rs` `bring_up` (restart grace before `load_active`, whose error it only logs) | `Start` |

`reclaim_unloaded` also has a case the model leaves out: a repository whose
directory is gone (#428). See "What the model leaves out".

Move the sync commit forward only after you have read each covered function
at the new commit and confirmed that the model still describes it. Do not
move it just because the check passes. Put the new commit and its subject
line in the bold line above.

## Reading a result

Start with the `.trace` file. For each step, open the code its action names
and ask whether that step can happen in that order in the code. Before you
show a counterexample to a person, or when you want to see how the workspace
moves between states, use the `/statechart` skill if it is installed (it
comes with the pstack plugin and is not part of this repository). Give it a
scratch copy of `WorkspaceRelease.tla`, with the failing scope's constants
and the one property as its rule. It reports the shortest path to the broken
rule in plain words. If it is not installed, the `.trace` file is enough.

There are four kinds of mismatch.

**A row that passed now fails.** Decide whether the failure is a bug or a
modeling error.

- **Real bug:** every step can happen in that order in the code. Report it as
  a sequence of steps in the code's words, for example "`back_to_active`
  cannot write `active`, then every tick refuses the workspace as
  `releasing`". Do not weaken the property and do not loosen the model to make
  it pass. Do not change product code unless the user asks.
- **Modeling error:** some step cannot happen that way. The usual causes are
  these:
  - An action that is one atomic step in the model has an `.await` in the
    code.
  - A fairness condition that the runtime does not give. One that the runtime
    does give: `tokio::sync::Mutex` serves waiters in order, which is why
    reclaims get `SF_vars`.
  - A missing guard, such as `~paused` for a step that runs on the repository
    thread.

  Fix the model, name the code that justifies the fix in a comment, and rerun.

**A row that failed now passes.** A finding below may be fixed. Confirm that
the code change fixes it. Then:

1. Change the row to `pass`, set its `fixed` column to `-`, and remove the
   finding and its issue number here.
2. Delete the finding's file in `fixes/`, or the edits in it that the code
   now does.
3. Add a mutant for the property if it has none that runs on the model
   itself.

**A row of the fixed model fails.** A known finding was hiding a new
violation of the same property. Treat it as in the first case, and use the
trace under `fixed/`.

**`stale` or `error`.**
- **`stale`:** a mutant's or fix's old text no longer occurs exactly once in
  the model. The message names the file and the first line of that text.
- **`error`:** TLC could not run, for example on a parse error. Read
  `<property>.<scope>.out`.

**A change that fixes some rows and breaks others.** Report both. Leave
`expected.tsv` alone until the new failures are resolved, then update it for
the whole change at once.

**A change that is a mutant.** If your updated model does what a mutant
does, the change brings back the regression the mutant names. That is the
verdict. Report it, and do not rewrite the mutant to fit.

## Changing the model when the code changes on purpose

1. Edit the actions that the coverage record maps to the changed functions.
   Keep one action per stretch of code with no `.await`, or per SQLite
   statement or transaction. Name the code in the comment above the action.
   Delete actions and variables that the code no longer has.
2. Run `just formal --mutants`. Fix each `stale` mutant or fix by editing its
   `<<<`/`===` text to match the new model, keeping what it means. If a
   property is left with no mutant that runs, write one.
3. Change `expected.tsv` only when the code change itself changes an outcome,
   and say which change does it in the commit message.
4. Move the sync commit forward as described in "Coverage record".

A new property needs a row in `expected.tsv` for each scope, and a mutant in
`formal/workspace-release/mutants/`. A mutant is a small edit to the model
that makes the property fail. It must mirror a plausible code regression,
named in its header. The `# breaks:` line names the property, its kind
(`INVARIANT` or `PROPERTY`) and the scope.

A fix in `fixes/` is a plausible code fix for a finding, written as an edit
to the model. It is not a decision about how to fix the code. Its job is to
show what else fails once the finding is gone.

## Findings on the code at the sync commit

None open: every row of `expected.tsv` passes. The first run found five,
each needing a failed SQLite write, a failed tear-down or a daemon death.
They are fixed, and `crates/reef-daemon/tests/leases.rs` reproduces each one
with a trigger that fails the write:

- **F1 (#311).** A retried release set a workspace that an earlier attempt
  had torn down back to `active`. A reclaim that finds the workspace left
  `releasing` now finishes it without writing `active` first (`Retry`).
- **F2 (#312).** A daemon death after the forget brought the workspace back
  `active` without its jj workspace. The retry now reads whether the jj
  workspace survived and, if not, skips the final snapshot and the forget
  (`Forgotten`).
- **F3 (#313).** A failed `finish_workspace` in a start's `reclaim_missing`
  stranded the workspace until the next restart. A reclaim with no entry
  now ends such a row (`MissingBegin`, `MissingForget`, `MissingFinish`).
- **F4 (#314).** A failed write in `load_active` stranded a `releasing`
  workspace and skipped loading the rest. `load_active` now writes nothing
  for a `releasing` row and loads it, untracked, for the next reclaim.
- **F5 (#315).** A failed tear-down and a failed `finish_workspace` left a
  workspace that every reclaim failed to snapshot. Fixed with F2.

## What the model leaves out

- **Other workspaces.** The model covers one workspace. Two workspaces
  interact only through the repository locks, which the model does not
  include.
- **GC and restore.** Both reclaim through the same pipeline but are not
  modeled as callers. A superseded lease (restore) is not modeled.
- **Shared stores, disk accounting, provenance sessions, presence.** These
  are left out.
- **The change lifecycle** in `lifecycle/` (#293, #294): the proposal state
  machine is a separate state machine.
- **`ReleaseSpec`.** `force` and `skip_final_snapshot` are not modeled. Both
  callers in the model use the default spec.
- **A repository whose directory is gone (#428).** The model's `Start` always
  opens the repository. When it cannot (its directory was deleted or is on an
  unmounted disk), its workspaces load no entry and a start does not touch
  them. A release that asks for a final snapshot is refused with
  `FinalSnapshotRepositoryGone`; one with `--no-snapshot` ends the workspace
  and removes its directory. An expiry reclaim refuses too, so a workspace
  whose repository is gone can be stuck until someone releases it.
- **Time.** Time is reduced to whether a lease is renewable (`live`) or past
  its grace (`past`).
- **Heartbeat timing.** A heartbeat stamped while the lease was renewable may
  commit later, but only before the next snapshot, forget or tear-down step
  starts (`NoHeartbeatInFlight`). No outcome depends on this. It keeps
  counterexamples to orders that can happen.
- **`reopen`.** It is assumed to give a jj handle exactly when the state
  directory exists.
- **Partial operations.** A failed final snapshot or forget changes nothing.
  A failed tear-down leaves the directory in place, and the state directory
  is always removed.
- **Restarts.** Liveness assumes a supervisor brings a dead daemon back.
- **Bounds.** A pass covers at most 2 failures, 1 quiesce and at most 1
  daemon death per behavior. Every path to a failure in the findings above
  fits inside these bounds.
