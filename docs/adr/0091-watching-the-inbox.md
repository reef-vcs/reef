# 0091. Watching the inbox: one board the daemon reads again on a trigger, and a stream of its differences

- Status: accepted
- Date: 2026-11-21
- Ticket: #372 ([brief](../dev-tickets/phase-3/issue-372-watch-inbox.md))
- Amends: [0088](0088-triage-inbox.md) (decision 1: the inbox is also kept, as a board, while someone watches; decision 3: a status first judged `OFF` gets an `AUTOMATIC` entry ended at once; decision 5: the web page reads the stream; the maintained-inbox alternative), [0037](0037-presence-channel.md) (a third long stream in the web UI's leader tab)

## Context

R3.33 made the daemon the one definition of the inbox, computed on read, and left keeping it live to each client. The web page subscribed to the operation log and to presence, read `GetInbox` again on the kinds the response named, and scoped those events to the repositories it last read. That scoping shipped a bug: a repository registered after the page opened was not followed (#367, review finding F1). The desktop shell's notifications (R3.39), the phone's push from the daemon (R3.40) and any other UI would each repeat the logic, and the phone's push runs in the daemon with no client connected.

Measured on the spike (`crates/reef-daemon/tests/inbox_cost.rs`, debug build, M2 Max): one repository's read costs about 0.2 ms per open item, linearly. 40 items took 5.3 ms, 400 took 42.6 ms, and 656 took 86.8 ms. Each open web page read every repository it showed on each trigger burst.

## Decision

1. **A board, kept while someone watches.** `InboxWatch` (`service/inbox/watch.rs`) runs one task from the first watcher to the last. The task holds one immutable snapshot per registered repository (`inbox::board::RepoBoard`): the listed items of both buckets by key, the totals, an error, and the first lines of the repository's newest landed changes. It reads a repository again through the same `buckets` that `GetInbox` uses. It publishes a new snapshot only when a stream could tell it from the last. `GetInbox` stays computed on read.
2. **Triggers are levels.** The task marks a repository dirty on these events, and the next read replaces its snapshot:
   - an operation of a kind `inbox::Object::triggers` lists, from an in-process `SubscribeOperations` over every repository;
   - a change to a presence entry of a kind it lists;
   - a repository opened or forgotten (`OpEvents`);
   - the time an item's words change by themselves (an instruction's target unseen for an hour).

   Any gap marks every repository: the operation stream ending, or presence lagging. So a restart is a fresh read, and no event is ever applied to items. This is not the maintained inbox ADR 0088 rejected, which applies events to state.
3. **`EXTERNAL` is a proposal trigger, and a first `OFF` judgment is announced.** A status first judged `OFF` gets an `AUTOMATIC` entry ended at once (amending ADR 0088 decision 3): its inbox words differ from an unjudged proposal's, and a board that read the proposal before the judgment learns of it no other way. `EXTERNAL` is a trigger because another jj client's operation can move a proposal, and a stream has no next read to show it at.
4. **Paced by R2.19's rule, moved into the daemon, counting work, not waiting.** Each repository is read on its own, one read of it at a time, so a repository whose thread is busy (a long git import, a backup's pause) holds back only its own reads. Reads start in batches, one read of each repository due, at most every 250 ms. Each read, from its start, holds back the next batch for four times its work, laid end to end with every other read's, so one read alone is spaced `max(250 ms, 4 × its work)` as R2.19 spaces it. A read's work is the CPU time it used (`work::worked`, the thread CPU clock): in its polls, and in its jobs on the repository thread and its reads on the blocking pool. Time it waits (for that thread busy with other jobs, or for a lock) is not work: waiting on another thread is not CPU, and counting it held every repository's reads back four times a stall in one (#387 review). So averaged over a trigger storm the inbox costs at most a quarter of one core, however many streams watch and however many repositories it reads, and nothing when idle. A batch after a gap reads every repository at once, in parallel up to the read pool's permits, and is paid for before the next batch starts.
5. **`InboxService.WatchInbox` (READ)** sends, per repository that changed, the items that entered or changed and the keys that left, with the section's state (totals, digest, error, `removed`). Its first frames are the whole inbox, and `synced` marks the frame that completes it. `InboxItem.bucket` carries each item's bucket, in `GetInbox` too. A stream holds the snapshot it last sent per repository. It takes its channel's one slot before it diffs, so a slow client misses intermediate states, never the latest, and costs one built frame and its snapshots (ADR 0037 decisions 8 and 9). The frames one difference is cut into (at most 1,000 items and keys and 1 MiB each) wait in the stream, at most one inbox's worth. Streams take subscription slots (`TOO_MANY_SUBSCRIPTIONS`). The task's own operation subscription takes one more, past the cap, so it is never refused and a stream opens even at `max_subscriptions = 1`; while the task runs, it counts toward the cap as any subscription does. A stream sends each repository as the board first reads it, so a repository whose thread is busy holds back no other's first frames; a stream that names repositories is whole once they are read. An empty frame follows 15 s without one.
6. **A repository that fails to read keeps its items.** Its change carries `error`, and the task retries it after 1 s, doubling to 60 s.
7. **The digest follows the caller's mark** (the user's decision, 2026-11-21). Each stream reads its caller's digest again when its repository's snapshot moved or `MarkInboxSeen` moved a mark, with the subjects from the snapshot. The web page keeps what it showed once it marked a digest seen, and adds later landings to it (`web/src/inbox/digest.ts`). A mark that moved past the page's own was set elsewhere, and the digest starts again from it.
8. **The web page reads the stream.** The leader tab opens `WatchInbox` beside the operation and presence streams and shares it with every tab. The inbox model becomes a projection of it and drops its trigger handling, presence scoping, pacing and `GetInbox` reads.
9. **`reef inbox watch`** prints the stream as JSON Lines, as `reef presence watch` does. `WatchInbox` is not an MCP tool.

## Alternatives

- **Each stream re-reads `GetInbox` and diffs.** The cost grows with streams times repositories per trigger, and R3.40 would need a fake stream.
- **Per-object re-reads** (only help and instructions after a comment). They save the proposals read on a comment burst, but add five shelves of held state per repository. The measured cost and the pacing keep a whole read under a quarter of a core. This is a follow-up if a fleet measures worse.
- **A broadcast of deltas with lag resync**, as presence has. The resync path is the state diff anyway. Using the diff alone deletes the queue and the lag path.
- **A digest base pinned per stream.** It keeps the visit in the daemon. But the web shares one stream per browser, and R3.39's notifications need the stream open with no inbox on screen.
- **A cause per item** (`entered`, `moved`, `changed`). A client derives it from its fold, and a reconnect needs the client's memory anyway.

## Consequences

- R3.39 and R3.40 subscribe: the shell through the web page's store (`live/inbox.ts`, `inbox/needs.ts`), and the daemon's push through `InboxWatch::watch`.
- The browser holds three of its six loopback connections in long streams, where it held two.
- An attention workspace's record (its lease, disk use and snapshot count) differs on every read, so its item is sent as changed whenever its repository is read again.
- A trigger reads its whole repository, whatever change it names. A `describe` of any change reads, as can any operation of another jj client (`EXTERNAL`), an agent running `jj` in a Reef workspace included. The pacing bounds what that costs; reading only what a trigger names is a follow-up.
- The last good items of a repository that fails to read live in the board, so they last while the board does. A board started again (every stream gone, or a daemon restart) that first fails to read a repository sends its `error` with no items. After a reconnect, `reef inbox watch` then prints `left` for what it had printed of that repository, and prints the items again once the repository reads.
- A landing undone after a page banked it stays in the page's count until the next visit.
