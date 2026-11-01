// A repository's window of the operation log (R2.21): every operation from
// the newest down to a bottom, each once, joined to the shared stream. The
// activity feed, its counts and the scrubber are all derived from it.
//
// The join. The window subscribes to the stream, waits until it is live,
// and only then lists the log from the head. Every operation recorded after
// the stream's position at that moment arrives in a later batch (including
// one the store holds and has not delivered yet); every operation at or
// before it was recorded before the list was read, so the list has it.
// Nothing falls between the two, and an operation in both, or a frame
// replayed after a reconnect or a handover, collapses on its id. The
// records are kept in `newerFirst` order, by content, not arrival, so a
// reload shows what a live session showed.
//
// Starting over. A reset batch (the stream skipped operations) discards
// nothing visible at once but reloads from the head down to the old
// bottom's time once the stream is live again. Garbage collection moves the
// stream across its rewrite with no reset, and every later operation names
// the rewritten head as its parent, which the window does not hold: an
// applied operation naming a parent the window does not hold starts over
// the same way, since a fresh list returns the kept operations under new ids
// and merging the two copies would count them twice. A writer that started
// from an older head also names a parent the window may not hold, below its
// bottom: so the parent is read first (a one-operation list at it, which a
// pre-GC id refuses), and only a rewritten parent, or one that ended within
// the window's period, starts over. A page token
// that hits the rewrite starts over too.
//
// A failed read is retried with the streams' backoff, never once per batch.
//
// At most `WINDOW_CAP` operations: older pages are offered until then, and
// live operations past it drop the oldest from the bottom.
import { Code } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import {
  isSessionInvalid,
  OPERATION_LOG_REWRITTEN,
  reefError,
  type ReefError,
} from "../api/errors";
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import type { Repository } from "../gen/reef/api/v1/repositories_pb";
import { timestampMs } from "@bufbuild/protobuf/wkt";
import { after, Backoff, type Cancel } from "../live/backoff";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import type { OperationBatch, OperationStore } from "../live/operations";
import { isRoot, newerFirst, recordOf, type OpRecord } from "./record";

/** Operations the window holds at most: R2.09's resume horizon and the store's `PENDING_LIMIT`. */
export const WINDOW_CAP = 10_000;
/** Operations per list page. */
export const PAGE_SIZE = 1000;

export const REPOSITORY_NOT_FOUND = "REPOSITORY_NOT_FOUND";
export const OPERATION_NOT_FOUND = "OPERATION_NOT_FOUND";

/** Starts over in a row before the window gives up and shows the error. */
const MAX_START_OVERS = 3;

export type Note = { kind: "reset" | "gc"; at: number } | null;

/** Where a point (`?at=`) sits relative to the window. */
export type Point =
  | { state: "loading" }
  /** Held by the window; `operation` once `GetOperation` answered, for the banner. */
  | { state: "inside"; id: string; operation: Operation | undefined }
  /** Older than the window can hold: one page ending at it, not joined to the window. */
  | { state: "detached"; operation: Operation; records: OpRecord[] }
  | { state: "error"; error: ReefError };

export interface WindowSnapshot {
  repository: Repository | undefined;
  /** Every operation held, newest first; null until the first list answers. */
  records: OpRecord[] | null;
  /** Ids held back from the table while the reader reads. */
  held: ReadonlySet<string>;
  /** The bottom is the log's root operation. */
  reachedRoot: boolean;
  /** The window reached `WINDOW_CAP` and drops its oldest operations. */
  capped: boolean;
  /** An older page can be loaded. */
  more: boolean;
  loadingOlder: boolean;
  /** Starting over: the records shown are the old ones until the reload answers. */
  reloading: boolean;
  /** The last start-over, until the next selection or scroll clears it. */
  note: Note;
  error: ReefError | null;
  /** The repository is not found: the page is replaced. */
  fatal: ReefError | null;
  /** Bumped by every list from the head that answered: a new set of records. */
  loads: number;
  /** Bumped by every change. */
  version: number;
}

type Phase = "resolving" | "waiting" | "listing" | "retrying" | "ready";

const LOADING: Point = { state: "loading" };

const endedMs = (operation: Operation) => (operation.endedAt ? timestampMs(operation.endedAt) : 0);

export class OperationWindow {
  private snapshot: WindowSnapshot = {
    repository: undefined,
    records: null,
    held: new Set(),
    reachedRoot: false,
    capped: false,
    more: false,
    loadingOlder: false,
    reloading: false,
    note: null,
    error: null,
    fatal: null,
    loads: 0,
    version: 0,
  };
  private readonly notifier: FrameNotifier;
  private phase: Phase = "resolving";
  /** Bumped by every start-over: answers to an older generation are dropped. */
  private generation = 0;
  private repoId: string | undefined;
  private byId = new Map<string, OpRecord>();
  /** Stream operations that arrived while waiting or listing. */
  private buffer = new Map<string, OpRecord>();
  private rootId: string | undefined;
  private token = "";
  private holding = false;
  private held = new Set<string>();
  /** A reload's lower bound: the old bottom's time. */
  private reloadUntil: number | undefined;
  private startOvers = 0;
  private stopped = false;
  private unsubscribe: (() => void) | undefined;
  private older: Promise<void> | undefined;
  /** Live operations not yet applied: applied in order, one parent check at a time. */
  private queue: OpRecord[] = [];
  private checking = false;
  private readonly backoff = new Backoff();
  private retry: Cancel | undefined;
  private readonly points = new Map<string, Point>();
  private readonly locating = new Set<string>();
  private readonly lookups = new Map<string, Promise<Operation>>();

  constructor(
    private readonly connection: Connection,
    private readonly store: OperationStore,
    frame: FrameScheduler,
    private readonly onSessionInvalid: () => void,
    /** As the route gave it: an id, a prefix or a path. */
    private readonly repo: string,
    private readonly now: () => number = Date.now,
  ) {
    this.notifier = new FrameNotifier(frame);
  }

  subscribe = (listener: () => void) => this.notifier.subscribe(listener);
  getSnapshot = () => this.snapshot;

  /** The repository's full id, once `GetRepository` answered. */
  get repositoryId(): string | undefined {
    return this.repoId;
  }

  start(): () => void {
    void this.resolve();
    return () => {
      this.stop();
    };
  }

  stop() {
    this.stopped = true;
    this.unsubscribe?.();
    this.unsubscribe = undefined;
    this.retry?.();
    this.retry = undefined;
  }

  /** While on, new operations are held back from the table behind a count. */
  setHolding(on: boolean) {
    if (this.holding === on) return;
    this.holding = on;
    if (!on && this.held.size > 0) this.showHeld();
  }

  /** Applies every held operation to the table. */
  showHeld() {
    if (this.held.size === 0) return;
    this.held = new Set();
    this.publish();
  }

  clearNote() {
    if (this.snapshot.note) this.update({ note: null });
  }

  /** Loads the next older page; one at a time, so a second call waits for the first. */
  loadOlder(): Promise<void> {
    if (this.older) return this.older;
    if (this.phase !== "ready" || !this.snapshot.more) return Promise.resolve();
    this.older = this.readOlder().finally(() => {
      this.older = undefined;
    });
    return this.older;
  }

  private async readOlder() {
    const generation = this.generation;
    const repo = this.repoId ?? this.repo;
    this.update({ loadingOlder: true });
    try {
      const page = await this.connection.clients.operations.listOperations({
        repo,
        pageSize: Math.min(PAGE_SIZE, WINDOW_CAP - this.byId.size),
        pageToken: this.token,
      });
      if (this.isStopped() || generation !== this.generation) return;
      for (const operation of page.operations) this.take(recordOf(operation));
      this.token = this.rootId === undefined ? page.nextPageToken : "";
      this.update({ loadingOlder: false, error: null });
      this.publish();
    } catch (thrown) {
      if (generation !== this.generation) return;
      this.update({ loadingOlder: false });
      this.fail(thrown, true);
    }
  }

  /**
   * Where `at` (an id or unique prefix) sits: inside the window, older than
   * it can hold (detached), or not yet known (`locatePoint` finds out). An
   * id the window holds is inside at once, with no read.
   */
  point(at: string): Point {
    const known = this.points.get(at);
    if (known && !this.stale(known)) return known;
    if (this.phase !== "ready") return LOADING;
    const id = this.idFor(at);
    if (id === undefined) return LOADING;
    const inside: Point = { state: "inside", id, operation: undefined };
    this.points.set(at, inside);
    return inside;
  }

  /**
   * Starts finding where `at` sits, unless that is known or under way; the
   * answer is published. Before the first list answers it waits: callers ask
   * again on the next snapshot.
   */
  locatePoint(at: string) {
    if (this.repoId === undefined || this.phase !== "ready") return;
    const point = this.point(at);
    if (point.state === "inside") {
      if (!point.operation && !this.locating.has(at)) {
        // The banner's operation, in the background.
        this.locating.add(at);
        const generation = this.generation;
        this.lookup(point.id).then(
          (operation) => {
            this.locating.delete(at);
            if (generation !== this.generation || this.points.get(at) !== point) return;
            this.points.set(at, { ...point, operation });
            this.update({});
          },
          () => {
            this.locating.delete(at);
          },
        );
      }
      return;
    }
    if (point.state !== "loading" || this.locating.has(at)) return;
    this.locating.add(at);
    void this.locate(at).finally(() => {
      this.locating.delete(at);
    });
  }

  /** A point that no longer says where its operation is: off the bottom, or loaded since. */
  private stale(point: Point): boolean {
    if (point.state === "inside") return !this.byId.has(point.id);
    if (point.state === "detached") return this.byId.has(point.operation.id);
    return false;
  }

  /** The id of the record `at` names: itself, or the one record it prefixes. */
  private idFor(at: string): string | undefined {
    if (this.byId.has(at)) return at;
    let found: string | undefined;
    for (const id of this.byId.keys()) {
      if (!id.startsWith(at)) continue;
      if (found !== undefined) return undefined;
      found = id;
    }
    return found;
  }

  /** The operation `at` names, by `GetOperation` (cached). */
  lookup(at: string): Promise<Operation> {
    let found = this.lookups.get(at);
    if (!found) {
      found = this.connection.clients.operations
        .getOperation({ repo: this.repoId ?? this.repo, operationId: at })
        .then((response) => {
          if (!response.operation) throw new Error("The daemon returned no operation.");
          return response.operation;
        });
      found.catch(() => {
        this.lookups.delete(at);
      });
      this.lookups.set(at, found);
    }
    return found;
  }

  /** `id` and its ancestors in the window, newest first: what `ListOperations` lists at it. */
  ancestors(id: string): OpRecord[] {
    const out: OpRecord[] = [];
    const seen = new Set<string>();
    const stack = [id];
    while (stack.length > 0) {
      const next = stack.pop() as string;
      if (seen.has(next)) continue;
      seen.add(next);
      const record = this.byId.get(next);
      if (!record) continue;
      out.push(record);
      stack.push(...record.parentIds);
    }
    return out.sort(newerFirst);
  }

  private async resolve() {
    try {
      const response = await this.connection.clients.repositories.getRepository({
        repo: this.repo,
      });
      if (this.isStopped()) return;
      this.repoId = response.repository?.id ?? this.repo;
      this.update({ repository: response.repository });
      // Subscribing after the lookup is safe: anything delivered before it
      // was recorded before the list, which follows.
      this.unsubscribe = this.store.subscribe((batch) => {
        this.onBatch(batch);
      });
      this.phase = "waiting";
      this.backoff.reset();
      if (this.store.live) void this.list();
    } catch (thrown) {
      this.fail(thrown, false);
      if (this.snapshot.fatal || this.isStopped()) return;
      this.later(() => {
        void this.resolve();
      });
    }
  }

  /** Runs `again` after the backoff's next delay, once. */
  private later(again: () => void) {
    this.retry?.();
    this.retry = after(this.backoff.next(), () => {
      this.retry = undefined;
      again();
    });
  }

  private onBatch(batch: OperationBatch) {
    if (this.isStopped()) return;
    if (batch.reset) {
      this.startOver("reset");
      return;
    }
    const records = batch.operations
      .filter((operation) => operation.repoId === this.repoId)
      .map(recordOf);
    if (this.phase !== "ready") {
      for (const record of records) this.buffer.set(record.id, record);
      if (this.phase === "waiting" && (batch.live || this.store.live)) void this.list();
      return;
    }
    if (records.length === 0) return;
    this.queue.push(...records);
    void this.drain();
  }

  /** Applies queued live operations in order; a missing parent is checked first. */
  private async drain() {
    if (this.checking) return;
    const generation = this.generation;
    let changed = false;
    for (;;) {
      const record = this.queue[0];
      if (!record) break;
      if (this.byId.has(record.id) || this.belowWindow(record)) {
        this.queue.shift();
        continue;
      }
      const missing = record.parentIds.filter(
        (parent) => !this.byId.has(parent) && parent !== this.rootId,
      );
      if (missing.length > 0) {
        if (changed) this.publish();
        // A window down to the root holds every parent there is.
        const below = !this.snapshot.reachedRoot && (await this.belowBottom(missing));
        if (this.isStopped() || generation !== this.generation) return;
        if (!below) {
          this.startOver("gc");
          return;
        }
      }
      this.queue.shift();
      this.byId.set(record.id, record);
      if (this.holding) this.held.add(record.id);
      changed = true;
    }
    if (changed) this.publish();
  }

  /**
   * An operation that ended before the bottom of a window that does not reach
   * the root (a frame replayed from before a GC, under old ids): it belongs to
   * the period not loaded, and a page lists it there.
   */
  private belowWindow(record: OpRecord): boolean {
    const bottom = this.snapshot.records?.at(-1);
    return !this.snapshot.reachedRoot && bottom !== undefined && newerFirst(record, bottom) > 0;
  }

  /**
   * Whether every parent in `ids` is a live operation that ended before the
   * window's bottom: a writer that started from an older head. A rewritten
   * parent ends within the window's period, or is a pre-GC id, which still
   * answers `GetOperation` but refuses a walk (`OPERATION_LOG_REWRITTEN`):
   * so each is read by a one-operation list at it. One that cannot be read
   * is treated as rewritten.
   */
  private async belowBottom(ids: string[]): Promise<boolean> {
    this.checking = true;
    try {
      const bottom = this.snapshot.records?.at(-1)?.endedAt ?? Number.NEGATIVE_INFINITY;
      for (const id of ids) {
        const page = await this.connection.clients.operations.listOperations({
          repo: this.repoId ?? this.repo,
          atOperation: id,
          pageSize: 1,
        });
        const parent = page.operations[0];
        if (!parent || endedMs(parent) >= bottom) return false;
      }
      return true;
    } catch {
      return false;
    } finally {
      this.checking = false;
    }
  }

  /** Lists from the head: one page, or down to `reloadUntil` after a start-over. */
  private async list() {
    if (this.phase !== "waiting" || this.repoId === undefined) return;
    this.phase = "listing";
    const generation = this.generation;
    const until = this.reloadUntil;
    const listed = new Map<string, OpRecord>();
    let root: string | undefined;
    let token = "";
    try {
      for (;;) {
        const page = await this.connection.clients.operations.listOperations({
          repo: this.repoId,
          pageSize: Math.min(PAGE_SIZE, WINDOW_CAP - listed.size),
          pageToken: token,
        });
        if (this.isStopped() || generation !== this.generation) return;
        let bottom: number | undefined;
        for (const operation of page.operations) {
          const record = recordOf(operation);
          if (isRoot(record)) root = record.id;
          else listed.set(record.id, record);
          bottom = record.endedAt;
        }
        token = page.nextPageToken;
        const done =
          !token ||
          root !== undefined ||
          listed.size >= WINDOW_CAP ||
          until === undefined ||
          (bottom !== undefined && bottom <= until);
        if (done) break;
      }
    } catch (thrown) {
      if (generation !== this.generation) return;
      this.phase = "retrying";
      this.fail(thrown, true);
      // Unless that started over: retry after the backoff.
      if (this.isRetrying() && !this.isStopped()) {
        this.later(() => {
          if (this.phase !== "retrying") return;
          this.phase = "waiting";
          if (this.store.live) void this.list();
        });
      }
      return;
    }
    for (const [id, record] of this.buffer) listed.set(id, record);
    this.byId = listed;
    this.buffer = new Map();
    this.rootId = root;
    this.token = root === undefined ? token : "";
    this.held = new Set();
    this.reloadUntil = undefined;
    this.startOvers = 0;
    this.queue = [];
    this.backoff.reset();
    this.phase = "ready";
    this.update({
      reachedRoot: root !== undefined,
      capped: false,
      reloading: false,
      error: null,
      loads: this.snapshot.loads + 1,
    });
    this.publish();
  }

  private startOver(kind: "reset" | "gc") {
    this.generation += 1;
    this.startOvers += 1;
    const records = this.snapshot.records;
    this.reloadUntil = records && records.length > 0 ? records.at(-1)?.endedAt : undefined;
    this.buffer = new Map();
    this.queue = [];
    this.points.clear();
    this.lookups.clear();
    this.retry?.();
    this.retry = undefined;
    this.phase = "waiting";
    this.update({ note: { kind, at: this.now() }, loadingOlder: false, reloading: true });
    if (this.startOvers > MAX_START_OVERS) {
      this.update({
        error: {
          code: Code.Unknown,
          kind: "",
          reason: "",
          message: "The operation log kept changing under the feed; reload the page.",
          hint: "",
          next: [],
          metadata: {},
        },
      });
      return;
    }
    if (this.store.live) void this.list();
  }

  /** Adds a listed record below the window, unless held already. */
  private take(record: OpRecord) {
    if (isRoot(record)) {
      this.rootId = record.id;
      this.token = "";
      this.update({ reachedRoot: true });
      return;
    }
    if (!this.byId.has(record.id)) this.byId.set(record.id, record);
  }

  /** Finds a point the window does not hold: older pages, else one page ending at it. */
  private async locate(at: string) {
    const generation = this.generation;
    const set = (point: Point) => {
      if (generation !== this.generation) return;
      this.points.set(at, point);
      this.update({});
    };
    try {
      const operation = await this.lookup(at);
      if (this.isStopped() || generation !== this.generation) return;
      // Older pages only for an operation older than the bottom. One no
      // older that the window does not hold is from before a GC rewrote the
      // log (the window holds the kept operations under new ids).
      const bottom = this.snapshot.records?.at(-1)?.endedAt ?? Number.POSITIVE_INFINITY;
      if (endedMs(operation) < bottom) {
        while (!this.byId.has(operation.id) && this.phase === "ready" && this.snapshot.more) {
          const before = this.byId.size;
          await this.loadOlder();
          if (generation !== this.generation || this.byId.size === before) break;
        }
      }
      if (this.isStopped() || generation !== this.generation) return;
      if (this.byId.has(operation.id)) {
        set({ state: "inside", id: operation.id, operation });
        return;
      }
      const page = await this.connection.clients.operations.listOperations({
        repo: this.repoId ?? this.repo,
        atOperation: operation.id,
        pageSize: PAGE_SIZE,
      });
      if (this.isStopped()) return;
      set({
        state: "detached",
        operation,
        records: page.operations.map(recordOf).filter((r) => !isRoot(r)),
      });
    } catch (thrown) {
      const error = reefError(thrown);
      if (isSessionInvalid(error)) {
        this.onSessionInvalid();
        return;
      }
      // A pre-GC point says so with the daemon's reason and hint.
      set({ state: "error", error });
    }
  }

  /** Re-derives the published records from `byId`, applying the cap. */
  private publish() {
    let records = [...this.byId.values()].sort(newerFirst);
    let capped = this.snapshot.capped;
    if (records.length > WINDOW_CAP) {
      for (const dropped of records.slice(WINDOW_CAP)) {
        this.byId.delete(dropped.id);
        this.held.delete(dropped.id);
      }
      records = records.slice(0, WINDOW_CAP);
      capped = true;
      // The bottom is no longer the root.
      this.rootId = undefined;
      this.update({ reachedRoot: false });
    }
    if (records.length >= WINDOW_CAP) {
      capped = true;
      this.token = "";
    }
    this.update({
      records,
      held: this.held,
      capped,
      more: this.token !== "" && !capped,
      version: this.snapshot.version + 1,
    });
  }

  /** A call, so TypeScript does not narrow `phase` across `fail`, which may start over. */
  private isRetrying(): boolean {
    return this.phase === "retrying";
  }

  /** A call, so TypeScript does not narrow `stopped` across an await. */
  private isStopped(): boolean {
    return this.stopped;
  }

  /**
   * `pageFailed`: a list or page read failed, where a rewritten log means
   * starting over rather than an error.
   */
  private fail(thrown: unknown, pageFailed: boolean) {
    if (this.isStopped()) return;
    const error = reefError(thrown);
    if (isSessionInvalid(error)) {
      this.onSessionInvalid();
      return;
    }
    if (
      pageFailed &&
      (error.reason === OPERATION_LOG_REWRITTEN || error.reason === OPERATION_NOT_FOUND) &&
      this.startOvers < MAX_START_OVERS
    ) {
      this.startOver("gc");
      return;
    }
    if (error.reason === REPOSITORY_NOT_FOUND && this.snapshot.records === null) {
      this.update({ fatal: error });
      return;
    }
    this.update({ error });
  }

  private update(change: Partial<WindowSnapshot>) {
    this.snapshot = { ...this.snapshot, ...change };
    this.notifier.notify();
  }
}
