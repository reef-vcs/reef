// The triage inbox's data (R3.33, ADR 0088; #372, ADR 0091): the browser's
// one `WatchInbox` stream (live/inbox.ts), every repository on the home page
// or one on its Inbox tab, held in place per bucket (inbox/held.ts), with
// each repository's digest of what landed since the person last looked.
//
// The daemon says when the inbox changed; this model never reads it again
// itself. A repository registered after the page opened arrives on the
// stream like any other.
//
// The digest follows the person's mark (inbox/digest.ts): once the page has
// marked a digest seen (`markSeen`, after it was on screen), what it showed
// stays for the rest of the visit and later landings add to it.
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import {
  InboxBucket,
  InboxService,
  type InboxItem,
  type LandedDigest,
} from "../gen/reef/api/v1/inbox_pb";
import { RepositoryService } from "../gen/reef/api/v1/repositories_pb";
import { holdDigest, shown, type VisitDigest } from "../inbox/digest";
import { holdRead, showNew, type HeldBucket } from "../inbox/held";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import type { InboxStore, SectionState } from "../live/inbox";

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
  needs: HeldBucket | null;
  watching: HeldBucket | null;
  /** Repositories with something landed since the base, in the order sent. */
  digests: readonly DigestState[];
  /** Repositories that fail to read; their last items stand. */
  failed: readonly SectionState[];
  /** The daemon's count of what needs the person now: the title's number. */
  needsTotal: number;
  reading: boolean;
  /** The repository the tab names could not be found, or a mark failed. */
  error: ReefError | null;
  version: number;
}

const EMPTY: InboxSnapshot = {
  needs: null,
  watching: null,
  digests: [],
  failed: [],
  needsTotal: 0,
  reading: true,
  error: null,
  version: 0,
};

const ms = (item: InboxItem) =>
  item.since ? Number(item.since.seconds) * 1000 + Math.floor(item.since.nanos / 1e6) : 0;

/** Every section's items of one bucket, newest first. */
function mergeSections(
  sections: readonly SectionState[],
  bucket: BucketName,
): { items: InboxItem[]; total: number } {
  const wanted = bucket === "needs" ? InboxBucket.NEEDS_YOU_NOW : InboxBucket.WATCHING;
  const items: InboxItem[] = [];
  let total = 0;
  for (const section of sections) {
    for (const item of section.items.values()) if (item.bucket === wanted) items.push(item);
    total += bucket === "needs" ? section.needsYouNowTotal : section.watchingTotal;
  }
  items.sort((a, b) => ms(b) - ms(a) || a.key.localeCompare(b.key));
  return { items, total };
}

export class InboxModel {
  private snapshot: InboxSnapshot = EMPTY;
  private readonly notifier: FrameNotifier;
  private whole = false;
  private readonly visits = new Map<string, VisitDigest>();
  private readonly answered = new Set<string>();
  private readonly marked = new Map<string, bigint>();
  /** By repository, each seq this page marked and the digest it showed then, for the bank. */
  private readonly markedSeqs = new Map<string, Map<bigint, LandedDigest>>();
  /** The tab's repository id once resolved; undefined on the home page. */
  private repoId: string | undefined;
  private stopped = false;
  /** The daemon keeps no mark for this caller. */
  private markRefused = false;

  constructor(
    private readonly connection: Connection,
    private readonly inbox: InboxStore,
    frame: FrameScheduler,
    private readonly onSessionInvalid: () => void,
    /** The repository the tab names (id, prefix or path); undefined for every one. */
    private readonly repo: string | undefined,
  ) {
    this.notifier = new FrameNotifier(frame);
  }

  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  getSnapshot = (): InboxSnapshot => this.snapshot;

  start(): () => void {
    const unsubscribe = this.inbox.subscribe(() => {
      this.project();
    });
    if (this.repo === undefined) this.project();
    else void this.resolve(this.repo);
    return () => {
      this.stopped = true;
      unsubscribe();
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
  }

  /**
   * The digest of `repoId` was on screen long enough: mark it seen through
   * `seq`, the newest landing it showed, once per seq. What it showed stays
   * for the rest of the visit.
   */
  async markSeen(repoId: string, seq: bigint) {
    if (this.markRefused || seq <= (this.marked.get(repoId) ?? 0n)) return;
    const visit = this.visits.get(repoId);
    const onScreen = visit && shown(visit);
    // A landing arrived after the render that armed this mark: the bank
    // would count it twice, and the next render marks again.
    if (onScreen?.throughSeq !== seq) return;
    this.marked.set(repoId, seq);
    const seqs = this.markedSeqs.get(repoId) ?? new Map<bigint, LandedDigest>();
    seqs.set(seq, onScreen);
    this.markedSeqs.set(repoId, seqs);
    this.project();
    try {
      await client(this.connection, InboxService).markInboxSeen({
        marks: [{ repo: repoId, seq }],
      });
    } catch (thrown) {
      this.marked.delete(repoId);
      this.markedSeqs.get(repoId)?.delete(seq);
      this.project();
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

  private async resolve(repo: string) {
    try {
      const response = await client(this.connection, RepositoryService).getRepository({ repo });
      if (this.stopped) return;
      this.repoId = response.repository?.id ?? repo;
      this.project();
    } catch (thrown) {
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

  /** The store's inbox, as this page shows it. */
  private project() {
    if (this.stopped) return;
    if (this.repo !== undefined && this.repoId === undefined) return;
    const sections = [...this.inbox.sections.values()].filter(
      (section) => this.repoId === undefined || section.repoId === this.repoId,
    );
    const reading =
      !this.inbox.synced &&
      (this.repoId === undefined || !sections.some((section) => section.listed));
    if (reading && !sections.some((section) => section.listed)) return;
    const hold = this.whole;
    this.whole ||= !reading;
    for (const repoId of [...this.visits.keys()]) {
      if (!sections.some((section) => section.repoId === repoId)) this.visits.delete(repoId);
    }
    const digests: DigestState[] = [];
    for (const section of sections) {
      if (!section.landed) continue;
      const marked = this.marked.get(section.repoId) ?? 0n;
      const visit = holdDigest(
        this.visits.get(section.repoId) ?? null,
        section.landed,
        this.markedSeqs.get(section.repoId) ?? new Map(),
      );
      this.visits.set(section.repoId, visit);
      const digest = shown(visit);
      if (digest.count > 0n) digests.push({ repoId: section.repoId, digest, marked });
    }
    const needs = mergeSections(sections, "needs");
    const watching = mergeSections(sections, "watching");
    const needsKeys = new Set(needs.items.map((item) => item.key));
    const watchingKeys = new Set(watching.items.map((item) => item.key));
    this.snapshot = {
      ...this.snapshot,
      needs: holdRead(hold ? this.snapshot.needs : null, {
        ...needs,
        elsewhere: watchingKeys,
        answered: this.answered,
      }),
      watching: holdRead(hold ? this.snapshot.watching : null, {
        ...watching,
        elsewhere: needsKeys,
        answered: this.answered,
      }),
      digests,
      failed: sections.filter((section) => section.error),
      needsTotal: needs.total,
      reading,
      version: this.snapshot.version + 1,
    };
    this.notifier.notify();
  }
}
