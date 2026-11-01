// The smartlog's data (R2.19): one revset's changes as ordered, laid-out
// rows, live from the shared operation stream with one read in flight.
//
// Nothing is read while the stream is not live (before it first is, or after
// a reset until it is again), so an operation recorded after a read arrives
// on the stream and asks for another. A refresh is `ListChanges`
// in repository scope with `skip_snapshot` (a page never snapshots an agent's
// workspace), then `GetStatus` at the operation it echoed, so both describe
// one operation. Operations during a refresh ask for exactly one more, which
// starts no sooner than `max(250 ms, 4 × the last refresh)` after the last
// one started: a burst is one read and a trailing one, and a slow daemon is
// asked less often. Nothing is read on a timer.
//
// At an operation (`at`), the view is frozen: the operation is looked up
// once, both reads are made at it, and the stream is not consulted.
import { Code, ConnectError } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import type { Change, LogEntry } from "../gen/reef/api/v1/changes_pb";
import { ChangeEffectKind, OperationKind, type Operation } from "../gen/reef/api/v1/operations_pb";
import type { Repository } from "../gen/reef/api/v1/repositories_pb";
import type { RepositoryStatus } from "../gen/reef/api/v1/status_pb";
import { layout, type GraphRow, type Layout } from "../graph/layout";
import { graphNode } from "../graph/node";
import { order, type OrderState } from "../graph/order";
import { DEFAULT_LIMIT, MAX_LIMIT } from "../graph/revset";
import { after, type Cancel } from "../live/backoff";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import type { OperationBatch, OperationStore } from "../live/operations";
import { changeState, type Labelled } from "../state/states";

export const OPERATION_NOT_FOUND = "OPERATION_NOT_FOUND";
export const INVALID_REVSET = "INVALID_REVSET";

/** The least time between the starts of two refreshes. */
export const MIN_SPACING_MS = 250;

export interface SmartlogRow {
  entry: LogEntry;
  change: Change;
  graph: GraphRow;
  state: Labelled;
  /** The latest streamed operation that created or rewrote this change. */
  cause: Operation | undefined;
  /** Rewritten or new in a live refresh: shown with the one change transition. */
  changed: boolean;
}

export interface SmartlogSnapshot {
  /** Null until the first read returns. */
  rows: SmartlogRow[] | null;
  /** Lanes still open after the last row, and the widest row. */
  open: Layout["open"];
  width: number;
  /** The operation and the revset the rows were read at and with. */
  atOperation: string;
  revset: string;
  repository: Repository | undefined;
  status: RepositoryStatus | undefined;
  /** At an operation: the operation, once looked up. */
  frozenAt: Operation | undefined;
  /** More entries exist past the limit. */
  more: boolean;
  limit: number;
  error: ReefError | null;
  /** Bumped by every refresh, so pane reads can follow it. */
  version: number;
}

export interface SmartlogOptions {
  repo: string;
  revset: string;
  limit?: number;
  /** An operation id or unique prefix: the view at that point, frozen. */
  at?: string | undefined;
}

/**
 * Kinds whose effects may leave out what the smartlog shows: effects carry no
 * tags, and a tag moves `immutable_heads()` and so a change's state.
 */
const UNREPORTED = new Set([
  OperationKind.IMPORT_REFS,
  OperationKind.EXTERNAL,
  OperationKind.UNDO,
  OperationKind.OTHER,
]);

/** Whether an operation can change what a smartlog of its repository shows. */
function touches(operation: Operation): boolean {
  const effects = operation.effects;
  if (!effects || UNREPORTED.has(operation.kind)) return true;
  return (
    effects.changes.length > 0 ||
    effects.bookmarks.length > 0 ||
    effects.workingCopies.length > 0 ||
    effects.changeCount > 0 ||
    effects.bookmarkCount > 0
  );
}

export class SmartlogModel {
  private snapshot: SmartlogSnapshot;
  private readonly notifier: FrameNotifier;
  private ordered: OrderState | undefined;
  /** Each loaded change's commit ids, to tell a rewrite from a move. */
  private commits = new Map<string, Set<string>>();
  private readonly causes = new Map<string, Operation>();
  private requested = false;
  /** The next read starts a new revset: nothing in it is a change to animate. */
  private fresh = false;
  /** A read was asked for while the stream was not live. */
  private waiting = false;
  private loading = false;
  private again = false;
  /** The next refresh is "Show more": new heads go below, nothing animates. */
  private revealing = false;
  private lastStart = Number.NEGATIVE_INFINITY;
  private lastDuration = 0;
  private pending: Cancel | undefined;
  private stopped = false;
  /** As the route gave it: an id, a prefix or a path. */
  private readonly repo: string;
  /** The full id, once a read has resolved it: what operations and presence carry. */
  private repoId: string | undefined;
  private revset: string;
  private readonly at: string | undefined;

  constructor(
    private readonly connection: Connection,
    private readonly operations: OperationStore,
    frame: FrameScheduler,
    private readonly onSessionInvalid: () => void,
    options: SmartlogOptions,
    private readonly now: () => number = Date.now,
  ) {
    this.notifier = new FrameNotifier(frame);
    this.repo = options.repo;
    this.revset = options.revset;
    this.at = options.at || undefined;
    this.snapshot = {
      rows: null,
      open: [],
      width: 0,
      atOperation: "",
      revset: options.revset,
      repository: undefined,
      status: undefined,
      frozenAt: undefined,
      more: false,
      limit: Math.min(Math.max(options.limit ?? DEFAULT_LIMIT, 1), MAX_LIMIT),
      error: null,
      version: 0,
    };
  }

  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  getSnapshot = (): SmartlogSnapshot => this.snapshot;

  get frozen(): boolean {
    return this.at !== undefined;
  }

  start(): () => void {
    if (this.frozen) {
      void this.readAt();
      return () => {
        this.stopped = true;
      };
    }
    const unsubscribe = this.operations.subscribe((batch) => {
      this.onBatch(batch);
    });
    if (this.operations.live) this.requestFirst();
    return () => {
      this.stopped = true;
      unsubscribe();
      this.pending?.();
    };
  }

  /** "Show more": a larger limit, read now. Rows already shown keep their places. Returns the limit. */
  showMore(by: number = DEFAULT_LIMIT): number {
    const limit = Math.min(this.snapshot.limit + by, MAX_LIMIT);
    if (limit === this.snapshot.limit || this.frozen) return this.snapshot.limit;
    this.update({ limit });
    this.revealing = true;
    this.requestNow();
    return limit;
  }

  /**
   * Another revset: read now, ordered afresh. Until it answers, and if it
   * fails (`INVALID_REVSET`), the last good graph stays.
   */
  setRevset(revset: string) {
    if (revset === this.revset) return;
    this.revset = revset;
    this.ordered = undefined;
    this.commits = new Map();
    this.fresh = true;
    if (this.frozen) void this.readAt();
    else if (this.requested) this.requestNow();
  }

  private requestFirst() {
    if (this.requested) return;
    this.requested = true;
    this.request();
  }

  private onBatch(batch: OperationBatch) {
    // After a reset, the next read waits for the new subscription to be
    // live, as the first did: an operation between the two is not missed.
    let ask = batch.reset || this.waiting;
    for (const operation of batch.operations) {
      // Until a read resolves the route's repository, any repository's
      // operation may be this one's.
      if (this.repoId !== undefined && operation.repoId !== this.repoId) continue;
      for (const effect of operation.effects?.changes ?? []) {
        if (
          effect.kind === ChangeEffectKind.CREATED ||
          effect.kind === ChangeEffectKind.REWRITTEN
        ) {
          this.causes.set(effect.changeId, operation);
        }
      }
      if (touches(operation)) ask = true;
    }
    if (batch.live && !this.requested) {
      // The first read covers this batch too.
      this.requestFirst();
      return;
    }
    if (!ask || !this.requested) return;
    this.waiting = !batch.live;
    if (batch.live) this.request();
  }

  /**
   * A read the user asked for: not spaced, since the spacing paces reads the
   * stream asks for. One in flight still finishes first.
   */
  private requestNow() {
    if (this.stopped) return;
    this.pending?.();
    this.pending = undefined;
    if (this.loading) this.again = true;
    else void this.refresh();
  }

  /** Asks for a refresh: now, after the spacing, or after the one in flight. */
  private request() {
    if (this.stopped) return;
    if (this.loading) {
      this.again = true;
      return;
    }
    if (this.pending) return;
    const spacing = Math.max(MIN_SPACING_MS, 4 * this.lastDuration);
    const wait = this.lastStart + spacing - this.now();
    if (wait <= 0) {
      void this.refresh();
      return;
    }
    this.pending = after(wait, () => {
      this.pending = undefined;
      void this.refresh();
    });
  }

  private async refresh() {
    if (this.stopped) return;
    if (!this.operations.live) {
      // A reset since this was asked for: read once the stream is live again.
      this.waiting = true;
      return;
    }
    this.loading = true;
    const revset = this.revset;
    const revealing = this.revealing;
    this.revealing = false;
    const started = this.now();
    this.lastStart = started;
    try {
      const read = await this.read("", revset);
      if (this.isStopped()) return;
      if (revset !== this.revset) {
        // The revset changed during the read: read the new one.
        this.again = true;
        return;
      }
      this.apply(read, {
        live: !revealing && !this.fresh && this.snapshot.rows !== null,
        frozenAt: undefined,
      });
    } catch (thrown) {
      this.fail(thrown);
    } finally {
      this.loading = false;
      this.lastDuration = this.now() - started;
      if (this.again) {
        this.again = false;
        this.request();
      }
    }
  }

  private async readAt() {
    const revset = this.revset;
    try {
      const response = await this.connection.clients.operations.getOperation({
        repo: this.repo,
        operationId: this.at ?? "",
      });
      const operation = response.operation;
      if (this.isStopped()) return;
      if (!operation) throw new ConnectError("The daemon returned no operation.", Code.NotFound);
      const read = await this.read(operation.id, revset);
      if (this.isStopped() || revset !== this.revset) return;
      this.apply(read, { live: false, frozenAt: operation });
    } catch (thrown) {
      this.fail(thrown);
    }
  }

  /**
   * `ListChanges` up to the limit (one page, unless the 3 MiB byte limit cut
   * it short; later pages are pinned to the first's operation), then
   * `GetStatus` at that operation. A GC between the two calls restarts the
   * read once.
   */
  private async read(atOperation: string, revset: string, retried = false): Promise<Read> {
    const { changes, status } = this.connection.clients;
    const limit = this.snapshot.limit;
    const entries: LogEntry[] = [];
    let pageToken = "";
    let at = atOperation;
    do {
      const page = await changes.listChanges({
        repo: this.repo,
        revset,
        atOperation: at,
        skipSnapshot: true,
        pageSize: limit - entries.length,
        pageToken,
      });
      entries.push(...page.entries);
      at = page.atOperation;
      pageToken = page.nextPageToken;
    } while (pageToken && entries.length < limit);
    const more = pageToken !== "";
    try {
      const response = await status.getStatus({ repo: this.repo, atOperation: at });
      return {
        revset,
        entries,
        more,
        atOperation: at,
        repository: response.repository,
        status: response.repositoryStatus,
      };
    } catch (thrown) {
      const error = reefError(thrown);
      if (!retried && !atOperation && error.reason === OPERATION_NOT_FOUND) {
        return this.read(atOperation, revset, true);
      }
      throw thrown;
    }
  }

  private apply(
    read: Read,
    { live, frozenAt }: { live: boolean; frozenAt: Operation | undefined },
  ) {
    const ordered = order(read.entries.map(graphNode), this.ordered, live);
    this.ordered = ordered.state;
    const graph = layout(ordered.nodes);
    const byCommit = new Map(read.entries.map((entry) => [entry.change?.commitId ?? "", entry]));
    const previous = this.commits;
    const commits = new Map<string, Set<string>>();
    const rows: SmartlogRow[] = [];
    for (const row of graph.rows) {
      const entry = byCommit.get(row.node.id);
      const change = entry?.change;
      if (!entry || !change) continue;
      let ids = commits.get(change.changeId);
      if (!ids) commits.set(change.changeId, (ids = new Set()));
      ids.add(change.commitId);
      const before = previous.get(change.changeId);
      rows.push({
        entry,
        change,
        graph: row,
        state: changeState(change),
        cause: frozenAt ? undefined : this.causes.get(change.changeId),
        changed: live && (before ? !before.has(change.commitId) : true),
      });
    }
    this.commits = commits;
    this.fresh = false;
    this.repoId = read.repository?.id || this.repoId;
    this.update({
      rows,
      open: graph.open,
      width: graph.width,
      atOperation: read.atOperation,
      revset: read.revset,
      repository: read.repository,
      status: read.status,
      frozenAt,
      more: read.more,
      error: null,
      version: this.snapshot.version + 1,
    });
  }

  /** A call, so TypeScript does not narrow `stopped` across an await. */
  private isStopped(): boolean {
    return this.stopped;
  }

  private fail(thrown: unknown) {
    if (this.isStopped()) return;
    const error = reefError(thrown);
    if (isSessionInvalid(error)) {
      this.onSessionInvalid();
      return;
    }
    // The last good graph stays; the error shows above it.
    this.update({ error });
  }

  private update(change: Partial<SmartlogSnapshot>) {
    this.snapshot = { ...this.snapshot, ...change };
    this.notifier.notify();
  }
}

interface Read {
  revset: string;
  entries: LogEntry[];
  more: boolean;
  atOperation: string;
  repository: Repository | undefined;
  status: RepositoryStatus | undefined;
}
