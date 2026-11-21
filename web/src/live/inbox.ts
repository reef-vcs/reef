// The inbox stream (#372, ADR 0091): one `WatchInbox` per browser, beside
// the operation and presence streams (live/share.ts). The daemon sends the
// whole inbox, `synced` on the frame that completes it, then per repository
// the items that entered or changed (each with its bucket), the keys that
// left, and the section's state. There is no cursor: a new stream's first
// inbox replaces what was held, and until it is complete the old one stays
// on screen. The daemon decides when the inbox changed; this store only
// folds what it says.
import { create } from "@bufbuild/protobuf";
import {
  InboxSectionChangeSchema,
  WatchInboxResponseSchema,
  type InboxError,
  type InboxItem,
  type LandedDigest,
  type WatchInboxResponse,
} from "../gen/reef/api/v1/inbox_pb";
import { isSessionInvalid, reefError } from "../api/errors";
import { FrameNotifier, type FrameScheduler } from "./frame";
import type { StreamHooks } from "./operations";

/** One repository's inbox as the stream last said. */
export interface SectionState {
  repoId: string;
  /** Both buckets' listed items, by key; each carries its bucket. */
  items: ReadonlyMap<string, InboxItem>;
  needsYouNowTotal: number;
  watchingTotal: number;
  landed: LandedDigest | undefined;
  /** Set while the repository fails to read; its items stand. */
  error: InboxError | undefined;
}

type Sections = Map<string, SectionState>;

function fold(sections: Sections, frame: WatchInboxResponse) {
  for (const change of frame.sections) {
    if (change.removed) {
      sections.delete(change.repoId);
      continue;
    }
    const items = new Map(sections.get(change.repoId)?.items);
    for (const key of change.left) items.delete(key);
    for (const item of change.items) items.set(item.key, item);
    sections.set(change.repoId, {
      repoId: change.repoId,
      items,
      needsYouNowTotal: change.needsYouNowTotal,
      watchingTotal: change.watchingTotal,
      landed: change.landed,
      error: change.error,
    });
  }
}

export class InboxStore {
  /** By repository, in the order the daemon first sent them. */
  sections: ReadonlyMap<string, SectionState> = new Map();
  /** The held inbox is complete: the current stream has sent `synced`. */
  synced = false;
  /** Increases with every change, for `useSyncExternalStore`. */
  version = 0;
  /** A new stream's inbox, until it is complete. */
  private next: Sections | null = null;
  private readonly notifier: FrameNotifier;

  constructor(frame: FrameScheduler) {
    this.notifier = new FrameNotifier(frame);
  }

  subscribe(listener: () => void): () => void {
    return this.notifier.subscribe(listener);
  }

  /** A new stream starts: its first inbox will replace the held one. */
  beginResync() {
    this.next = new Map();
    this.changed();
  }

  applyFrame(frame: WatchInboxResponse) {
    if (this.next) {
      fold(this.next, frame);
      if (frame.synced) {
        this.sections = this.next;
        this.next = null;
        this.synced = true;
      }
    } else {
      const sections = new Map(this.sections);
      fold(sections, frame);
      this.sections = sections;
      this.synced ||= frame.synced;
    }
    this.changed();
  }

  /**
   * The held inbox as one frame, for a tab that joins. While a new stream's
   * first inbox is under way, only what it has sent so far, unsynced: the
   * joining tab gets the rest and `synced` from the frames that follow.
   */
  snapshotFrame(): WatchInboxResponse {
    const sections = this.next ?? this.sections;
    return create(WatchInboxResponseSchema, {
      sections: [...sections.values()].map((section) =>
        create(InboxSectionChangeSchema, {
          repoId: section.repoId,
          items: [...section.items.values()],
          needsYouNowTotal: section.needsYouNowTotal,
          watchingTotal: section.watchingTotal,
          landed: section.landed,
          error: section.error,
        }),
      ),
      synced: this.next === null && this.synced,
    });
  }

  private changed() {
    this.version += 1;
    this.notifier.notify();
  }
}

export type OpenInbox = (signal: AbortSignal) => AsyncIterable<WatchInboxResponse>;

/**
 * Reads the stream until `signal` aborts or the session ends, watching
 * again after any error with the backoff.
 */
export async function runInbox(
  open: OpenInbox,
  store: InboxStore,
  hooks: Omit<StreamHooks, "onFrame"> & {
    onFrame?: (frame: WatchInboxResponse) => void;
    onResync?: () => void;
  },
): Promise<void> {
  const { signal, backoff } = hooks;
  // A call, not `signal.aborted`, which TypeScript would narrow across the awaits.
  const aborted = () => signal.aborted;
  while (!aborted()) {
    store.beginResync();
    hooks.onResync?.();
    try {
      for await (const frame of open(signal)) {
        backoff.reset();
        store.applyFrame(frame);
        hooks.onFrame?.(frame);
      }
      if (aborted()) return;
      hooks.onRetry?.(reefError(new Error("the inbox stream ended")));
    } catch (thrown) {
      if (aborted()) return;
      const error = reefError(thrown);
      if (isSessionInvalid(error)) {
        hooks.onSessionInvalid();
        return;
      }
      hooks.onRetry?.(error);
    }
    await hooks.sleep(backoff.next(), signal);
  }
}
