// The triage inbox's data (R3.33, ADR 0088): `GetInbox` over every
// repository (the home page) or one (its Inbox tab), held in place per
// bucket (inbox/held.ts), with each repository's digest of what landed
// since the person last looked.
//
// It reads live, as the proposal page does: nothing is read until the
// operation stream is live, then a read follows any operation of a kind the
// last response names (`refresh_operations`: comments, transitions,
// landings, conflicts, workspaces; never a snapshot alone) in a repository
// read, and any change to a presence entry of a kind it names
// (`refresh_presence`: proposals' statuses and workspaces' attention, which
// move with no operation). Reads are paced (live/paced.ts), one in flight.
//
// A digest keeps the base its visit started from: later reads send it as
// `digest_from`, so marking it seen (the page does once it was on screen,
// `markSeen`) never empties what the person is reading. The next visit
// starts from the mark.
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import {
  InboxService,
  type InboxItem,
  type InboxMark,
  type InboxSection,
  type LandedDigest,
} from "../gen/reef/api/v1/inbox_pb";
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { PresenceKind } from "../gen/reef/api/v1/presence_pb";
import { holdRead, showNew, type HeldBucket } from "../inbox/held";
import { NeedsWatcher } from "../inbox/needs";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import { stateSignature } from "../live/kinds";
import type { OperationBatch, OperationStore } from "../live/operations";
import { Paced } from "../live/paced";
import type { PresenceStore } from "../live/presence";

export type BucketName = "needs" | "watching";

/** Refusals that mean the daemon keeps no mark for this caller. */
const NO_MARK: ReadonlySet<string> = new Set(["REMOTE_IDENTITY_REQUIRED", "REMOTE_ACCESS_DENIED"]);

/** One repository's digest, as this visit shows it. */
export interface DigestState {
  repoId: string;
  digest: LandedDigest;
  /** The newest transition this page has marked seen; 0n before. */
  marked: bigint;
}

export interface InboxSnapshot {
  /** Null until the first read answers. */
  needs: HeldBucket | null;
  watching: HeldBucket | null;
  /** Repositories with something landed since the base, in the order read. */
  digests: readonly DigestState[];
  /** Repositories that failed to read; the others stand. */
  failed: readonly InboxSection[];
  /** The daemon's count of what needs the person now: the title's number. */
  needsTotal: number;
  /** Needs-you-now items new at the latest read (inbox/needs.ts). */
  added: readonly InboxItem[];
  /** A failed read; the last good buckets stay. */
  error: ReefError | null;
  version: number;
}

const EMPTY: InboxSnapshot = {
  needs: null,
  watching: null,
  digests: [],
  failed: [],
  needsTotal: 0,
  added: [],
  error: null,
  version: 0,
};

/** Every section's items of one bucket, newest first. */
function mergeSections(
  sections: readonly InboxSection[],
  bucket: BucketName,
): { items: InboxItem[]; total: number } {
  const items: InboxItem[] = [];
  let total = 0;
  for (const section of sections) {
    if (section.error) continue;
    items.push(...(bucket === "needs" ? section.needsYouNow : section.watching));
    total += bucket === "needs" ? section.needsYouNowTotal : section.watchingTotal;
  }
  const ms = (item: InboxItem) =>
    item.since ? Number(item.since.seconds) * 1000 + Math.floor(item.since.nanos / 1e6) : 0;
  items.sort((a, b) => ms(b) - ms(a) || a.key.localeCompare(b.key));
  return { items, total };
}

export class InboxModel {
  private snapshot: InboxSnapshot = EMPTY;
  private readonly notifier: FrameNotifier;
  private readonly paced: Paced;
  private readonly needs = new NeedsWatcher();
  /**
   * Each repository's base for this visit, from its first digest: its seq,
   * and the words its first read gave (a mark or the last day, since when),
   * which later reads, sent from this base, cannot know.
   */
  private readonly bases = new Map<string, Pick<LandedDigest, "fromSeq" | "fromMark" | "since">>();
  /** Each repository's last section read without an error. */
  private readonly good = new Map<string, InboxSection>();
  private readonly answered = new Set<string>();
  private readonly marked = new Map<string, bigint>();
  /** Repository ids read, once known; every repository's operations ask until then. */
  private repoIds: Set<string> | null = null;
  private operationKinds: ReadonlySet<OperationKind> | null = null;
  private presenceKinds: readonly PresenceKind[] = [PresenceKind.AUTOMATIC, PresenceKind.ATTENTION];
  private signature = "";
  private requested = false;
  private waiting = false;
  private stopped = false;
  /** The daemon keeps no mark for this caller. */
  private markRefused = false;
  /** The latest read's sections, in the order read. */
  private sections: readonly InboxSection[] = [];

  constructor(
    private readonly connection: Connection,
    private readonly operations: OperationStore,
    private readonly presence: PresenceStore,
    frame: FrameScheduler,
    private readonly onSessionInvalid: () => void,
    /** The repository the tab names (id, prefix or path); undefined for every one. */
    private readonly repo: string | undefined,
  ) {
    this.notifier = new FrameNotifier(frame);
    this.paced = new Paced(() => this.refresh());
  }

  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  getSnapshot = (): InboxSnapshot => this.snapshot;

  start(): () => void {
    const unsubscribe = this.operations.subscribe((batch) => {
      this.onBatch(batch);
    });
    const unsubscribePresence = this.presence.subscribe(() => {
      this.onPresence();
    });
    this.signature = this.presenceSignature();
    if (this.operations.live) this.requestFirst();
    return () => {
      this.stopped = true;
      unsubscribe();
      unsubscribePresence();
      this.paced.stop();
    };
  }

  /** "Show" in a bucket's header. */
  show(bucket: BucketName) {
    const held = bucket === "needs" ? this.snapshot.needs : this.snapshot.watching;
    if (!held) return;
    this.update(bucket === "needs" ? { needs: showNew(held) } : { watching: showNew(held) });
  }

  /** The page answered the help request `key`: its row says so when it leaves. */
  answeredItem(key: string) {
    this.answered.add(key);
    this.paced.requestNow();
  }

  /**
   * The digest of `repoId` was on screen long enough: mark it seen through
   * `seq`, the newest landing it showed, once per seq. The digest keeps its
   * base, so it stays as it is for the rest of the visit.
   */
  async markSeen(repoId: string, seq: bigint) {
    if (this.markRefused || seq <= (this.marked.get(repoId) ?? 0n)) return;
    this.marked.set(repoId, seq);
    this.publishDigests();
    try {
      await client(this.connection, InboxService).markInboxSeen({
        marks: [{ repo: repoId, seq }],
      });
    } catch (thrown) {
      this.marked.delete(repoId);
      this.publishDigests();
      const error = reefError(thrown);
      // A caller the daemon keeps no mark for (a remote `read` token, or one
      // linked to no identity) reads the inbox as it is: stop marking.
      if (NO_MARK.has(error.reason)) {
        this.markRefused = true;
        return;
      }
      if (!this.stopped) this.fail(thrown);
    }
  }

  private update(patch: Partial<InboxSnapshot>) {
    this.snapshot = { ...this.snapshot, ...patch, version: this.snapshot.version + 1 };
    this.notifier.notify();
  }

  private fail(thrown: unknown) {
    const error = reefError(thrown);
    if (isSessionInvalid(error)) this.onSessionInvalid();
    else this.update({ error });
  }

  private requestFirst() {
    if (this.requested) return;
    this.requested = true;
    this.paced.request();
  }

  private asks(kind: OperationKind, repoId: string): boolean {
    if (this.repoIds !== null && !this.repoIds.has(repoId)) return false;
    return this.operationKinds === null || this.operationKinds.has(kind);
  }

  private onBatch(batch: OperationBatch) {
    let ask = batch.reset || this.waiting;
    for (const operation of batch.operations) {
      if (this.asks(operation.kind, operation.repoId)) ask = true;
    }
    if (batch.live && !this.requested) {
      this.requestFirst();
      return;
    }
    if (!ask || !this.requested) return;
    this.waiting = !batch.live;
    if (batch.live) this.paced.request();
  }

  private presenceSignature(): string {
    const scope = this.repoIds?.size === 1 ? [...this.repoIds][0] : "";
    return this.presenceKinds
      .map((kind) => stateSignature(this.presence.entries.values(), kind, scope ?? ""))
      .join(";");
  }

  private onPresence() {
    const signature = this.presenceSignature();
    if (signature === this.signature) return;
    this.signature = signature;
    if (this.requested && this.operations.live) this.paced.request();
  }

  private refresh(): Promise<void> | undefined {
    if (this.stopped) return undefined;
    if (!this.operations.live) {
      this.waiting = true;
      return undefined;
    }
    return this.read();
  }

  private async read() {
    const digestFrom: InboxMark[] = [...this.bases].map(
      ([repo, base]) => ({ repo, seq: base.fromSeq }) as InboxMark,
    );
    let response;
    try {
      response = await client(this.connection, InboxService).getInbox({
        repos: this.repo ? [this.repo] : [],
        digestFrom,
      });
    } catch (thrown) {
      if (!this.stopped) this.fail(thrown);
      return;
    }
    if (this.stopped) return;
    // A repository no longer read (forgotten) takes its base and rows along.
    const read = new Set(response.sections.map((section) => section.repoId));
    for (const map of [this.bases, this.good]) {
      for (const repoId of [...map.keys()]) if (!read.has(repoId)) map.delete(repoId);
    }
    // A repository that fails to read keeps what it last showed.
    const sections = response.sections.map((section) => {
      if (!section.error) {
        this.good.set(section.repoId, section);
        return section;
      }
      return this.good.get(section.repoId) ?? section;
    });
    this.repoIds = new Set(sections.map((section) => section.repoId));
    this.operationKinds = new Set(response.refreshOperations);
    if (response.refreshPresence.length > 0) this.presenceKinds = response.refreshPresence;
    this.signature = this.presenceSignature();
    for (const section of sections) {
      const landed = section.landed;
      if (landed && !this.bases.has(section.repoId)) {
        const { fromSeq, fromMark, since } = landed;
        this.bases.set(section.repoId, { fromSeq, fromMark, since });
      }
    }
    this.sections = sections;
    const needs = mergeSections(sections, "needs");
    const watching = mergeSections(sections, "watching");
    const needsKeys = new Set(needs.items.map((item) => item.key));
    const watchingKeys = new Set(watching.items.map((item) => item.key));
    this.snapshot = {
      ...this.snapshot,
      needs: holdRead(this.snapshot.needs, {
        ...needs,
        elsewhere: watchingKeys,
        answered: this.answered,
      }),
      watching: holdRead(this.snapshot.watching, {
        ...watching,
        elsewhere: needsKeys,
        answered: this.answered,
      }),
      failed: response.sections.filter((section) => section.error),
      needsTotal: needs.total,
      added: this.needs.next(needs.items),
      error: null,
    };
    this.publishDigests();
  }

  private publishDigests() {
    const digests: DigestState[] = [];
    for (const section of this.sections) {
      const digest = section.landed;
      if (!digest || digest.count === 0n) continue;
      const base = this.bases.get(section.repoId);
      digests.push({
        repoId: section.repoId,
        digest: base ? { ...digest, fromMark: base.fromMark, since: base.since } : digest,
        marked: this.marked.get(section.repoId) ?? 0n,
      });
    }
    this.update({ digests });
  }
}
