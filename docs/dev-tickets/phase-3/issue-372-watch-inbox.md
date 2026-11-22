# Issue #372: WatchInbox, one live stream of inbox changes

Phase 3 · Size M · Fixes: issue #372 · Builds on: R3.33 (PR #367, ADR 0088), R2.09 (ADR 0036), R2.10 (ADR 0037) · Unblocks: R3.39, R3.40 · Interacts with: #89, #362, #145, #352

## Context

Phase 3 has two open exit criteria: the desktop app notifies on needs-you-now (R3.39), and a phone receives a needs-you-now notification and approves (R3.40). Both need to know when the triage inbox changed. `InboxService.GetInbox` gives the inbox but says nothing when it changes, so today the web page keeps itself live. It subscribes to the operation log and to presence, re-reads `GetInbox` on the operation kinds and presence kinds the last response named, scopes those events to the repositories it last read, and diffs rows by key. That scoping shipped one bug (#367 review F1: an agent's first `reef ask` in a newly registered repository appeared only on reload). R3.39's shell, R3.40's push from the daemon, and any third-party UI would each repeat the logic. R3.40's push runs inside the daemon with no client connected, so the logic must live in the daemon anyway.

This ticket gives the daemon a server-streaming `InboxService.WatchInbox` (READ). It sends the whole inbox, then every change by item key. The daemon owns the trigger set, and the web inbox moves to the stream.

### What was verified while writing this ticket

At main `fe2b1214` (API revision 44, schema 28, last ADR 0090).

- **The trigger table is already the daemon's.** `inbox::Object::triggers()` (`crates/reef-daemon/src/inbox/mod.rs`) lists, per object, the operation kinds and presence kinds that can change its items, and `GetInboxResponse.refresh_operations`/`refresh_presence` are their union. What the web holds on its own is the rest. It defaults the presence kinds before the first read, scopes events to `repoIds` from the last read, compares presence signatures, paces reads (`live/paced.ts`), and keeps a failed repository's last good rows (`good`). The stream moves that logic into the daemon.
- **The buckets do not depend on the viewer; the digest does.** `service/inbox.rs` reads each source through its READ handler with no caller, so every viewer gets the same items. Only the digest's base depends on the caller (their mark, `digest_from`, or 24 hours ago).
- **A read costs about 0.2 ms per open item (debug build), linearly.** The spike `crates/reef-daemon/tests/inbox_cost.rs` (kept as an ignored measurement) times `GetInbox` for one repository, with the median of 11 reads after one warm read:

  | Open proposals (each waiting on a sign-off) | Help requests | Items | Median | Min | Max |
  | --- | --- | --- | --- | --- | --- |
  | 20 | 20 | 40 | 5.3 ms | 4.3 ms | 8.3 ms |
  | 100 | 100 | 200 | 20.7 ms | 20.6 ms | 24.5 ms |
  | 200 | 200 | 400 | 42.6 ms | 42.0 ms | 43.4 ms |
  | 400 | 256 (R3.32's cap) | 656 | 86.8 ms | 80.7 ms | 90.2 ms |

  These numbers are from a debug build on an M2 Max, so they are an upper bound for the installed release build. The cost grows with what is open, as ADR 0088's S-265 says. Re-reading a repository's whole inbox on a trigger therefore costs tens of milliseconds at realistic sizes, so no incremental model is needed (decision 1).
- **Today the cost scales with subscribers.** Each open inbox page re-reads every repository it shows on each trigger burst. The home page shows every repository, so K open pages and R repositories cost K × R section reads per burst.
- **Item records change between reads.** A `Workspace` record carries its lease, disk use and snapshot count, so two reads of an unchanged attention workspace differ. Proposal and comment records are stable between reads when nothing happened. So "changed" can fire for an attention workspace on any recompute of its repository. This is rare (attention is rare) and harmless.
- **The browser holds six HTTP/1.1 connections per origin on loopback**, and `live/share.ts` keeps two long streams per browser (operations and presence) in the leader tab. A stream per inbox page or per tab is not affordable.
- **`buf breaking` (FILE) runs in CI**, so `GetInboxResponse.refresh_operations`/`refresh_presence` stay. Their comments now point at `WatchInbox`.

## Data shape

### The wire (`inbox.proto`, additions only)

```proto
service InboxService {
  // The inbox, then every change to it (#372). Same caller and `repos` as
  // GetInbox; with no `repos`, a repository registered later joins.
  rpc WatchInbox(WatchInboxRequest) returns (stream WatchInboxResponse) {
    option (reef.api.v1.access) = ACCESS_READ;
    option idempotency_level = NO_SIDE_EFFECTS;
  }
}

message WatchInboxRequest {
  // As GetInboxRequest.repos; empty for every registered repository, those
  // registered later included.
  repeated string repos = 1;
}

message WatchInboxResponse {
  // Per repository that changed: its items that entered or changed, the
  // keys that left, and its state.
  repeated InboxSectionChange sections = 1;
  // True from the frame that completes the first inbox onward.
  bool synced = 2;
}

message InboxSectionChange {
  string repo_id = 1;
  // Items new to the section's lists or changed (moved to the other
  // bucket included), each whole, with its bucket.
  repeated InboxItem items = 2;
  // Keys no longer listed: resolved, or past a bucket's 200.
  repeated string left = 3;
  // The section's state, whole, on every change.
  uint32 needs_you_now_total = 4;
  uint32 watching_total = 5;
  LandedDigest landed = 6;
  // Set while the repository fails to read; its items stand as last sent.
  InboxError error = 7;
  // The repository was forgotten: drop the section; its items are in `left`.
  bool removed = 8;
}

enum InboxBucket {
  INBOX_BUCKET_UNSPECIFIED = 0;
  INBOX_BUCKET_NEEDS_YOU_NOW = 1;
  INBOX_BUCKET_WATCHING = 2;
}

message InboxItem {
  // ... fields 1-13 as today ...
  // The bucket it is in: GetInbox's lists and WatchInbox's changes alike,
  // so no client maps reasons to buckets.
  InboxBucket bucket = 14;
}
```

A client folds the stream into a map per repository by key. It needs no operation kind, presence kind, reason-to-bucket table or re-read. The first frames hold every section in full. A frame holds at most 1,000 items and keys and 1 MiB, and a section too large for one continues in the next frame. An empty frame is sent after 15 s without one, as presence does.

### The daemon (`crates/reef-daemon/src/inbox/`)

```rust
// inbox/read.rs: the gathering half of today's service/inbox.rs, moved, so
// GetInbox and the board read one way.
pub(crate) async fn buckets(state, repo, workspaces) -> Result<Buckets, Status>;
pub(crate) async fn digest(state, repo, viewer, from, subjects) -> Result<v1::LandedDigest, Status>;

// inbox/board.rs (pure): what every subscriber shares, replaced whole.
pub(crate) struct Board { repos: BTreeMap<RepoId, Arc<RepoBoard>>, ready: bool }
pub(crate) struct RepoBoard {
    rev: u64,                                   // moves only when a subscriber could see a difference
    items: BTreeMap<String, v1::InboxItem>,     // both buckets, cut at 200 each, by key
    needs_you_now_total: u32, watching_total: u32,
    error: Option<v1::InboxError>,              // items stay the last good read
    subjects: Arc<HashMap<String, String>>,     // the newest landed changes' first lines
    restale_at: Option<Timestamp>,              // when an item's words change by themselves
}
pub(crate) fn diff(sent: Option<&RepoBoard>, now: &RepoBoard) -> (Vec<v1::InboxItem>, Vec<String>);

// inbox/watch.rs: the one task that keeps the board, and the seam R3.40 uses.
pub(crate) struct InboxWatch { /* Mutex<Weak<Shared>> */ }
impl InboxWatch {
    pub(crate) fn watch(&self, state: &Arc<AppState>) -> Watch;   // starts the task with the first watcher
    pub(crate) fn marks_moved(&self);                             // MarkInboxSeen
}
pub(crate) struct Watch { board: watch::Receiver<Arc<Board>>, marks: watch::Receiver<u64>, _shared: Arc<Shared> }

// inbox/session.rs: one WatchInbox stream. Per repository it holds the
// Arc<RepoBoard> it last sent and the digest it last sent.
```

**Organizing structure.** The board is a map of immutable per-repository snapshots behind a `tokio::sync::watch`, with one writer, the task. A subscriber holds an `Arc` of the snapshot it last sent per repository and sends `diff(sent, now)`. Triggers are levels: they mark a repository dirty, and a read replaces its snapshot. No event is ever applied to items. So there is no queue to lag and no resync path to test.

## Decisions

1. **The daemon re-reads a repository's whole section on a trigger and diffs states, once for every subscriber.** One task, started by the first watcher and stopped with the last, keeps a board of every registered repository. A trigger marks a repository dirty. The task re-reads dirty repositories through the same `inbox::read::buckets` that `GetInbox` uses, so there is one classifier. It publishes a new `RepoBoard` only when its items, totals, error or subjects differ. A subscriber diffs the board it last sent against the newest. This is ADR 0088's "computed on read", shared and kept only while someone watches. It is not the maintained inbox ADR 0088 rejected. That inbox applies events to state, and has restart, missed-event and ordering rules. Here a restart is a fresh read, any gap marks everything dirty, and the newest board wins. Per-object re-reads (only help and instructions after a comment) are not needed at the measured cost. They are a follow-up if a fleet shows otherwise. `GetInbox` stays computed on read, and it is the stream's oracle in the tests.
2. **Triggers come from the one table, through the existing channels.** The task holds:
   - **Operations.** One in-process `SubscribeOperations` with no `repos` and `kinds` = `inbox::refresh_operations()`. It follows repositories registered later, other jj processes' operations and log rewrites as any subscription does. Each operation marks its repository dirty. The stream ending (a rewrite, too far behind, an error) resubscribes after a backoff and marks every repository dirty.
   - **Presence.** `PresenceHub::subscribe` for `refresh_presence()` (`AUTOMATIC`, `ATTENTION`). Each change marks its repository dirty, and a lag marks every repository dirty.
   - **The registry.** `OpEvents::changes` bumps on a repository opened or forgotten (and on every head move). The task compares the set of open repositories and re-lists registered ones only when that set moved. A new repository is read and joins every stream that named no `repos`. A forgotten one is dropped, and its streams send `removed`.
   - **Time.** The one headline that changes with no event ("nobody has picked it up", an hour after an instruction's target was last seen) gives its repository a deadline (`restale_at`). The task wakes once at the earliest deadline, so the stream says what `GetInbox` would. This is not a tick.

   Snapshots alone are never a trigger, as ADR 0088 decision 5 requires.
3. **`EXTERNAL` joins the proposal triggers.** Another client's jj operation (`jj describe` at the root, the jj CLI's `jj undo`) can move a proposal. Until now it showed at the next read, and a stream has no next read. It is rare, and it costs one read of the repository. This closes R3.33 review R7-4.
4. **The cost does not depend on how many subscribe, and is zero when idle.** No watcher means no task. A watched, quiet daemon reads nothing (keepalives only). A trigger burst costs one section read of the repositories it touched, however many streams, tabs, devices or (later) push subscriptions watch. Reads are paced by R2.19's rule, moved from `live/paced.ts` into the daemon. Each repository is read on its own, one read of it at a time, so a busy repository thread holds back only that repository. Reads start at most every 250 ms, and each read, from its start, holds back the next for four times its work, laid end to end with every other read's; a read's work is the CPU time it used, not the time it waited (amended after the #387 review, which measured a 4 s stall in one repository holding another's next read back 11 s). So the inbox uses at most a quarter of one core in a trigger storm, whatever the read costs. At the measured debug cost of a 200-proposal repository (43 ms) that is a read every 250 ms, 17% of a core while the storm lasts. A small repository costs under 3%. The latency from a trigger to a frame is at most one spacing plus one read. Workspaces needing attention come from one `ListWorkspaces` per read, scoped to its repository. The board holds every repository's items once: about the size of one `GetInbox` response.
5. **A slow subscriber costs one built frame and its `Arc`s (ADR 0037 decisions 8 and 9, #89).** The session reserves its channel's one slot before it diffs. While the transport is blocked it builds nothing, and when the slot frees it sends the difference between what it sent and the newest board. An item that came and went meanwhile is never sent. It holds at most one superseded snapshot per repository. #89's queueing in h2's send buffer, below the session, is unchanged. Streams count against the daemon's subscription slots (`TOO_MANY_SUBSCRIPTIONS`, shared with `SubscribeOperations` and `SubscribePresence`), and the task's own operation subscription takes one slot.
6. **A failing repository keeps its items.** Its section change carries `error`, its items stand as last sent, and the other repositories carry on. The task retries it after 1 s, doubling to 60 s, while it fails and someone watches. A trigger in it retries too. Recovery is an ordinary diff from the last good read. The web's `good` map moves into the daemon.
7. **The digest follows the mark** (user decision A). Each section's digest is from the caller's mark as it stands. It is sent again when a landing or the caller's mark moves it, and from 24 hours back for a caller with no identity. `MarkInboxSeen` wakes the sessions (`marks_moved`). Each session re-reads its viewer's digest (one SQL aggregate, `db.landed_since`) when its repository's board moved or a mark moved. The newest landings' subjects come from the board, read once per repository, not once per stream. The per-visit base becomes the page's presentation rule. The page banks what it showed when it marks seen, and adds what lands later to it.
8. **`reef inbox watch`** (user decision B). The CLI prints the stream as JSON Lines, one line per repository that changed, as `reef presence watch` does, with the same `--for`, `--follow` and `--count`, and reconnects without reprinting what it printed. *Spelled `reef inbox --watch` in the question; it ships as a subcommand beside `op watch` and `presence watch`, and `reef inbox` alone still prints the inbox. `reef guide`'s leaf commands now count a command that runs beside its subcommands.* An agent that supervises others blocks on it for needs-you-now instead of polling `reef inbox`. It is also the stream's end-to-end test surface.
9. **The web moves to the stream.** The leader tab opens a third stream, `WatchInbox({})`, beside operations and presence, and republishes its frames on `reef-live`. Its `welcome` carries the leader's inbox, encoded as one synced frame. The browser then holds three of its six loopback connections in long streams, where it held two, and the inbox page's `GetInbox` reads are gone. `web/src/live/inbox.ts` (new) folds frames into an `InboxStore`. `views/inbox-model.ts` becomes a projection of it: it filters to the tab's repository (resolved once from the route), merges sections, and keeps `held.ts` and `needs.ts` as they are. It drops `Paced`, the operation and presence subscriptions, the presence kinds and signature, `repoIds`, `good` and `read()`. `inbox/digest.ts` (new, pure) banks the digest per visit (decision 7).
10. **API revision 45, ADR 0091, threat row S-266.** `WatchInbox` is a `NOT_TOOLS` entry (MCP has no stream shape, and `tools/list` is at its budget), with no `remote/rules.rs` row (READ). Its `reef schema` OUTPUTS row comes with `reef inbox watch`. ADR 0091 amends ADR 0088 (decisions 1 and 5, and the maintained-inbox alternative) and ADR 0037 (a third daemon stream; the web's third long connection). No schema change; the schema slot stays unused.

### User decisions

- **A. The digest follows the mark** (decision 7). The alternative pinned each stream's base when it opened. The web would then have opened its inbox stream only while some tab shows an inbox, so that "a visit" is "while this browser shows an inbox", and a mark from another device would not reach an open page. *The user's choice, the recommendation ("Follow the mark live"), answered in chat on 2026-11-21; the inbox request `cmt_d9f66f9b9f04323c` was withdrawn after it.* Accepted cost: a landing undone after the page showed it stays in the shown count until the next visit.
- **B. A CLI for the stream in this ticket** (decision 8). The alternative was a follow-up issue. *The user's choice, the recommendation ("Ship it now"), answered in chat on 2026-11-21; the inbox request `cmt_af7d4bf035d6e250` was withdrawn after it.*

### Alternatives weighed

Four design sketches were made in parallel, on four models, and cross-judged. All four came to the same core: shared immutable per-repository state, re-read and replaced on a trigger, and each subscriber diffing "last sent" against "newest". Their differences, and what this ticket takes:

| Question | Taken | Not taken, and why |
| --- | --- | --- |
| How much to re-read | The whole section of a dirty repository | Per-object (or per-record) re-reads. They save the proposals read on a comment burst, but add five shelves of held state per repository. At 0.2 ms per open item (debug), with each read spaced by four times its work, the whole read stays under a quarter of one core. A follow-up if a fleet measures worse. |
| Where the digest base lives | The caller's mark, live; the page banks what it showed (user decision A) | A base pinned per stream (`digest_from` on the request). It keeps the visit in the daemon. But a browser's stream is per browser, not per visit, so the web would have to open it only while an inbox is on screen. R3.39's notifications need the stream open with no inbox on screen. |
| What a client must remember to notify | Each item carries its bucket; `needs.ts` keeps its set of keys reported once | A cause per item (`LISTED`, `ENTERED`, `MOVED`, `CHANGED`). A client can derive it from its fold, and a reconnect needs the client's memory anyway. |
| How a stream is admitted | `Subscriptions::admit`, `TOO_MANY_SUBSCRIPTIONS` as for every stream | A separate cap for inbox streams |
| Per-subscriber re-reads of `GetInbox` | No | Cost per subscriber × repository, and R3.40 would need a fake stream |
| A broadcast of deltas with lag resync (presence's shape) | No | The resync path is the state diff anyway, and using it alone deletes the queue |

## Scope

- In: `WatchInbox`, the board and its task, `reef-client`'s `watch_inbox`, the SDKs (regenerated), the web inbox on the stream, `InboxItem.bucket` on `GetInbox` too, `EXTERNAL` as a proposal trigger, and `reef inbox watch`.
- Out: notifications themselves (R3.39), push (R3.40), per-object re-reads, an `OFF` proposal's evidence event (#362's remainder) and `watch_state`'s event (#145's remainder), and #89's transport queueing.
- The proposal page and the fleet panel keep their own presence handling. They are not inbox views.

## Tests

- `inbox/board.rs` (unit, pure): `diff` for entered, changed, moved between buckets, left, a cap crossing (item 201 enters as one leaves), a new repository (`sent` none), and the property that folding `diff(a, b)` onto `a` gives `b` over generated boards. `restale_at` for a pending instruction whose target was last seen 59 minutes ago.
- `tests/inbox_watch.rs` (daemon, `no_automatic_snapshots`), asserting on frames only:
  1. The first frames equal `GetInbox` for the same caller, the last `synced`. A daemon with no repository sends one empty synced frame.
  2. **Oracle.** After each step (help asked, then answered; a proposal proposed; a sign-off satisfied through `AUTOMATIC`; a landing; an undo; attention set and cleared), the folded stream equals `GetInbox`.
  3. A repository registered after the stream opened arrives in full on a stream with no `repos`, and its help request follows (#367 F1 at the API). A forgotten repository sends `removed` and its keys in `left`. A stream naming one repository never gets another.
  4. A repository that fails to read sends `error`, keeps its items, and the other repository still updates.
  5. A slow reader: the client stops reading while an item enters and leaves 200 times, then resumes. It gets one frame without that item, and the board is shared (two streams, one read per burst).
  6. Idle: 20 snapshots cause no read (counted by the task). A `describe` is not in this test: it is a proposal trigger for any change, since a describe can change a proposal's words, so it reads.
  7. The digest: a landing sends `landed`. `MarkInboxSeen` from another connection sends `landed` from the new mark. A remote token linked to no identity reads from 24 hours ago.
  8. `EXTERNAL`: a `jj describe` of a proposed change at the root changes its item's record.
- `tests/inbox.rs`: every `GetInbox` item carries the bucket of the list it is in.
- Web unit tests: `InboxStore` folds (full, change, left, removed, error), and an encoded store applied to an empty one round-trips. `holdDigest`: same base replaces, a moved mark banks, two marks in one visit, another device's mark. Share: a follower gets `welcome.inbox`, and a handover replaces the state. `inbox.test.tsx` runs on a fake stream.
- Playwright (`web/e2e/inbox.spec.ts`): the existing inbox steps pass, and a repository registered while the home page is open shows its help request without a reload.
- CLI: `reef inbox watch` prints the first inbox and then a change as JSON Lines.
- `tests/inbox_cost.rs` (ignored): the measurement above, rerun in the PR.

## Done when

- `WatchInbox` streams the inbox and its changes as above, and the web inbox reads only it.
- The tests above pass, and `just preflight`, `just doc`, the web lint, typecheck, unit tests, bundle budget and Playwright pass.
- ADR 0091, `docs/inbox.md`, the threat model (S-266), the README's "Phase 3 so far" and the CHANGELOG say what changed.
