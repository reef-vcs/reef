// The presence stream: state, not history (R2.10). The daemon sends the
// current set and then changes, with `synced` on the frame that completes
// the set, and no cursor: a new subscription's set replaces what was held.
// Entries that end are kept as endings, with why, so a view can tell
// "finished" from "went silent" (ADR 0037, decision 15; R2.20).
import { create } from "@bufbuild/protobuf";
import {
  PresenceEndReason,
  PresenceEventSchema,
  SubscribePresenceResponseSchema,
  type Presence,
  type SubscribePresenceResponse,
} from "../gen/reef/api/v1/presence_pb";
import { isSessionInvalid, reefError } from "../api/errors";
import { FrameNotifier, type FrameScheduler } from "./frame";
import type { StreamHooks } from "./operations";

/** Endings held at most, oldest dropped first. */
export const ENDINGS_LIMIT = 1024;

/** An entry that ended while this tab watched. */
export interface Ending {
  /** Its last state. */
  presence: Presence;
  /** `UNSPECIFIED` when a resubscription's set dropped it. */
  reason: PresenceEndReason;
  /** When this tab learned of it, in milliseconds since the epoch. */
  at: number;
}

export class PresenceStore {
  readonly entries = new Map<string, Presence>();
  /**
   * Entries that ended, by id, oldest first. Per tab: one that joins later
   * gets the leader's live set and only the endings it sees from then on.
   */
  readonly endings = new Map<string, Ending>();
  /** The held set is complete: the current subscription has sent `synced`. */
  synced = false;
  /** Increases with every change, for `useSyncExternalStore`. */
  version = 0;
  /** Increases with every new subscription, whose set replaces the held one. */
  resyncs = 0;
  /** Ids the current subscription has sent, until its set is synced. */
  private seen: Set<string> | null = null;
  private readonly notifier: FrameNotifier;

  constructor(
    frame: FrameScheduler,
    private readonly now: () => number = Date.now,
  ) {
    this.notifier = new FrameNotifier(frame);
  }

  subscribe(listener: () => void): () => void {
    return this.notifier.subscribe(listener);
  }

  /** A new subscription starts: its set will replace the held one. */
  beginResync() {
    this.seen = new Set();
    this.synced = false;
    this.resyncs += 1;
    this.changed();
  }

  applyFrame(frame: SubscribePresenceResponse) {
    for (const event of frame.events) {
      const presence = event.presence;
      if (!presence) continue;
      if (event.ended) {
        this.entries.delete(presence.id);
        this.seen?.delete(presence.id);
        this.ended(presence, event.endReason);
      } else {
        this.entries.set(presence.id, presence);
        this.seen?.add(presence.id);
        this.endings.delete(presence.id);
      }
    }
    if (frame.synced && !this.synced) {
      const seen = this.seen;
      if (seen) {
        for (const [id, presence] of [...this.entries]) {
          if (seen.has(id)) continue;
          this.entries.delete(id);
          this.ended(presence, PresenceEndReason.UNSPECIFIED);
        }
      }
      this.seen = null;
      this.synced = true;
    }
    this.changed();
  }

  /**
   * The held set as one frame, for a tab that joins. While a resubscription
   * is under way, only the entries it has sent so far, unsynced: the rest
   * may be about to end, and the joining tab gets the remainder and `synced`
   * from the frames that follow.
   */
  snapshotFrame(): SubscribePresenceResponse {
    const seen = this.seen;
    const entries = [...this.entries.values()].filter((p) => !seen || seen.has(p.id));
    return create(SubscribePresenceResponseSchema, {
      events: entries.map((presence) => create(PresenceEventSchema, { presence })),
      synced: this.synced,
    });
  }

  private ended(presence: Presence, reason: PresenceEndReason) {
    // Re-inserted, so the map stays oldest first.
    this.endings.delete(presence.id);
    this.endings.set(presence.id, { presence, reason, at: this.now() });
    if (this.endings.size > ENDINGS_LIMIT) {
      const oldest = this.endings.keys().next();
      if (!oldest.done) this.endings.delete(oldest.value);
    }
  }

  private changed() {
    this.version += 1;
    this.notifier.notify();
  }
}

export type OpenPresence = (signal: AbortSignal) => AsyncIterable<SubscribePresenceResponse>;

/**
 * Reads the stream until `signal` aborts or the session ends, resubscribing
 * after any error with the backoff.
 */
export async function runPresence(
  open: OpenPresence,
  store: PresenceStore,
  hooks: Omit<StreamHooks, "onFrame"> & {
    onFrame?: (frame: SubscribePresenceResponse) => void;
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
      hooks.onRetry?.(reefError(new Error("the presence stream ended")));
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
