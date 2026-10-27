# 0036. Operation log subscriptions

- Status: accepted
- Date: 2026-10-27
- Ticket: R2.09

## Context

The operation log is Reef's event stream ([design](../design.md#api), Subscriptions): every snapshot, workspace create, move and release, describe, abandon, undo, file edit and ref import is one jj operation, and [0030](0030-reads-over-jj-state.md) made each one readable with its kind, workspace, agent and effects. R2.01 declared `SubscribeOperations` in full ([0029](0029-daemon-api-v1.md)), but a client that wanted to know what changed still had to poll: `wait_for_operations` read `ListOperations` twice a second per waiting call ([0035](0035-mcp-server.md) decision 10), and the CLI had no way to follow the log.

The declaration leaves open how the daemon learns of an operation (it had no notification), what an operation another `jj` process writes does, what resuming means on a log that is a graph, how frames are cut, what a slow client costs, and what happens across garbage collection, a forgotten repository and a daemon restart. Phase 2's exit criterion asks for events within 100 ms, and with 500 subscribers at 1,000 operations a minute; the second half is R2.22's broker and load test.

## Decision

1. **The repository thread wakes subscriptions.** Every operation Reef records is committed in a job on the repository's thread ([0012](0012-daemon-process-model-and-threading.md)). The thread compares its loaded operation and generation before each job and after the job and its reload, and when either moved tells `OpEvents`, the daemon's hub, with the repository as loaded. That one place covers every write path, the merge of concurrent operations a reload commits, an outside operation a reload happens to load, and a reopen after a panic. `OpEvents` keeps per repository a version, the last tip (so a subscription reads the op store without a job on a thread that may be busy), and one daemon-wide `watch` that every subscription awaits.
2. **A poll of the op heads finds other `jj` writers.** For each repository at least one subscription follows, one task reads the op heads from the store every 200 ms and wakes subscriptions when they changed. An operation written by `jj` in the registered root or inside a workspace arrives in about 200 ms; the 100 ms target is for operations Reef records.
3. **A position is a frontier**: the op heads up to which every operation has been sent or filtered out, so it holds concurrent heads. Sending operation `o` replaces its parents in the frontier with `o`. What follows a position is every ancestor of the current op heads that is not the frontier's: for `after_operation`, what jj means by "after" on a graph.
4. **`oplog::walk_after` reads a range with a bound.** It walks newest end time first from the heads and the frontier together, marking the frontier's ancestors as it reaches them, takes back anything it kept that a later-ending descendant of the frontier reaches (clocks out of step), stops when only marked operations are left, and orders the rest parents first. jj-lib's `walk_ancestors_range` reads its first chunk without a bound. A frontier member the heads do not reach is reported, so a resume from a rewritten or dropped operation fails instead of replaying the log.
5. **The cursor is opaque and covers every followed repository**: base64url of a versioned encoding of each repository's frontier. It is not bound to the filter. `head_operation_ids`, the one schema addition (API revision 4), gives a subscription to one repository its frontier in clear, so a client that filters can resume with `after_operation`.
6. **Replay runs to the op heads read at the start** (`live: false`); the first live frame follows at once, empty when nothing is new. A subscription without a start sends that frame first. Either way the client knows where it starts before it writes anything it wants to see.
7. **Frames hold at most 1,000 operations and 1 MiB**; one operation is never split. A live frame with operations goes out as soon as it is read, unless the previous one went out under 50 ms before: then everything recorded meanwhile goes in one frame. Replay frames and the rest of a frame a bound cut go at once. An empty frame is a keepalive every 15 s. Nothing is dropped or summarized; summarizing with correct counts is the activity feed's (R2.21).
8. **Backpressure is pull.** Each subscription is one task feeding a channel of one, and it reserves the channel's slot before it reads the log, so it reads only when the transport has taken the previous frame. A slow client costs one built frame, what HTTP/2 flow control lets the transport hold, and its position; when it reads again it catches up from the log. A position more than 10,000 operations behind the op heads, at the start or mid-stream, ends the subscription with `TOO_FAR_BEHIND`. Frame reads have their own permits, apart from the read RPCs', and the daemon serves at most 1,024 subscriptions (`TOO_MANY_SUBSCRIPTIONS`).
9. **Garbage collection moves subscriptions across its rewrite.** GC tells `OpEvents` a rewrite begins, and where the op heads moved from and to (`gc_with`'s callback names the head it rewrote); sessions do not read a repository while its rewrite is under way, nor across one they have not applied, and discard a read a rewrite overlapped. A position at the rewritten head moves to its rewrite and sends nothing, since the rewrite records nothing new. A position behind it (GC's own snapshots land just before the rewrite) first reads up to the rewritten head, whose operations' old copies the undo window keeps, then moves. A position that cannot be read that way (outside the undo window, replaying, two rewrites behind) ends with `OPERATION_LOG_REWRITTEN`, as does a cursor from before the rewrite: `walk_after` stops at operations GC collected behind the frontier and reports the frontier as unreached. A `jj op abandon` or `jj util gc` behind the daemon's back is found by the next read and ends the subscription the same way.
10. **Shutdown ends streams at once with `UNAVAILABLE` and no reason**, the clients' existing signal that the daemon went away, so no stream holds shutdown for the 5-second grace. Operations recorded at shutdown and while the daemon was down reach a client that resumes with its last cursor. A forgotten repository leaves subscriptions to every repository and ends those that named it (`REPOSITORY_NOT_FOUND`); a repository registered later is followed from its head at registration by subscriptions to every repository.
11. **`feed::read` is the seam for R2.22.** What one subscription reads from one repository (the range after its position, filtered, with effects) is one function; R2.22's broker serves the same call from memory and changes nothing else. Wake-ups are O(subscriptions) per operation and each subscription reads the log; that is the cost R2.22 measures at 500.
12. **`reef op watch` prints the stream as JSON Lines and ends by itself**: after `--for` seconds (30 by default) or `--count` operations, unless `--follow`; `--for 0` prints what `--after` replays. A command that never returns by default would hang an agent's tool call ([0025](0025-agent-facing-cli-contract.md)). It reconnects with the cursor when the daemon goes away, prints nothing twice, notes on stderr where to continue, and ends when nobody reads its stdout. JSON Lines departs from one array per list: a stream has no end at which to close one.
13. **`wait_for_operations` reads the stream** with the same inputs, result and description; `tools/list` does not change.

## Consequences

- Every operation Reef records reaches subscribers through one hook; a new write path needs nothing as long as it commits on the repository thread.
- Waiting costs nothing while nothing happens, for the MCP tool and `op watch` alike.
- Each subscription reads the log per operation; ten subscribers are well inside the 100 ms target, and R2.22 decides whether 500 need the broker.
- Resuming by `after_operation` while the log has concurrent heads can replay the other branch's operations; the cursor cannot.
- Operations a subscription reads across a GC carry their ids from before the rewrite; `GetOperation` no longer finds them by those ids.
- How far behind a reader that stops falls depends on what HTTP/2 buffers for it, so `TOO_FAR_BEHIND` mid-stream is tested against a fake source.

## Alternatives considered

- **A broadcast channel of operations per repository.** An unbounded queue per slow subscriber, or dropping, which the design forbids.
- **Pushing from `OpMeta::commit` in reef-lib.** reef-lib has no channel, and merges a reload commits would be missed.
- **A notify watch of the op heads directory.** FSEvents registration costs scale with unrelated directories on macOS ([0026](0026-narrowed-notify-snapshots.md)), and it is a watch per repository against one small `readdir`.
- **jj-lib's `walk_ancestors_range`.** Its first chunk is read without a bound.
- **Cursors as operation ids.** One id holds neither concurrent heads nor several repositories.
- **A resume by time.** Clocks differ across hosts, the trap `ended_after` documents.
- **Ending every stream on any GC.** A live view would lose its stream to routine maintenance.
- **`op log --follow`.** One command printing an array and a stream.
