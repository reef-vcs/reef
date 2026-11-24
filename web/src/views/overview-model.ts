// The overview's data: registered repositories and each one's latest
// operation, live from the operation stream with no gap.
//
// The stream is open before anything is read. Once it is live, each
// repository's newest operation is listed (`page_size: 1`, eight calls at a
// time), and the model keeps whichever of the listed one and every streamed
// one ended last. An operation recorded between the two calls arrives on the
// stream, so none is missed in either order.
import { client, type Connection } from "../api/connection";
import { streamsOf } from "../api/streams";
import { isPassing, isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { type Operation, OperationService } from "../gen/reef/api/v1/operations_pb";
import { type Repository, RepositoryService } from "../gen/reef/api/v1/repositories_pb";
import { after, Backoff, type Cancel } from "../live/backoff";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import type { OperationBatch, OperationStore } from "../live/operations";

export interface OverviewSnapshot {
  /** Null until the first list returns. */
  repositories: Repository[] | null;
  latest: ReadonlyMap<string, Operation>;
  error: ReefError | null;
}

/** At most one reload of the repositories per second. */
export const RELOAD_EVERY_MS = 1000;
const LIST_CONCURRENCY = 8;

function endedAt(operation: Operation): bigint {
  const t = operation.endedAt;
  return t ? t.seconds * 1_000_000_000n + BigInt(t.nanos) : 0n;
}

/** True if `a` ended after `b`, ties broken by id. */
export function isLater(a: Operation, b: Operation): boolean {
  const ta = endedAt(a);
  const tb = endedAt(b);
  return ta !== tb ? ta > tb : a.id > b.id;
}

export class OverviewModel {
  private snapshot: OverviewSnapshot = { repositories: null, latest: new Map(), error: null };
  private readonly latest = new Map<string, Operation>();
  private repositories: Repository[] | null = null;
  /** Why the repositories, and the newest operation of each, were not read last. */
  private error: ReefError | null = null;
  private readonly latestErrors = new Map<string, ReefError>();
  private readonly backoff = new Backoff(Backoff.PAGE_MS);
  private retry: Cancel | undefined;
  /** Repositories whose newest operation has been listed since the last reset. */
  private readonly listed = new Set<string>();
  /** Bumped by a reset, so answers to earlier reads are dropped. */
  private generation = 0;
  private loading = false;
  /** The first list has been asked for: it waits for the stream to be live. */
  private requested = false;
  /** A reload was asked for during a load, so another follows it. */
  private reloadAgain = false;
  private lastLoad = Number.NEGATIVE_INFINITY;
  private pendingReload: Cancel | undefined;
  private stopped = false;
  private readonly notifier: FrameNotifier;

  constructor(
    private readonly connection: Connection,
    private readonly operations: OperationStore,
    frame: FrameScheduler,
    private readonly onSessionInvalid: () => void,
  ) {
    this.notifier = new FrameNotifier(frame);
  }

  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  getSnapshot = (): OverviewSnapshot => this.snapshot;

  start(): () => void {
    const unsubscribe = this.operations.subscribe((batch) => {
      this.onBatch(batch);
    });
    // Nothing is read before the stream is live (a follower tab may already
    // be), so an operation recorded after the read arrives on the stream.
    if (this.operations.live) this.requestFirst();
    const unsubscribeResume = streamsOf(this.connection).onResume(() => {
      if (!this.requested) return;
      this.lastLoad = Number.NEGATIVE_INFINITY;
      this.requestReload();
    });
    return () => {
      this.stopped = true;
      unsubscribe();
      unsubscribeResume();
      this.retry?.();
      this.pendingReload?.();
    };
  }

  private requestFirst() {
    if (this.requested) return;
    this.requested = true;
    void this.load();
  }

  private onBatch(batch: OperationBatch) {
    if (batch.live) this.requestFirst();
    if (batch.reset && this.requested) {
      this.generation += 1;
      this.latest.clear();
      this.listed.clear();
      this.requestReload();
    }
    const known = this.repositories && new Set(this.repositories.map((r) => r.id));
    for (const operation of batch.operations) {
      this.fold(operation);
      if (known && !known.has(operation.repoId)) this.requestReload();
    }
    if (batch.live) this.listLatest();
    this.changed();
  }

  private fold(operation: Operation) {
    const held = this.latest.get(operation.repoId);
    if (!held || isLater(operation, held)) this.latest.set(operation.repoId, operation);
  }

  private requestReload() {
    if (this.loading) {
      this.reloadAgain = true;
      return;
    }
    if (this.pendingReload) return;
    const wait = Math.min(this.lastLoad + RELOAD_EVERY_MS - Date.now(), RELOAD_EVERY_MS);
    if (wait <= 0) {
      void this.load();
      return;
    }
    this.pendingReload = after(wait, () => {
      this.pendingReload = undefined;
      void this.load();
    });
  }

  private async load() {
    if (this.stopped) return;
    this.loading = true;
    this.lastLoad = Date.now();
    const generation = this.generation;
    try {
      const repositories: Repository[] = [];
      let pageToken = "";
      do {
        const page = await client(this.connection, RepositoryService).listRepositories({
          pageToken,
        });
        repositories.push(...page.repositories);
        pageToken = page.nextPageToken;
      } while (pageToken);
      if (this.isStopped()) return;
      this.repositories = repositories;
      this.error = null;
      const ids = new Set(repositories.map((r) => r.id));
      for (const id of this.latestErrors.keys()) if (!ids.has(id)) this.latestErrors.delete(id);
      if (generation === this.generation) this.listLatest();
    } catch (thrown) {
      this.error = this.failed(thrown);
      this.later(this.error);
    } finally {
      this.loading = false;
      if (this.reloadAgain) {
        this.reloadAgain = false;
        this.requestReload();
      }
      this.changed();
    }
  }

  /** Lists the newest operation of each repository not yet listed, once the stream is live. */
  private listLatest() {
    if (!this.operations.live || !this.repositories || this.stopped) return;
    const queue = this.repositories.map((r) => r.id).filter((id) => !this.listed.has(id));
    for (const id of queue) this.listed.add(id);
    const generation = this.generation;
    const worker = async () => {
      for (let repo = queue.shift(); repo !== undefined; repo = queue.shift()) {
        try {
          const page = await client(this.connection, OperationService).listOperations({
            repo,
            pageSize: 1,
          });
          if (this.isStopped() || generation !== this.generation) return;
          for (const operation of page.operations) this.fold(operation);
          this.latestErrors.delete(repo);
        } catch (thrown) {
          if (this.isStopped() || generation !== this.generation) return;
          this.listed.delete(repo);
          if (!this.repositories?.some((r) => r.id === repo)) continue;
          const error = this.failed(thrown);
          if (error) this.latestErrors.set(repo, error);
          this.later(error);
        }
        this.changed();
      }
    };
    for (let i = 0; i < Math.min(LIST_CONCURRENCY, queue.length); i++) void worker();
  }

  /** A call, so TypeScript does not narrow `stopped` across an await. */
  private isStopped(): boolean {
    return this.stopped;
  }

  /** Why a read failed; an invalid session ends the page. */
  private failed(thrown: unknown): ReefError | null {
    const error = reefError(thrown);
    if (!isSessionInvalid(error)) return error;
    this.onSessionInvalid();
    return null;
  }

  /** Reads what failed again after the backoff, if another try may not meet `error`. */
  private later(error: ReefError | null) {
    if (this.retry || this.stopped || !error || !isPassing(error)) return;
    this.retry = after(this.backoff.next(), () => {
      this.retry = undefined;
      if (this.error) this.requestReload();
      else this.listLatest();
    });
  }

  private changed() {
    this.snapshot = {
      repositories: this.repositories,
      latest: new Map(this.latest),
      error: this.error ?? [...this.latestErrors.values()][0] ?? null,
    };
    this.notifier.notify();
  }
}
