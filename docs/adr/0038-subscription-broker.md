# 0038. Subscription broker

- Status: accepted
- Date: 2026-10-27
- Ticket: R2.22

## Context

Phase 2's exit criterion asks that a subscriber receive operation log and presence events within 100 ms "with 500 subscribers on one daemon at 1,000 operations per minute". R2.09 ([0036](0036-operation-log-subscriptions.md)) delivered the first half for one to ten subscribers. Every session read the log itself: on each operation, every session woke and ran its own blocking read (the op heads, a walk from its position, two SQLite queries, the effects, a conversion), so 500 subscribers cost 500 reads per operation. R2.09 put that read behind the session's `Source` and left the broker to this ticket, conditionally ("R2.22 decides whether 500 need the broker").

The [design](../design.md) (Scale) says the store needs "a broker between the operation log and the connections, not database notifications", and R4.19 measures delivery "through the broker from R2.22". Presence ([0037](0037-presence-channel.md)) already fans out from memory: its hub stores each change once and broadcasts it.

R2.22's load generator measured the daemon before the broker at the target on an M2 Max. It met the latency gate, with a p99 of 64 ms, but spent 211 CPU-seconds a minute (3.5 cores), almost all of it in per-subscriber reads.

## Decision

1. **One ingest per followed repository into a ring.** When the log of a repository some session follows may have moved, one task reads what follows its ring's head once, converts each operation once (classification, the workspace's owner, effects, the message), and appends it with the position after it. Sessions read the ring.
2. **The broker changes where a session reads, never what it sends.** `BrokerSource` is a second `Source` beside R2.09's `LogSource`. A position the ring holds is served from memory. Any other position (older than the ring, never in it, ring not yet primed) is read from the log as before. The conversion is one function (`feed::deliver`) that both paths call, and a test compares their operations with and without the ring, effects included.
3. **Positions are the frontiers frames already carry, indexed.** Each entry stores the frontier after it, and an index maps every held frontier to its sequence number. The cursor, `head_operation_ids` and resume do not change. A walk that has consumed the same downward-closed set of operations stands at the same frontier, so a session that read past the ring from the log lands on a position the ring holds once the ingest catches up.
4. **Eviction is by bounds only, never held for a reader.** A ring holds at most 4,096 operations and 8 MiB, and all rings together at most 64 MiB, evicting from the largest. A ring nobody follows is dropped after a minute. A stalled session pins nothing and holds back no one: when it reads again it is served from the ring if its position is still there, from the log if not, and fails `TOO_FAR_BEHIND` past 10,000 as before.
5. **Rewrites restart the ring.** After GC the ring restarts at the rewritten head, where caught-up sessions move, so they keep reading from memory. An outside rewrite (a dropped op head, an abandoned head), or more new operations than a ring holds at once, restarts it at the op heads, logged at `warn`. Sessions whose positions are gone fall back to the log and get R2.09's errors. Forgetting a repository drops its ring. Shutdown stops the supervisor and its ingest tasks.
6. **Its own crate, generic over topic, position and item.** `reef-broker` depends on `tokio` alone. The crate graph test forbids jj-lib, tonic, prost, rusqlite and every Reef crate in its graph. The daemon's adapter (`subscribe::broker`) is one producer and reader; the store brings its own over the same `Broker`, `Item`, `Entry`, `Lookup` and `Limits`.
7. **The ingest reads without a permit.** There is one per followed repository, so it is bounded by repositories, and it must not queue behind sessions catching up from the log on their permits.
8. **Wake-ups come from the rings.** A session's view of a repository takes its version from the ring, so an append wakes it and finds what woke it. The supervisor forwards an `OpEvents` change only when a repository came or went or a rewrite moved (what sessions still read from `OpEvents`), so a plain operation wakes each session once. The crate also offers a wake-up per topic for the store, which the daemon does not use.
9. **Presence stays on its hub.** It already fans out without per-subscriber reads, and its contract (a set, then coalesced changes, then a resync from state) has no positions. It is measured beside the log at 500.
10. **The daemon raises its open-file limit and backs off at it.** At start, the binary raises its soft `RLIMIT_NOFILE` to 10,240 on macOS (`OPEN_MAX`) and 65,536 elsewhere, within the hard limit. When `accept` fails for want of descriptors, the listener waits 100 ms before accepting again and warns at most every 10 s. Without the backoff, tonic's accept loop retries at once and logs only at `trace`, spinning a core at the limit. A daemon started from a macOS login shell inherits 256, which cannot hold 500 clients.
11. **The generator and its gate.** `tests/subscription_load.rs`, beside R1.23's load test, opens 500 operation and 500 presence subscribers over 500 connections and records 1,000 operations a minute. Latency is measured from each operation's `ended_at` (presence from `updated_at`), and every delivery is checked against `ListOperations`. The gate: p99 within 100 ms and max within 2 s on both channels, nothing missing, duplicated or reordered, and at least 95% of the target rate. It runs in `load.yml` on both runners, and results are published before and after, in `docs/load-tests.md` and `docs/benchmarks.md`.

## Consequences

- A subscriber's cost per operation is a copy and a filter in memory. The daemon reads the log once per operation per followed repository, plus once for each session that starts or resumes behind its ring.
- Each followed repository holds a ring of up to 8 MiB, and an idle daemon holds none.
- The ring keeps what the ingest read. An operation recorded with no `reef-agent` before its workspace row exists (a create's own operation) keeps an empty agent in the ring, where a later log read would find the owner. R2.09's live reads had the same race per subscriber.
- One slow conversion (a huge rewrite's effects) delays that repository's subscribers, as it delayed each subscriber's own read before. The summary line's `ingest_max_ms` shows it.
- Per-topic wake-ups and frames encoded once for every subscriber are the store's next steps, if R4.19 measures a need.

## Alternatives considered

- **A broadcast channel of operations per repository.** [0036](0036-operation-log-subscriptions.md) rejected it: a lagging reader loses events or needs a queue.
- **A cache of ranges inside `feed::read`.** Every session would still hop to the blocking pool, filter and query SQLite, and concurrent misses would compute the same range many times.
- **Holding the ring back to the slowest reader.** One stalled client would pin memory and, at a cap, hold back everyone.
- **Push fan-out with a queue per subscriber.** Memory per slow reader.
- **A module inside `reef-daemon`.** Lighter, but the boundary the store relies on would be enforced only by review.
- **An out-of-process broker now (NATS, Redis).** A dependency for a local daemon; the row says in-process first.
- **Presence in the broker.** Opposite contracts, and nothing saved.
- **Encoding each frame once for every subscriber.** Needs a custom codec, and filtering makes most frames differ.
- **Extending `load.rs`, or a criterion bench, for the generator.** The first drags in a 17-minute workspace profile; the second cannot run a multi-process measurement with delivery checks.
