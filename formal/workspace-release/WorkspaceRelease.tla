--------------------------- MODULE WorkspaceRelease ---------------------------
(***************************************************************************)
(* How one workspace's life ends: a release, a lease expiry, a crash and   *)
(* the next start, racing heartbeats, watcher snapshots and a quiesce.     *)
(*                                                                         *)
(* Every action names the code it stands for; paths are relative to        *)
(* crates/reef-daemon/src. One action is one step of the code with no      *)
(* `.await` inside it, or one SQLite statement or transaction. The         *)
(* coverage record in .claude/skills/workspace-release/SKILL.md lists the  *)
(* functions and the commit the model was last checked against.            *)
(*                                                                         *)
(* Left out: GC and restore, which reclaim through the same pipeline;      *)
(* other workspaces (each reclaim is per workspace); lease supersession;   *)
(* ReleaseSpec's force and skip_final_snapshot; the shared stores; disk    *)
(* accounting; the change lifecycle (lifecycle/).                          *)
(***************************************************************************)
EXTENDS Naturals

CONSTANTS
    MaxFailures,   \* SQLite writes, snapshots or forgets that fail
    MaxCrashes,    \* daemon deaths
    MaxQuiesces    \* quiesce/resume pairs (maintenance/quiesce.rs)

\* "tick" is the lease expiry loop's reclaim (LeaseManager::tick_once,
\* ReclaimReason::Expired); "release" is ReleaseWorkspace
\* (service/workspaces.rs release_workspace, ReclaimReason::Released).
Reclaimers == {"tick", "release"}
Procs == Reclaimers \cup {"watcher"}
Terminal == {"released", "reclaimed"}
EndState(p) == IF p = "tick" THEN "reclaimed" ELSE "released"

\* Steps of Manager::reclaim (workspace/reclaim.rs) run with the scan lock.
Pipeline == {"retry", "check", "begin", "final", "forget", "teardown", "finish"}

VARIABLES
    up,          \* the daemon is running
    dbState,     \* workspaces.state
    lease,       \* the current lease: "live" (renewable), "past" (past its
                 \* grace by the clock the expiry tick reads), "ended"
    loaded,      \* the Manager has a WorkspaceEntry for it
    mem,         \* that entry's record().state; a reclaimer's Arc keeps it
                 \* readable after remove_entry
    handle,      \* that entry's jj handle (entry.jj) is Some
    tracked,     \* the snapshot scheduler tracks it (watcher snapshots)
    lock,        \* who holds entry.scan_lock: "free" or a Proc
    dir,         \* the workspace directory exists
    stateDir,    \* its working-copy state directory exists
    jjws,        \* the jj workspace reef-<id> exists in the view
    pc,          \* where each Proc is
    leftReleasing, \* lock_for_reclaim's left_releasing, per reclaimer
    hb,          \* a heartbeat in flight: "idle", or stamped while the
                 \* lease was "fresh" (live) or "stale" (past)
    client,      \* the lease holder is still running
    paused,      \* a quiesce is in effect
    failLeft, crashLeft, quiesceLeft

entry == <<loaded, mem, handle, tracked>>
disk == <<dir, stateDir, jjws>>
env == <<hb, client, paused>>
budgets == <<failLeft, crashLeft, quiesceLeft>>
vars == <<up, dbState, lease, entry, lock, disk, pc, leftReleasing, env, budgets>>

TypeOK ==
    /\ up \in BOOLEAN /\ loaded \in BOOLEAN /\ handle \in BOOLEAN
    /\ tracked \in BOOLEAN /\ dir \in BOOLEAN /\ stateDir \in BOOLEAN
    /\ jjws \in BOOLEAN /\ client \in BOOLEAN /\ paused \in BOOLEAN
    /\ dbState \in {"active", "releasing"} \cup Terminal
    /\ mem \in {"active", "releasing"} \cup Terminal
    /\ lease \in {"live", "past", "ended"}
    /\ lock \in {"free"} \cup Procs
    /\ pc \in [Procs -> {"idle", "entry", "waitlock", "missing", "mfinish",
                          "wwait", "wsnap"} \cup Pipeline]
    /\ leftReleasing \in [Reclaimers -> BOOLEAN]
    /\ hb \in {"idle", "fresh", "stale"}
    /\ failLeft \in 0..MaxFailures /\ crashLeft \in 0..MaxCrashes
    /\ quiesceLeft \in 0..MaxQuiesces

\* Created and leased (workspace/create.rs), its holder running.
Init ==
    /\ up = TRUE /\ dbState = "active" /\ lease = "live"
    /\ loaded = TRUE /\ mem = "active" /\ handle = TRUE /\ tracked = TRUE
    /\ lock = "free"
    /\ dir = TRUE /\ stateDir = TRUE /\ jjws = TRUE
    /\ pc = [p \in Procs |-> "idle"]
    /\ leftReleasing = [p \in Reclaimers |-> FALSE]
    /\ hb = "idle" /\ client = TRUE /\ paused = FALSE
    /\ failLeft = MaxFailures /\ crashLeft = MaxCrashes
    /\ quiesceLeft = MaxQuiesces

Fail == failLeft > 0 /\ failLeft' = failLeft - 1

Goto(p, l) == pc' = [pc EXCEPT ![p] = l]

\* Return from Manager::reclaim, dropping the scan lock guard.
Exit(p) == lock' = "free" /\ Goto(p, "idle")

\* A heartbeat is stamped and committed within one SQLite mutex wait
\* (HbStart, HbCommit), far shorter than a snapshot, forget or tear-down,
\* so those never start while one is in flight. No outcome depends on
\* this; it keeps counterexamples to orders that can happen.
NoHeartbeatInFlight == hb = "idle"

-----------------------------------------------------------------------------
(* Starting a reclaim *)

\* tick_once: leases_expired_before(now) lists unended leases past their
\* grace, whatever the workspace's state, then reclaims each one.
TickList ==
    /\ up /\ ~paused /\ pc["tick"] = "idle" /\ lease = "past"
    /\ Goto("tick", "entry")
    /\ UNCHANGED <<up, dbState, lease, entry, lock, disk, leftReleasing, env, budgets>>

\* `reef ws release`. Its holder usually runs it, but anyone may.
ReleaseCall ==
    /\ up /\ pc["release"] = "idle"
    /\ Goto("release", "entry")
    /\ UNCHANGED <<up, dbState, lease, entry, lock, disk, leftReleasing, env, budgets>>

\* An unfinished row with no entry and no directory: a start's
\* reclaim_missing could not record its end.
MissingRow == ~loaded /\ ~dir /\ dbState \in {"active", "releasing"}

\* Manager::reclaim's self.entry(id), then lock_for_reclaim's first state
\* read: a `releasing` entry gets try_lock (refused if held), an `active`
\* one waits. No entry: reclaim_unloaded's NotActive or NotLoaded, unless
\* MissingRow.
Entry(p) ==
    /\ pc[p] = "entry" /\ ~MissingRow
    /\ IF ~loaded \/ mem \in Terminal
         THEN Goto(p, "idle") /\ UNCHANGED <<lock, leftReleasing>>
       ELSE IF mem = "releasing"
         THEN IF lock = "free"
                THEN lock' = p /\ leftReleasing' = [leftReleasing EXCEPT ![p] = TRUE]
                     /\ Goto(p, "retry")
                ELSE Goto(p, "idle") /\ UNCHANGED <<lock, leftReleasing>>
       ELSE Goto(p, "waitlock") /\ UNCHANGED <<lock, leftReleasing>>
    /\ UNCHANGED <<up, dbState, lease, entry, disk, env, budgets>>

\* scan_lock.lock().await returns; left_releasing = state is `releasing`.
WaitLock(p) ==
    /\ pc[p] = "waitlock" /\ lock = "free"
    /\ lock' = p
    /\ leftReleasing' = [leftReleasing EXCEPT ![p] = (mem = "releasing")]
    /\ Goto(p, IF mem = "releasing" THEN "retry" ELSE "check")
    /\ UNCHANGED <<up, dbState, lease, entry, disk, env, budgets>>

\* left_releasing: an earlier reclaim left the workspace `releasing`, and
\* this one finishes it, writing nothing first. jj_workspace_exists reads
\* on the repository thread whether that reclaim got past its forget; its
\* `?` returns the error with the scan lock dropped. Then straight to the
\* final snapshot: the workspace is already `releasing` and untracked, so
\* no begin_reclaim, which would refuse it.
Retry(p) ==
    /\ pc[p] = "retry" /\ ~paused
    /\ \/ Fail /\ Exit(p)
       \/ Goto(p, "final") /\ UNCHANGED <<lock, failLeft>>
    /\ UNCHANGED <<up, dbState, lease, entry, disk, leftReleasing, env, crashLeft,
                   quiesceLeft>>

\* The retry found the jj workspace gone (jj_workspace_exists): nothing
\* changes it while this reclaim holds the scan lock.
Forgotten(p) == leftReleasing[p] /\ ~jjws

\* `if record.state != WorkspaceState::Active` -> NotActive.
Check(p) ==
    /\ pc[p] = "check"
    /\ IF mem = "active" THEN Goto(p, "begin") /\ UNCHANGED lock ELSE Exit(p)
    /\ UNCHANGED <<up, dbState, lease, entry, disk, leftReleasing, env, budgets>>

\* Step 1: db.begin_reclaim(id, expired_by), one UPDATE: active ->
\* releasing, and for an expiry only while the lease is unended and past
\* its grace. Then the record and scheduler.untrack, with no await between.
\* Refused: LeaseNotExpired / NotActive. A SQLite error returns through `?`.
BeginOk(p) == dbState = "active" /\ (p = "release" \/ lease = "past")

Begin(p) ==
    /\ pc[p] = "begin"
    /\ \/ Fail /\ Exit(p) /\ UNCHANGED <<dbState, mem, tracked>>
       \/ BeginOk(p)
          /\ dbState' = "releasing" /\ mem' = "releasing" /\ tracked' = FALSE
          /\ Goto(p, "final") /\ UNCHANGED <<lock, failLeft>>
       \/ ~BeginOk(p) /\ Exit(p) /\ UNCHANGED <<dbState, mem, tracked, failLeft>>
    /\ UNCHANGED <<up, lease, loaded, handle, disk, leftReleasing, env, crashLeft,
                   quiesceLeft>>

-----------------------------------------------------------------------------
(* The pipeline, with the scan lock held and the workspace `releasing` *)

\* A step failed, costing `cost` failures, then back_to_active:
\* set_workspace_state(active), the record, track. If that write fails
\* too (one more failure), it logs and returns: the workspace stays
\* `releasing`.
RollBack(cost) ==
    \/ failLeft >= cost /\ failLeft' = failLeft - cost
       /\ dbState' = "active" /\ mem' = "active" /\ tracked' = TRUE
    \/ failLeft > cost /\ failLeft' = failLeft - cost - 1
       /\ UNCHANGED <<dbState, mem, tracked>>

\* Steps 2-3, final_snapshot: none if the root is gone, or if the jj
\* workspace is (Forgotten), with nothing to record into. Otherwise
\* snapshot_locked, on the repository thread (a quiesce pauses it), which
\* fails with NotLoaded when the entry has no jj handle (run_snapshot,
\* workspace/snapshot.rs). A failure (neither caller here forces) rolls
\* back and returns.
Final(p) ==
    /\ pc[p] = "final" /\ ~paused /\ NoHeartbeatInFlight
    /\ \/ (~dir \/ Forgotten(p)) /\ Goto(p, "forget")
          /\ UNCHANGED <<dbState, mem, tracked, lock, failLeft>>
       \/ dir /\ ~Forgotten(p) /\ handle /\ Goto(p, "forget")
          /\ UNCHANGED <<dbState, mem, tracked, lock, failLeft>>
       \/ dir /\ ~Forgotten(p) /\ handle /\ RollBack(1) /\ Exit(p)
       \/ dir /\ ~Forgotten(p) /\ ~handle /\ RollBack(0) /\ Exit(p)
    /\ UNCHANGED <<up, lease, loaded, handle, disk, leftReleasing, env, crashLeft,
                   quiesceLeft>>

\* Step 4, forget: forget_jj_workspace (NoSuchWorkspace is success) on the
\* repository thread, skipped when Forgotten, then entry.jj.take(). A
\* failure rolls back and returns, or, with the root gone, only returns.
Forget(p) ==
    /\ pc[p] = "forget" /\ NoHeartbeatInFlight
    /\ \/ Forgotten(p) /\ handle' = FALSE /\ Goto(p, "teardown")
          /\ UNCHANGED <<jjws, dbState, mem, tracked, lock, failLeft>>
       \/ ~Forgotten(p) /\ ~paused /\ jjws' = FALSE /\ handle' = FALSE
          /\ Goto(p, "teardown")
          /\ UNCHANGED <<dbState, mem, tracked, lock, failLeft>>
       \/ ~Forgotten(p) /\ ~paused /\ dir /\ RollBack(1) /\ Exit(p)
          /\ UNCHANGED <<jjws, handle>>
       \/ ~Forgotten(p) /\ ~paused /\ ~dir /\ Fail /\ Exit(p)
          /\ UNCHANGED <<jjws, handle, dbState, mem, tracked>>
    /\ UNCHANGED <<up, lease, loaded, dir, stateDir, leftReleasing, env, crashLeft,
                   quiesceLeft>>

\* Step 4, tear_down then remove_state_dir. A failed tear-down is only
\* recorded in the reclaim row's detail, and the directory survives it;
\* the state directory is removed either way.
TearDown(p) ==
    /\ pc[p] = "teardown" /\ NoHeartbeatInFlight
    /\ \/ dir' = FALSE /\ UNCHANGED failLeft
       \/ Fail /\ UNCHANGED dir
    /\ stateDir' = FALSE
    /\ Goto(p, "finish")
    /\ UNCHANGED <<up, dbState, lease, entry, lock, jjws, leftReleasing, env,
                   crashLeft, quiesceLeft>>

\* Step 4, db.finish_workspace: one transaction ending the lease and
\* writing the terminal state; then the record and remove_entry. Its `?`
\* returns the error, leaving the workspace `releasing`, untracked.
Finish(p) ==
    /\ pc[p] = "finish"
    /\ \/ Fail /\ UNCHANGED <<dbState, lease, mem, loaded>>
       \/ dbState' = EndState(p) /\ lease' = "ended" /\ mem' = EndState(p)
          /\ loaded' = FALSE /\ UNCHANGED failLeft
    /\ Exit(p)
    /\ UNCHANGED <<up, handle, tracked, disk, leftReleasing, env, crashLeft,
                   quiesceLeft>>

\* reclaim_unloaded: Manager::reclaim with no entry, for a MissingRow whose
\* repository is open (and no restore pending). There is no scan lock to
\* take. An `active` row first goes through Manager::begin_reclaim, as in
\* Begin; a `releasing` one goes on.
MissingBegin(p) ==
    /\ pc[p] = "entry" /\ MissingRow
    /\ IF dbState = "active"
         THEN \/ Fail /\ Goto(p, "idle") /\ UNCHANGED dbState
              \/ BeginOk(p) /\ dbState' = "releasing" /\ Goto(p, "missing")
                 /\ UNCHANGED failLeft
              \/ ~BeginOk(p) /\ Goto(p, "idle") /\ UNCHANGED <<dbState, failLeft>>
       ELSE Goto(p, "missing") /\ UNCHANGED <<dbState, failLeft>>
    /\ UNCHANGED <<up, lease, entry, lock, disk, leftReleasing, env, crashLeft,
                   quiesceLeft>>

\* Then reclaim_missing, as at start but for this reclaim's reason:
\* forget_jj_workspace, whose failure is only logged, and remove_state_dir.
MissingForget(p) ==
    /\ pc[p] = "missing" /\ ~paused
    /\ Goto(p, "mfinish") /\ stateDir' = FALSE
    /\ \/ jjws' = FALSE /\ UNCHANGED failLeft
       \/ Fail /\ UNCHANGED jjws
    /\ UNCHANGED <<up, dbState, lease, entry, lock, dir, leftReleasing, env,
                   crashLeft, quiesceLeft>>

\* Then finish_workspace, whose error is returned. A row another reclaim
\* finished meanwhile is a conflict, which writes nothing.
MissingFinish(p) ==
    /\ pc[p] = "mfinish" /\ Goto(p, "idle")
    /\ IF dbState = "releasing"
         THEN \/ dbState' = EndState(p) /\ lease' = "ended" /\ UNCHANGED failLeft
              \/ Fail /\ UNCHANGED <<dbState, lease>>
       ELSE UNCHANGED <<dbState, lease, failLeft>>
    /\ UNCHANGED <<up, entry, lock, disk, leftReleasing, env, crashLeft, quiesceLeft>>

Reclaim(p) ==
    \/ Entry(p) \/ WaitLock(p) \/ Retry(p) \/ Check(p) \/ Begin(p)
    \/ Final(p) \/ Forget(p) \/ TearDown(p) \/ Finish(p)
    \/ MissingBegin(p) \/ MissingForget(p) \/ MissingFinish(p)

-----------------------------------------------------------------------------
(* Watcher snapshots: Manager::scheduled_snapshot (workspace/snapshot.rs) *)

WatchStart ==
    /\ up /\ loaded /\ tracked /\ pc["watcher"] = "idle"
    /\ Goto("watcher", "wwait")
    /\ UNCHANGED <<up, dbState, lease, entry, lock, disk, leftReleasing, env, budgets>>

\* Under the scan lock: skipped unless the record is `active` and not paused.
WatchLock ==
    /\ pc["watcher"] = "wwait" /\ lock = "free"
    /\ IF mem = "active" /\ ~paused
         THEN lock' = "watcher" /\ Goto("watcher", "wsnap")
         ELSE Goto("watcher", "idle") /\ UNCHANGED lock
    /\ UNCHANGED <<up, dbState, lease, entry, disk, leftReleasing, env, budgets>>

\* The snapshot runs (it fails, logged at debug, without a jj handle).
WatchSnap ==
    /\ pc["watcher"] = "wsnap"
    /\ Exit("watcher")
    /\ UNCHANGED <<up, dbState, lease, entry, disk, leftReleasing, env, budgets>>

-----------------------------------------------------------------------------
(* Leases and time *)

\* LeaseManager::heartbeat takes Timestamp::now() before renew_lease's
\* transaction, which may commit after the expiry tick read the lease as
\* past: two steps. RenewLease refuses during a quiesce
\* (service/workspaces.rs, quiesce.check()).
HbStart ==
    /\ up /\ client /\ ~paused /\ hb = "idle" /\ lease # "ended"
    /\ hb' = IF lease = "live" THEN "fresh" ELSE "stale"
    /\ UNCHANGED <<up, dbState, lease, entry, lock, disk, pc, leftReleasing, client,
                   paused, budgets>>

\* db.renew_lease: renews only an `active` workspace's unended lease not
\* past its grace at the stamped time.
HbCommit ==
    /\ up /\ hb # "idle"
    /\ lease' = IF dbState = "active" /\ lease # "ended" /\ hb = "fresh"
                  THEN "live" ELSE lease
    /\ hb' = "idle"
    /\ UNCHANGED <<up, dbState, entry, lock, disk, pc, leftReleasing, client, paused,
                   budgets>>

\* The clock passes expires_at + grace with no renewal.
Expire ==
    /\ lease = "live" /\ lease' = "past"
    /\ UNCHANGED <<up, dbState, entry, lock, disk, pc, leftReleasing, env, budgets>>

\* The holder exits for good.
ClientStops ==
    /\ client /\ client' = FALSE
    /\ UNCHANGED <<up, dbState, lease, entry, lock, disk, pc, leftReleasing, hb,
                   paused, budgets>>

\* quiesce: pause_all snapshots, pause_ticks (a tick already running goes
\* on), pause the repository threads.
Quiesce ==
    /\ up /\ ~paused /\ quiesceLeft > 0
    /\ paused' = TRUE /\ quiesceLeft' = quiesceLeft - 1
    /\ UNCHANGED <<up, dbState, lease, entry, lock, disk, pc, leftReleasing, hb,
                   client, failLeft, crashLeft>>

\* resume (or auto_resume): extend_after_pause moves every unended lease's
\* expiry to at least now + grace, whatever its workspace's state.
Resume ==
    /\ up /\ paused
    /\ paused' = FALSE
    /\ lease' = IF lease = "past" THEN "live" ELSE lease
    /\ UNCHANGED <<up, dbState, entry, lock, disk, pc, leftReleasing, hb, client,
                   budgets>>

-----------------------------------------------------------------------------
(* Crash and start *)

\* The process dies: every task, lock and entry goes with it.
Crash ==
    /\ up /\ crashLeft > 0 /\ crashLeft' = crashLeft - 1
    /\ up' = FALSE /\ loaded' = FALSE /\ handle' = FALSE /\ tracked' = FALSE
    /\ lock' = "free"
    /\ pc' = [p \in Procs |-> "idle"]
    /\ leftReleasing' = [p \in Reclaimers |-> FALSE]
    /\ hb' = "idle" /\ paused' = FALSE
    /\ UNCHANGED <<dbState, lease, mem, disk, client, failLeft, quiesceLeft>>

\* bring_up (daemon.rs): apply_restart_grace, then Manager::load_active
\* (workspace/manager.rs), which writes nothing for a row it loads. A
\* missing directory goes to reclaim_missing, which logs and goes on past
\* a failed forget or finish_workspace and removes the state directory.
\* Otherwise the entry is loaded in the row's state, tracked if `active`,
\* with a jj handle when reopen finds the state directory. A `releasing`
\* one is left for the next reclaim, which retries it. A directory that is
\* there but cannot be read (#323) leaves the row as it is, unloaded, until
\* the next start; the model leaves that out, as the step writes nothing.
StartLease == IF lease = "past" THEN "live" ELSE lease

Start ==
    /\ ~up /\ up' = TRUE
    /\ IF dbState \notin {"active", "releasing"}
         THEN lease' = StartLease
              /\ UNCHANGED <<dbState, entry, disk, failLeft>>
       ELSE \/ dir /\ lease' = StartLease
               /\ loaded' = TRUE /\ mem' = dbState /\ handle' = stateDir
               /\ tracked' = (dbState = "active")
               /\ UNCHANGED <<dbState, disk, failLeft>>
            \/ ~dir /\ stateDir' = FALSE /\ UNCHANGED entry
               /\ \/ jjws' = FALSE /\ dbState' = "reclaimed" /\ lease' = "ended"
                     /\ UNCHANGED failLeft
                  \/ Fail /\ UNCHANGED jjws
                     /\ dbState' = "reclaimed" /\ lease' = "ended"
                  \/ Fail /\ jjws' = FALSE
                     /\ UNCHANGED dbState /\ lease' = StartLease
                  \/ failLeft > 1 /\ failLeft' = failLeft - 2 /\ UNCHANGED jjws
                     /\ UNCHANGED dbState /\ lease' = StartLease
               /\ UNCHANGED dir
    /\ UNCHANGED <<lock, pc, leftReleasing, env, crashLeft, quiesceLeft>>

-----------------------------------------------------------------------------

Next ==
    \/ TickList \/ ReleaseCall
    \/ \E p \in Reclaimers : Reclaim(p)
    \/ WatchStart \/ WatchLock \/ WatchSnap
    \/ HbStart \/ HbCommit \/ Expire \/ ClientStops
    \/ Quiesce \/ Resume \/ Crash \/ Start

\* What the runtime guarantees:
\* - tokio::sync::Mutex queues waiters in order, so a watcher that keeps
\*   retaking the scan lock cannot starve a waiting reclaim: strong
\*   fairness for reclaims.
\* - The expiry loop's interval fires (TickList); a running reclaim and an
\*   in-flight heartbeat finish; a quiesce's auto_resume timer ends it.
\* - A supervisor (launchd, systemd, or the CLI's autostart) brings a dead
\*   daemon back (Start).
\* - A lease nobody can renew runs out: its workspace is not `active`, or
\*   its holder is gone.
\* Failures, crashes and quiesces are finite (the constants), so they stop.
Fairness ==
    /\ \A p \in Reclaimers : SF_vars(Reclaim(p))
    /\ WF_vars(TickList)
    /\ WF_vars(WatchLock) /\ WF_vars(WatchSnap)
    /\ WF_vars(HbCommit)
    /\ WF_vars(Resume)
    /\ WF_vars(Start)
    /\ WF_vars(Expire /\ (dbState # "active" \/ ~client))

Spec == Init /\ [][Next]_vars /\ Fairness

-----------------------------------------------------------------------------
(* Properties. Each says what a user would lose if it broke. *)

Settled == up /\ \A p \in Reclaimers : pc[p] \notin Pipeline

\* Only the scan lock's holder runs the pipeline, so two reclaims never
\* both snapshot, forget and tear down one workspace.
PipelineHoldsLock ==
    /\ \A p \in Reclaimers : pc[p] \in Pipeline => lock = p
    /\ pc["watcher"] = "wsnap" => lock = "watcher"

\* `ws list` (the database) and the daemon's entry agree whenever no one
\* holds the scan lock.
EntryMatchesRow == (up /\ loaded /\ lock = "free") => mem = dbState

\* A finished workspace's lease has ended, and only a finished one's.
LeaseEndsWithWorkspace == (dbState \in Terminal) <=> (lease = "ended")

\* A workspace Reef calls `active` has its directory: an agent told it
\* holds a workspace can write to it.
ActiveHasDirectory == (Settled /\ dbState = "active") => dir

\* ... and its jj workspace, so its snapshots have a working copy to record.
ActiveHasJjWorkspace == (Settled /\ dbState = "active") => jjws

\* A loaded active workspace is tracked: its writes keep being snapshotted.
ActiveIsTracked == (Settled /\ loaded /\ mem = "active") => tracked

\* No watcher snapshot runs on a workspace being released.
WatcherOnlyOnActive == pc["watcher"] = "wsnap" => dbState = "active"

\* An expiry reclaim gets past begin_reclaim only on a lease still past
\* its grace: a heartbeat that landed after the tick read it wins. This
\* restates begin_reclaim's guard, so it catches an edit to that guard.
ExpiryHonorsRenewal ==
    [][(pc["tick"] = "begin" /\ pc'["tick"] = "final") => lease = "past"]_vars

\* A heartbeat never revives the lease of a workspace being released.
\* Only a revival (past to live) is visible in this model; it restates
\* renew_lease's state check, so it catches an edit to that check.
NoRenewalWhileReleasing ==
    [][(hb # "idle" /\ hb' = "idle" /\ lease = "past" /\ lease' = "live")
         => dbState = "active"]_vars

\* No workspace is left `releasing` (#229): each one ends or comes back.
ReleasingResolves == (dbState = "releasing") ~> (dbState # "releasing")

\* A workspace whose holder is gone is reclaimed and its disk freed.
AbandonedIsReclaimed ==
    (~client /\ dbState = "active") ~> (dbState \in Terminal)
=============================================================================
