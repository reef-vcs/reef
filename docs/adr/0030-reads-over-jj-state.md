# 0030. Reads over jj state

- Status: accepted
- Date: 2026-10-26
- Ticket: R2.02

## Context

R2.01 ([0029](0029-daemon-api-v1.md)) declared the read side of `reef.api.v1` (`GetStatus`, `ListChanges`, `GetChange`, `GetDiff`, `ListOperations`, `GetOperation`) and left it to R2.02, with its meaning but not its mechanics: what a `revision` is over jj's revset language, which reads snapshot first, how a page pins jj state, how an operation Reef did not just write is classified, and what an operation's effects are. Every later Phase 2 ticket builds on those answers: R2.03 resolves revisions and writes the operation attributes, R2.09 streams classified operations with effects, R2.12 replays at an operation.

jj work in the daemon runs one job at a time on each repository's thread ([0012](0012-daemon-process-model-and-threading.md)), which is also where snapshots commit. jj-lib resolves symbols as tags, local bookmarks, commit id prefixes in hex, then change id prefixes in reverse hex only, while Reef's responses spell change ids in hex; and jj-lib's revset expression walkers are private.

## Decision

1. **Reads run off the repository thread.** A read takes its `Arc<ReadonlyRepo>` from one short job on the repository thread (reloading first for change, diff and status reads, as a snapshot does; never for operation reads, which read the operation heads themselves and so never merge or write), then does its work on the blocking pool under `pollster::block_on`, holding one of `max(2, available_parallelism())` permits. A slow diff never delays a snapshot, and reads of one repository run in parallel. A read stops between entries when its RPC is dropped or the daemon shuts down. A read that panics fails with `INTERNAL` and has the repository's thread reopen the store, which the panic may have poisoned (#63). Reads never import git refs.
2. **Revisions are jj's revset language, resolved by jj-lib in jj's symbol order**, plus one resolver extension that runs last: a symbol of hex digits (with jj's `/<n>` offset) is a change id prefix. So hex text is a commit id prefix first and a change id prefix second; reverse hex is only a change id; and a bookmark named like an id prefix is the bookmark. "Exactly one commit": none is `NOT_FOUND`, several (a revset of many, a divergent change id, a conflicted bookmark) are `INVALID_ARGUMENT`, reason `AMBIGUOUS_REVISION`. `CreateWorkspace`, `MoveWorkspace` and `BuildStore` resolve their revisions the same way.
3. **Four aliases, defined per read from the view read, and no user configuration**: `trunk()` is the commit of the first default-start bookmark (R1.31's list: `main`, origin's HEAD, `master`, `trunk`, locally then on `origin`; a conflicted one skipped), else `root()`; `immutable_heads()` is `trunk() | tags()`; `immutable()` is `::(immutable_heads() | root())`; `mutable()` is `~immutable()`. Every daemon answers a revset alike. `trunk()` is not the `default` workspace's working copy, R1.31's last fallback for a create: that commit is the user's mutable work.
4. **`@` is the request's workspace's working-copy commit** in the view read; without a request workspace it is `INVALID_ARGUMENT`, reason `WORKSPACE_REQUIRED`. Whether an expression uses `@` is learned by parsing it as two workspace names no one can type and comparing the parsed expressions' `Debug` forms: only `@` differs between the two. This catches `present(@)`, which swallows the resolution error a cheaper check would rely on.
5. **`at_operation` is an operation id or a unique prefix of lowercase hex** and nothing else (jj's `@-` operation expressions are refused). Any operation in the store qualifies. A read at an operation never snapshots.
6. **Reads that involve `@` snapshot first**: `GetStatus` with a workspace; `GetChange`, `ListChanges` and `GetDiff` when a revision uses `@` (an empty `ListChanges` revset and an empty `to_revision` count, since they include working copies), on the first page, at head, unless `skip_snapshot`. A workspace that is not active is never snapshotted. The snapshot coalesces: a read that finds a scan begun after it arrived returns without scanning, so concurrent reads of one workspace cost at most two scans.
7. **Pages over jj state are positional at a pinned state.** A token pins the operation the first page read (for the operation log, the heads it walked from) and carries a cursor: entries returned so far and the last entry's key, which the next page checks after skipping as many. Every page also stops before its response passes 3 MiB, so a page may hold fewer than `page_size` items. Deep pages cost more than early ones; a page of the operation log examines at most 10,000 operations.
8. **Operation kinds for operations Reef did not just write**: the attribute `reef.kind` (with `reef.snapshot-kind`), which R2.03 starts writing; else the descriptions Reef writes; else `EXTERNAL`. The root operation is not classified. A snapshot's kind comes from the attribute, else the snapshot index, else the description. jj's own texts (`import git head`, `initialize repo`) are `EXTERNAL` until R2.03 tags them.
9. **Effects are the difference between an operation's view and its parents'**: commits visible after and not before (`A`), and the reverse (`R`), walked from the index. A commit in `A` with a recorded predecessor of its own change is `REWRITTEN` from it; other commits in `A` are `REWRITTEN` (a change visible before) or `CREATED`; what is left of `R` is `HIDDEN` (its change still visible) or `ABANDONED`. Bookmarks and working copies are reported when they differ from every parent's; the `git` remote is left out. Lists are capped at 1,000 with their totals. Effects are filled wherever an `Operation` is reported, computed lazily and cached by operation id.

## Consequences

- Reads scale with the blocking pool, not with one thread per repository, and a read cannot delay a snapshot's commit. The cost is that jj-lib stores are shared across threads, as scans already do ([0012](0012-daemon-process-model-and-threading.md)).
- Phase 1's change-first resolution is gone: a bookmark named like an id prefix now resolves to the bookmark, hex text that prefixes both a commit and a change names the commit, and a divergent change or conflicted bookmark is `invalid_argument` (exit 2) where it was `conflict` (exit 5). The CHANGELOG says so.
- The `@` check depends on jj-lib's `Debug` output of parsed expressions only to the extent that it names the workspace; a test parses every form of `@`. A jj-lib bump that changes it fails there, and the fallback is a public expression visitor upstream.
- Effects follow jj's predecessor records; operations from tools that record none get coarser answers (`REWRITTEN` with no previous commit).
- Re-walking for each page makes deep pages of a long history expensive; a cached walk order is the follow-up if that is measured to matter.

## Alternatives considered

**Resolving reads on the repository thread.** Simplest, and how creates resolve; but every read would queue behind snapshots and every other read of the repository.

**Phase 1's change-first order.** Disagrees with every jj tool, and leaves a bookmark named `cafe` unreachable while any id starts with it.

**Rewriting hex symbols to `change_id()` before resolution.** Needs jj-lib's private expression folders.

**Computing a merge operation's parent view with `RepoLoader::merge_operations`.** It writes unpublished operations to the op store; a read must not write.

**Key-sorted pages over materialized logs**, as R2.01's lists do. Holds the whole history in memory per page.
