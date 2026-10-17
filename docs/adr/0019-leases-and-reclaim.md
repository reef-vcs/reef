# 0019. Leases and reclaim

- Status: accepted
- Date: 2026-10-17
- Ticket: R1.15

## Context

A git worktree has no owner and no expiry. An agent that crashes leaves its worktree on disk until a human notices, and nothing records what was in it. That is worktree failure 3 in the design document, and the reason the design's "Leased" rule exists: every workspace records an owner, a change, a purpose, and a lease the agent's session must heartbeat. When the lease lapses, the daemon takes a final snapshot, records it in the operation log, and reclaims the directory. A crashed agent therefore leaves a snapshot and a log entry, not a directory. Phase 1's exit criterion says the same: killing an agent mid-edit leaves a snapshot in the store and no directory on disk once the lease expires.

The design also warns that lease tuning is a product decision. Too short, and a slow agent loses its workspace mid-task. Too long, and disk fills with dead workspaces. Because a reclaim always snapshots first, a wrong default can be recovered from: the work is in the operation log either way.

jj's own `workspace forget` is `MutableRepo::remove_workspace` followed by `workspace_store().forget`. It abandons the working-copy commit when that commit is discardable (an empty tree relative to its parent, no description, nothing else referencing it). Otherwise the commit stays as a visible head. Reclaim uses exactly this, so an agent's unfinished change survives as a head and an untouched workspace leaves nothing behind.

## Decision

1. **One lease per workspace, and the lease id is the capability.** `Create` issues the lease in the same transaction as the workspace row. Its length is the request's `lease_ms`, or `lease.default` when that is 0. A request above `lease.max` is `InvalidArgument`, not silently capped. Grace is `lease.grace` at issue time and cannot be set per request. Lease rows are never deleted; R1.17's GC deletes them along with their workspace. A heartbeat must name the workspace's current lease. Nothing checks that the heartbeat comes from the owner, who is informational (RX.01 reviews this).
2. **Heartbeat.** `Workspaces.Heartbeat { repo, workspace_id, lease_id }` sets `expires_at = now + length` and `last_heartbeat_at = now`, then returns the new expiry and `heartbeat_every`: `lease.heartbeat`, at most a third of the length. Its errors:
   - A lease that is not the workspace's current one is `Conflict` ("lease superseded").
   - A workspace that was released or reclaimed is `Conflict` ("reclaimed at <time>"), and so is one being released at that moment.
   - A lease past its grace is `Conflict`. It is about to be reclaimed, and the tick decides that, not the heartbeat.

   A heartbeat never snapshots and never touches jj. The renewal is one SQLite transaction that checks the workspace's state, its current lease, and the grace before it writes.
3. **Grace.** A heartbeat after `expires_at` but within `expires_at + grace` revives the lease. A slow agent that misses one heartbeat, or a machine that pauses, does not lose its directory.
4. **Expiry tick.** Every `lease.tick`, the daemon reads the leases with `expires_at + grace_ms < now` (one indexed query, with "now" computed once) and reclaims each workspace in turn on the expiry task. Reclaims run one at a time per daemon: they are rare, and the final snapshot holds the workspace's scan lock anyway.
5. **One reclaim pipeline.** `Release`, expiry, and (R1.18) restore share `Manager::reclaim`:
   1. `active` → `releasing`, in one statement. For an expiry the statement also requires the lease to still be past its grace, so a heartbeat that landed after the tick's query wins. Then the workspace is untracked, so no watcher snapshot races the final one.
   2. A final snapshot described `reef: final snapshot of workspace <id> (lease expired)` (or `(released)`). If the root is gone, there is nothing to snapshot: the reclaim proceeds with `snapshot_taken = 0` and the reclaim row's detail says `workspace root missing`.
   3. **A failed final snapshot stops the reclaim.** The workspace goes back to `active` with `attention = final-snapshot-failed`. Its directory and lease stay as they are, and the next tick retries. The error is logged at `error` on the first failing tick and every tenth after. `Release` with `force` (R1.22's "reclaim without snapshot", behind a confirmation) is the only way past this.
   4. The disk usage is measured. Then `remove_workspace` and `forget` run on the repository thread (`reef: release workspace <id>`), followed by backend tear-down and removal of `wc/<id>`. Finally, in one transaction: the lease is ended (`expired`, `released`, or `restored`), a `reclaims` row is written, and the state becomes `reclaimed` (or `released`).
6. **Restart grace.** At start, before anything can reclaim, every unended lease gets `expires_at = max(expires_at, now + lease.restart_grace)`. The first tick after a restart therefore reclaims nothing that was alive at shutdown. Shutdown snapshots dirty workspaces (`kind = shutdown`) after the expiry loop has stopped, and leaves leases as they are.
7. **Defaults:**
   - `lease.default = 30m`. It covers a long test run between heartbeats from a live `ws run`, which heartbeats every minute anyway.
   - `grace = 5m`. It covers a daemon or machine pause.
   - `max = 24h`. It is the longest unattended workspace a human can ask for.
   - `restart_grace = 5m` and `tick = 10s`.

   The configuration requires `grace <= default`, `restart_grace <= max`, and `heartbeat < default`, and warns when `heartbeat >= default / 2`.

## Consequences

- A dead agent's directory lives at most `length + grace + tick`: 35 minutes and 10 seconds with the defaults. Its last edits are in the operation log as the final snapshot, and its change stays visible if it was not empty.
- Disk use is bounded by live leases. One exception: a workspace whose final snapshot keeps failing (for example, a permission denied that never clears) keeps its directory indefinitely. `reef ws list` shows it with `attention = final-snapshot-failed`, the error is logged every tenth tick, and a human decides whether to fix it or `release --force` it.
- A human who runs `reef ws create` by hand gets a 30-minute workspace unless they heartbeat or pass a long lease (R1.22's `--lease`, up to `lease.max`). `ws run` (R1.21) heartbeats for its process's lifetime.
- `List` and `Inspect` now carry a lease for released and reclaimed workspaces too: the lease they ended with, with `state = ENDED` and its `end_reason`. Before R1.15 the field was unset once a workspace ended.
- Expiry uses wall-clock milliseconds, because leases must survive restarts. A forward clock jump (sleep, NTP) can expire leases early. The restart grace covers the restart case; the sleep-without-restart case is accepted.
- The reclaim and a concurrent `Release` of the same workspace serialize on the workspace's scan lock and the `active` → `releasing` statement. Exactly one of them proceeds, and the other gets `Conflict`.

## Alternatives considered

- **Process-bound leases**, where the daemon checks that the agent's pid is alive. Rejected: the daemon cannot see processes on another machine or in a container, and pid reuse makes the check unreliable even locally.
- **No grace.** Rejected: one missed heartbeat (a GC pause, a suspended laptop, a slow network) would reclaim a live agent's directory.
- **Reclaiming without a final snapshot when the snapshot fails**, so disk is always bounded. Rejected: that is the one way a reclaim could lose work. Keeping a directory a human can inspect is the better failure.
- **Letting a heartbeat after the grace revive the lease until the tick runs.** Rejected: whether the agent kept its workspace would then depend on tick timing. With a hard grace end, the heartbeat's answer and the tick's agree.
