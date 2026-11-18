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
   could otherwise hide a new violation of the same property.
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
  git log --oneline ce9dc7b9..HEAD -- \
    crates/reef-daemon/src/workspace/reclaim.rs crates/reef-daemon/src/workspace/manager.rs \
    crates/reef-daemon/src/workspace/snapshot.rs crates/reef-daemon/src/snapshot/ \
    crates/reef-daemon/src/lease/mod.rs crates/reef-daemon/src/db/workspaces.rs \
    crates/reef-daemon/src/db/leases.rs crates/reef-daemon/src/model/mod.rs \
    crates/reef-daemon/src/maintenance/quiesce.rs \
    crates/reef-daemon/src/service/workspaces.rs crates/reef-daemon/src/daemon.rs
  ```

- **For pending edits in a Reef workspace** (no `.git` there): run
  `reef diff` from inside the workspace. In the root checkout, use
  `git diff`.

A change to one of these files does not always touch the model. Check whether
it changes one of the functions in the coverage record.

## Coverage record

Last confirmed against the code at **`ce9dc7b9`** (main, "R3.17: degraded
landing to a git remote (#309)"; full id
`ce9dc7b9c06fe059270e807ff0a7d3f1fc677c0f`). Each model action names its code
in a comment. Paths are under `crates/reef-daemon/src/`.

| Code | Model |
|---|---|
| `workspace/reclaim.rs` `Manager::reclaim` (and its `entry.jj.take()`) | `Check`, `Begin`, `Final`, `Forget`, `TearDown`, `Finish` |
| `workspace/reclaim.rs` `lock_for_reclaim`, `finish_going_back` | `Entry`, `WaitLock`, `GoBack` |
| `workspace/reclaim.rs` `final_snapshot`, `forget`, `tear_down`, `back_to_active` | `Final`, `Forget`, `TearDown`, `RollBack` |
| `workspace/manager.rs` `loaded_entry`, `load_active`, `reopen`, `reclaim_missing`, `forget_jj_workspace`, `remove_state_dir` | `Entry`, `Start`, `Forget`, `TearDown` |
| `workspace/snapshot.rs` `scheduled_snapshot`, `run_snapshot` (`NotLoaded` without a jj handle) | `WatchStart`, `WatchLock`, `WatchSnap`, `Final` |
| `snapshot/` the scheduler's `track`, `untrack`, `is_paused`, `pause_all` | `tracked`, `Begin`, `RollBack`, `Start`, `WatchLock` |
| `lease/mod.rs` `heartbeat`, `tick_once`, `apply_restart_grace`, `pause_ticks`, `extend_after_pause` | `HbStart`, `HbCommit`, `TickList`, `Start`, `Quiesce`, `Resume` |
| `db/workspaces.rs` `set_workspace_state`, `begin_reclaim`, `finish_workspace` | `GoBack`, `Begin`, `Finish`, `RollBack`, `Start` |
| `db/leases.rs` `renew_lease`, `leases_expired_before`, `extend_all_active_leases_to` | `HbCommit`, `TickList`, `Resume`, `Start` |
| `model/mod.rs` `WorkspaceState::can_become` | the state values the actions allow |
| `maintenance/quiesce.rs` `quiesce`, `resume` | `Quiesce`, `Resume` |
| `service/workspaces.rs` `release_workspace`, `renew_lease` (refused during a quiesce) | `ReleaseCall`, `HbStart` |
| `daemon.rs` `bring_up` (restart grace before `load_active`, whose error it only logs) | `Start` |

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
   finding.
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

These are the `fail` rows in `expected.tsv`. Each needs at least one failed
SQLite write, a failed tear-down, or a daemon death, and some need more than
one of these. They are rare. They matter because when one happens, only a
restart or a person's command gets the workspace out, as before #229.

**F1. A retried release can roll a torn-down workspace back to `active`.**
Rows: `ActiveHasDirectory`, `ActiveHasJjWorkspace`, `ActiveIsTracked`.

1. A release forgets the jj workspace and tears down the directory, then
   `finish_workspace` fails. The workspace stays `releasing`.
2. The next reclaim of it, a `reef ws release` or an expiry tick, takes the
   scan lock. `finish_going_back` writes `active`.
3. One of these follows, and the reclaim returns with the workspace
   `active`:
   - `begin_reclaim` errors. This needs the write in step 2 to succeed and
     the very next write to fail.
   - `begin_reclaim` refuses an expiry, because a quiesce's `resume`
     extended the lease between the tick's `leases_expired_before` and this
     `begin_reclaim`.
   - The pipeline runs again, skips the final snapshot (no root), and then
     `forget_jj_workspace` fails with an error other than `NoSuchWorkspace`.
     `back_to_active` writes `active`.

Result: the workspace is `active` in the row and in memory, and heartbeats
renew it. It has no directory, no jj workspace and no jj handle, so its
watcher snapshots fail, logged only at debug level. It is also untracked
when `begin_reclaim` errored. A later release or expiry does finish it.

The cause is that a retry publishes `active` without knowing whether an
earlier attempt got past the forget. `fixes/F1.fix` keeps such a workspace
`releasing` instead.

**F2. A daemon death after the forget brings the workspace back `active`
without its jj workspace.** Row: `ActiveHasJjWorkspace` (`crash`).

1. A release forgets the jj workspace.
2. The daemon dies before the tear-down.
3. At start, `load_active` sets the `releasing` row to `active`. The
   directory and the state directory still exist, so the workspace is loaded
   and tracked.

The model assumes `reopen` gets a jj handle. What snapshots of the workspace
then do is untested, so finding out needs a test against the real daemon.

**F3. A failed `finish_workspace` in `reclaim_missing` strands the workspace
until the next restart.** Rows: `ActiveHasDirectory`, `AbandonedIsReclaimed`
(`crash`). This needs a daemon death and a failed write.

1. The daemon dies after the tear-down.
2. At start, `load_active` finds the directory missing and calls
   `reclaim_missing`.
3. There, `finish_workspace` fails, which is only logged.

The row stays `active`, but the workspace is never loaded, so every expiry
tick gets `NotLoaded` for it. The code's comment at
`workspace/manager.rs` `reclaim_missing` accepts this ("the next start
retries").

**F4. A failed write in `load_active` strands a `releasing` workspace until
the next restart.** Row: `ReleasingResolves` (`crash`). This needs a daemon
death and a failed write.

1. The daemon dies during a release.
2. At start, `load_active`'s `set_workspace_state(Active)` fails. Its `?`
   returns, which also skips loading every workspace after this one, and
   `bring_up` only logs the error and serves.
3. The row stays `releasing` and unloaded. Every tick and every release gets
   `NotActive` for it.

**F5. A failed tear-down followed by a failed `finish_workspace` leaves a
workspace that no reclaim can finish.** Row: `AbandonedIsReclaimed`
(`nocrash`).

1. A release forgets the jj workspace (`entry.jj.take()`), the tear-down
   fails and leaves the directory, and `remove_state_dir` runs.
2. `finish_workspace` fails.
3. Every later reclaim takes the final snapshot of the directory. That fails
   with `NotLoaded`, because there is no jj handle, and `back_to_active` sets
   `final-snapshot-failed`. This repeats on every tick.

A restart does not help, because `reopen` cannot find the state directory.
Only a release with `--skip-final-snapshot` finishes it.

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
