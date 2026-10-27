# 0037. Presence channel

- Status: accepted
- Date: 2026-10-27
- Ticket: R2.10

## Context

The [design](../design.md#api) (Subscriptions) gives Reef two channels: the operation log, durable and complete ([0036](0036-operation-log-subscriptions.md)), and presence, "a second, ephemeral channel over the same connection: an agent running tests, an agent editing in a directory, a lease heartbeat, a batch's progress through its checks. Presence is never persisted, so the log stays an audit trail and liveness stays cheap." R2.01 declared `PresenceService` with three RPCs ([0029](0029-daemon-api-v1.md)) and left them unimplemented. Nothing recorded who is doing what right now: a lease renewal records no operation, so no client could see a heartbeat, and a ten-second create was invisible until it returned.

The declaration leaves open who implements which RPC (R2.01's brief gave the report RPCs to R2.11), what an entry's identity is, how long it lives, what bounds memory, what a new subscriber sees, how a chatty publisher or a slow reader is handled, and what the daemon reports itself. Phase 2's exit criterion asks for presence within 100 ms, and with 500 subscribers; the second half is R2.22's.

## Decision

1. **R2.10 implements all three RPCs; R2.11 reports for clients.** What the report RPCs mean (ids, replacement, TTL, caps, coalescing, who may touch what) is the channel's semantics, which bounding memory and testing delivery need settled, and delivery is measured from a report. R2.10 also owns every entry the daemon reports itself; R2.11 owns every entry a client reports on an agent's behalf (`ws run`, the MCP server, the SDKs, `reef presence report` and `clear`) and `reef ws renew`.
2. **A second stream on the same connection, not a multiplexed one.** `SubscribePresence` stays its own server stream. A client wanting both channels opens both on one connection; the daemon sets no limit on streams per connection, and each stream has its own HTTP/2 flow control, so a reader that stops reading its operation stream still receives presence, up to the connection's window. The channels' delivery contracts are opposite (the log drops nothing and resumes from a cursor; presence coalesces and resynchronizes from state), so one stream would need both per frame.
3. **An entry has a daemon-assigned id and four fixed fields**: kind, agent, repository and workspace. A replacement may change anything else, so an entry never moves in or out of a subscriber's filter: a subscriber sees it appear, change and end. A field another kind carries is refused, so every entry reads one way.
4. **TTL is clamped, not refused**: unset or zero is 30 seconds, and values are held to 1 second to 10 minutes, as `page_size` is clamped ([0029](0029-daemon-api-v1.md)); the response's `expires_at` is authoritative. One timer task runs every expiry at its deadline.
5. **The id is the capability**, as a lease id is ([0019](0019-leases-and-reclaim.md), [0027](0027-trust-boundaries-in-phase-1.md)): any client may replace or clear an entry whose id it holds. No client may report kind `LEASE` or touch an entry the daemon reports (`DAEMON_PRESENCE`).
6. **Caps refuse, never evict**: at most 4,096 client entries per daemon and 32 per agent (`TOO_MANY_PRESENCE_ENTRIES`). A client that forgets to pass its id back finds out at once instead of silently pushing out other clients' entries. Daemon entries are not counted; active workspaces and running operations bound them.
7. **A subscriber receives the current set, then changes.** The frames that carry the set end with one marked `synced`, which every later frame is too; an empty set is one empty synced frame at once. There is no cursor: the set is small and in memory, so a reconnecting client subscribes again and takes the new set as the whole truth.
8. **Presence coalesces per entry, latest state wins.** A frame holds an entry at most once; frames with events are at least 50 ms apart, and an isolated change is not delayed. A slow reader may miss intermediate states, never an entry's latest state or its end; an entry that ends before it was sent is not sent. "Coalesce, never drop" holds per entry: what is dropped is superseded states, never the fact that an entry exists or ended.
9. **A broadcast with resync from state.** The hub broadcasts changes on one bounded channel; each session drains it into a map of pending states, one per entry, whether or not its transport took the last frame, so a slow reader costs one built frame and that map. A session that falls behind the broadcast resynchronizes from the hub's current set, ending each entry it holds that is gone with the end reason the hub remembers. [0036](0036-operation-log-subscriptions.md) rejected a broadcast for the log because a lagging reader loses events; for presence, state rather than history is the truth, so a lag is repaired exactly.
10. **Publishers are coalesced, not refused.** The hub stores every report at once (the response returns it) but broadcasts an entry's changes at most once per 50 ms: the first change after a quiet interval at once, later ones folded into one broadcast of the latest state. That caps fan-out per entry whatever a client does and gives a cosmetic signal no error path for its rate.
11. **Lease entries are observed liveness.** Every lease issue and renewal publishes or replaces one `LEASE` entry per workspace carrying the lease, lapsing two heartbeat intervals after the renewal. So it says "the holder is heartbeating" and lapses (`EXPIRED`) long before the lease does, which is what a fleet panel's "what is stuck" needs. Heartbeats are not sent over presence: an ephemeral channel would carry a durable capability, and `RenewLease` already travels on the same connection. After a restart no lease entry exists until each holder's next heartbeat.
12. **Editing entries come from committed snapshot diffs.** After a watcher or explicit snapshot that recorded an operation, a hook lists the paths that differ from the commit it rewrote to its new commit (no copy detection, no content read, on the blocking pool under its own permits) and puts them in front of the workspace's entry. Ignored files never appear, since a snapshot's tree has none; the watcher's raw paths would be dominated by `target/`. A move ends the entry; its paths were on the old change.
13. **Progress after 250 ms.** A guard publishes a create's or store build's progress once the operation has run 250 ms, replaces it as phases and counts move, and ends it when dropped, whatever the outcome. A fast create is not news.
14. **Never persisted.** Entries live in the daemon's memory only: not in SQLite, not in backups, not across a restart. Shutdown ends streams with `UNAVAILABLE` and no reason, as the log's.
15. **Three schema additions (API revision 5)**: `Presence.lease`, so a fleet panel does not call `GetWorkspace` on every heartbeat; `Presence.by_daemon`, so a client knows which entries it may touch and observed edits differ from declared intent; and `PresenceEvent.end_reason` with `PresenceEndReason` (`CLEARED`, `EXPIRED`, `GONE`), since "finished" and "went silent" are a fleet panel's most different signals.
16. **`reef presence watch`, no MCP tool.** The CLI prints the stream as JSON Lines, ends by itself as `op watch` does, and across a restart prints only what is new or changed and an end (with no reason) for each entry that vanished, so a consumer never needs to know a restart happened. An agent does its work without watching presence, and `tools/list` sits at its budget; R2.11's reporting needs no tool.

## Consequences

- Fleet views (R2.20, R3.36) read who is heartbeating, editing what, and how far long operations have got, from one stream beside the log.
- A crashed holder's lease entry lapses within two heartbeats, while its lease lives on until it expires; "no lease entry" is not "not held".
- Editing entries appear a debounce and a scan after a write, and only in workspaces with a watcher or explicit snapshots; an agent's declared intent is R2.11's.
- Daemon entries name the workspace's owner. `ws run`'s workspaces are owned by `user@host:<pid>`, so filtering by an agent name misses them until R2.11 decides whether `ws run` passes the agent as the owner.
- Three streams stalled on one connection would stall every stream on it (hyper's client windows: 2 MiB per stream, 5 MiB per connection); the service comment tells clients to read every stream they open.
- Each subscription reads the hub's broadcast; R2.22 measures 500 and decides whether the broadcast moves into its broker.

## Alternatives considered

- **Multiplexing presence into `SubscribeOperations`.** Opposite delivery contracts in one stream, a cursor that means nothing for half its content, and one flow-control window for both.
- **R2.11 implementing the report RPCs.** The channel's semantics would be split across two tickets, and delivery could not be tested from a report.
- **Persisting presence in SQLite.** The design forbids it; liveness must stay cheap.
- **A cursor for presence.** The state is small and in memory; a fresh set is simpler and exact.
- **Evicting the oldest entry at the cap.** Hides one client's leak in another client's loss.
- **Refusing reports over a rate.** Gives a cosmetic signal an error path in every reporter.
- **Editing entries from the watcher's events.** Ignored files would dominate them.
- **Lease entries that last until the lease expires.** A crashed holder would look alive for up to 24 hours.
- **`ReportPresence` renewing leases.** An ephemeral channel carrying a durable capability.
