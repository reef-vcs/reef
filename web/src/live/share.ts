// One operation stream, one presence stream and one inbox stream (#372) per
// browser, shared by every tab of the origin (R2.18, ADR 0047). On loopback the browser speaks
// HTTP/1.1, where each open stream holds one of six connections per origin,
// so streams per tab would starve unary calls by the third tab.
//
// The tab holding the Web Lock `reef-live` leads: it opens the streams and
// republishes every frame, still encoded, on the `BroadcastChannel`
// `reef-live`, as it arrives (never on an animation frame, which a hidden
// leader does not get). Other tabs decode the same frames. A tab that joins
// says `hello`, and the leader answers with its presence set, its inbox and
// its operation stream's position. When the leader closes, the lock passes to
// another tab, which resumes from the last cursor it saw and resubscribes to
// presence and the inbox. Without `navigator.locks`, every tab leads alone.
//
// Streams that fall silent are cut and opened again (live/silence.ts), and
// the tab is not `live` until each has a frame. A follower that hears
// nothing from its leader asks it, and takes the lock over from a leader
// that does not answer: a browser may have suspended it.
import { fromBinary, toBinary } from "@bufbuild/protobuf";
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError } from "../api/errors";
import { DaemonService } from "../gen/reef/api/v1/daemon_pb";
import { InboxService, WatchInboxResponseSchema } from "../gen/reef/api/v1/inbox_pb";
import {
  OperationService,
  SubscribeOperationsResponseSchema,
} from "../gen/reef/api/v1/operations_pb";
import { PresenceService, SubscribePresenceResponseSchema } from "../gen/reef/api/v1/presence_pb";
import { after, Backoff, sleep, type Cancel } from "./backoff";
import { animationFrame, FrameNotifier, type FrameScheduler } from "./frame";
import { InboxStore, runInbox } from "./inbox";
import { OperationStore, positionFrame, runOperations } from "./operations";
import { PresenceStore, runPresence } from "./presence";
import {
  browserClock,
  onWake,
  SILENT_MS,
  SILENT_ON_WAKE_MS,
  Silence,
  Since,
  type Clock,
} from "./silence";

export const LIVE_NAME = "reef-live";

/** Operation-frame cursors a follower remembers, to drop repeats after a handover. */
const RECENT_CURSORS = 1024;

/** How long a follower waits for its leader's answer before it takes over. */
export const ANSWER_MS = 5_000;

export type ConnectionState = "connecting" | "live" | "reconnecting" | "ended";

type Message =
  | { t: "hello"; from: string }
  | {
      t: "welcome";
      to: string;
      operations: Uint8Array;
      presence: Uint8Array;
      inbox: Uint8Array;
      state: ConnectionState;
    }
  | { t: "operations"; frame: Uint8Array }
  | { t: "reset" }
  | { t: "resync" }
  | { t: "presence"; frame: Uint8Array }
  | { t: "inboxResync" }
  | { t: "inbox"; frame: Uint8Array }
  | { t: "state"; state: ConnectionState }
  | { t: "silent" }
  | { t: "ended" };

/** What `BroadcastChannel` provides, so tests can supply their own. */
export interface Channel {
  postMessage(message: unknown): void;
  addEventListener(type: "message", listener: (event: MessageEvent) => void): void;
  close(): void;
}

export interface LiveEnvironment {
  locks: Pick<LockManager, "request"> | undefined;
  channel: ((name: string) => Channel) | undefined;
  frame: FrameScheduler;
  sleep: (ms: number, signal: AbortSignal) => Promise<void>;
  after: (ms: number, run: () => void) => Cancel;
  clock: Clock;
  onWake: (run: () => void) => () => void;
}

export function browserEnvironment(): LiveEnvironment {
  return {
    locks: typeof navigator !== "undefined" ? navigator.locks : undefined,
    channel:
      typeof BroadcastChannel !== "undefined" ? (name) => new BroadcastChannel(name) : undefined,
    frame: animationFrame,
    sleep,
    after,
    clock: browserClock,
    onWake,
  };
}

export class Live {
  readonly operations: OperationStore;
  readonly presence: PresenceStore;
  readonly inbox: InboxStore;
  state: ConnectionState = "connecting";
  /** This tab holds the lock and the streams. */
  leading = false;
  private readonly stateNotifier: FrameNotifier;
  private readonly endedListeners = new Set<() => void>();
  private readonly stop = new AbortController();
  private channel: Channel | undefined;
  private releaseLock: (() => void) | undefined;
  /** Aborted when this tab stops leading: its streams end. */
  private lane = new AbortController();
  /** Aborted to leave the queue for the lock. */
  private queue = new AbortController();
  private readonly silence: Silence;
  /** When a follower last heard its leader. */
  private readonly heard: Since;
  /** How many messages of a leader's this follower has had. */
  private messages = 0;
  private cancelFollow: Cancel | undefined;
  private stopWaking: (() => void) | undefined;
  private operationsFailing = false;
  private presenceFailing = false;
  private inboxFailing = false;
  /**
   * A follower folds the leader's inbox frames only from its `welcome` or a
   * new stream's start: a frame before them is a change to an inbox it does
   * not hold yet, and the welcome already counts it.
   */
  private inboxJoined = false;
  private everLive = false;
  /** Names this tab's `hello`, so only it applies the answer. */
  private readonly id = Math.random().toString(36).slice(2);
  /**
   * Cursors of operation frames this follower has applied. When the lock
   * passes, the new leader resumes from the last cursor it saw, which can
   * be behind another follower's: frames that follower already has come
   * round again and are dropped here.
   */
  private readonly recentCursors = new Set<string>();

  constructor(
    private readonly connection: Connection,
    private readonly env: LiveEnvironment = browserEnvironment(),
  ) {
    this.operations = new OperationStore(env.frame);
    this.presence = new PresenceStore(env.frame);
    this.inbox = new InboxStore(env.frame);
    this.stateNotifier = new FrameNotifier(env.frame);
    this.silence = new Silence(env.clock, env.after);
    this.heard = new Since(env.clock);
  }

  start() {
    const { locks, channel } = this.env;
    this.stopWaking = this.env.onWake(() => {
      if (this.leading) this.silence.check(SILENT_ON_WAKE_MS);
      else this.checkLeader(SILENT_ON_WAKE_MS);
    });
    if (!locks || !channel) {
      this.lead();
      return;
    }
    this.channel = channel(LIVE_NAME);
    this.channel.addEventListener("message", (event) => {
      this.receive(event.data as Message);
    });
    this.follow(locks);
  }

  close() {
    this.halt();
    this.channel?.close();
  }

  /** Ends the streams, the timers and the claim to the lock. */
  private halt() {
    this.stop.abort();
    this.lane.abort();
    this.queue.abort();
    this.cancelFollow?.();
    this.stopWaking?.();
    this.releaseLock?.();
  }

  /** Waits for the lock, on what the leader sends until then. */
  private follow(locks: Pick<LockManager, "request">) {
    this.heard.now();
    this.post({ t: "hello", from: this.id });
    this.watchLeader();
    this.queue = new AbortController();
    this.claim(locks, { mode: "exclusive", signal: this.queue.signal });
  }

  private claim(locks: Pick<LockManager, "request">, options: LockOptions) {
    let held = false;
    locks
      .request(LIVE_NAME, options, () => {
        if (this.stop.signal.aborted) return;
        held = true;
        this.lead();
        // Held until this tab closes or `close` runs.
        return new Promise<void>((release) => {
          this.releaseLock = release;
        });
      })
      .catch(() => {
        // Aborted while waiting for the lock, or, once held, taken over by a
        // tab that this one did not answer.
        if (held && !this.stop.signal.aborted) this.stepDown(locks);
      });
  }

  private stepDown(locks: Pick<LockManager, "request">) {
    this.lane.abort();
    this.leading = false;
    this.inboxJoined = false;
    this.follow(locks);
  }

  private watchLeader() {
    this.cancelFollow?.();
    this.cancelFollow = this.env.after(Math.max(SILENT_MS - this.heard.elapsed(), 1000), () => {
      if (!this.checkLeader(SILENT_MS)) this.watchLeader();
    });
  }

  /**
   * Asks a leader silent for `limit` whether it is there, and takes over from
   * one that is not. Whether it asked.
   */
  private checkLeader(limit: number): boolean {
    const { locks } = this.env;
    if (this.leading || !locks || this.heard.elapsed() < limit) return false;
    const { messages } = this;
    this.channel?.postMessage({ t: "silent" } satisfies Message);
    this.cancelFollow?.();
    this.cancelFollow = this.env.after(ANSWER_MS, () => {
      if (this.leading || this.stop.signal.aborted) return;
      if (this.messages > messages) {
        this.watchLeader();
        return;
      }
      this.setState(this.everLive ? "reconnecting" : "connecting");
      this.queue.abort();
      this.claim(locks, { mode: "exclusive", steal: true });
    });
    return true;
  }

  subscribeState(listener: () => void): () => void {
    return this.stateNotifier.subscribe(listener);
  }

  /** Called once if the daemon refuses the session. */
  onEnded(listener: () => void): () => void {
    this.endedListeners.add(listener);
    return () => {
      this.endedListeners.delete(listener);
    };
  }

  private lead() {
    this.leading = true;
    this.cancelFollow?.();
    this.cancelFollow = undefined;
    this.lane = new AbortController();
    const { signal } = this.lane;
    // What a follower holds says nothing about the streams it opens now.
    this.operationsFailing = this.presenceFailing = this.inboxFailing = true;
    const operations = client(this.connection, OperationService);
    const presence = client(this.connection, PresenceService);
    const inbox = client(this.connection, InboxService);
    const shared = {
      signal,
      sleep: this.env.sleep,
      onSessionInvalid: () => {
        this.end();
      },
    };
    void runOperations(
      (cursor, signal) =>
        this.silence.watch(signal, (signal) =>
          operations.subscribeOperations(
            cursor === undefined ? {} : { start: { case: "cursor", value: cursor } },
            { signal },
          ),
        ),
      this.operations,
      {
        ...shared,
        backoff: new Backoff(),
        onFrame: (frame) => {
          this.operationsFailing = false;
          this.post({ t: "operations", frame: toBinary(SubscribeOperationsResponseSchema, frame) });
          this.updateState();
        },
        onRetry: () => {
          this.operationsFailing = true;
          this.updateState();
        },
      },
      () => {
        this.post({ t: "reset" });
      },
    );
    const watchPresence = (signal: AbortSignal) =>
      this.silence.watch(signal, (signal) => presence.subscribePresence({}, { signal }));
    void runPresence(watchPresence, this.presence, {
      ...shared,
      backoff: new Backoff(),
      onResync: () => {
        this.post({ t: "resync" });
        this.updateState();
      },
      onFrame: (frame) => {
        this.presenceFailing = false;
        this.post({ t: "presence", frame: toBinary(SubscribePresenceResponseSchema, frame) });
        this.updateState();
      },
      onRetry: () => {
        this.presenceFailing = true;
        this.updateState();
      },
    });
    const watchInbox = (signal: AbortSignal) =>
      this.silence.watch(signal, (signal) => inbox.watchInbox({}, { signal }));
    void runInbox(watchInbox, this.inbox, {
      ...shared,
      backoff: new Backoff(),
      onResync: () => {
        this.post({ t: "inboxResync" });
        this.updateState();
      },
      onFrame: (frame) => {
        this.inboxFailing = false;
        this.post({ t: "inbox", frame: toBinary(WatchInboxResponseSchema, frame) });
        this.updateState();
      },
      onRetry: () => {
        this.inboxFailing = true;
        this.updateState();
      },
    });
  }

  private receive(message: Message) {
    if (this.stop.signal.aborted) return;
    if (this.leading) {
      if (message.t === "hello") {
        this.post({
          t: "welcome",
          to: message.from,
          operations: toBinary(SubscribeOperationsResponseSchema, positionFrame(this.operations)),
          presence: toBinary(SubscribePresenceResponseSchema, this.presence.snapshotFrame()),
          inbox: toBinary(WatchInboxResponseSchema, this.inbox.snapshotFrame()),
          state: this.state,
        });
      } else if (message.t === "silent") {
        this.silence.check(SILENT_ON_WAKE_MS);
        this.post({ t: "state", state: this.state });
      }
      return;
    }
    // Another follower's question is no sign of the leader.
    if (message.t !== "hello" && message.t !== "silent") {
      this.heard.now();
      this.messages += 1;
    }
    switch (message.t) {
      case "hello":
      case "silent":
        return;
      case "welcome":
        // Another tab's answer: this tab already holds what it carries.
        if (message.to !== this.id) return;
        this.presence.beginResync();
        this.presence.applyFrame(fromBinary(SubscribePresenceResponseSchema, message.presence));
        this.inbox.beginResync();
        this.inbox.applyFrame(fromBinary(WatchInboxResponseSchema, message.inbox));
        this.inboxJoined = true;
        this.operations.applyFrame(
          fromBinary(SubscribeOperationsResponseSchema, message.operations),
        );
        this.setState(message.state);
        return;
      case "operations": {
        const frame = fromBinary(SubscribeOperationsResponseSchema, message.frame);
        if (frame.operations.length > 0 && frame.cursor) {
          if (this.recentCursors.has(frame.cursor)) return;
          this.recentCursors.add(frame.cursor);
          if (this.recentCursors.size > RECENT_CURSORS) {
            const oldest = this.recentCursors.values().next().value;
            if (oldest !== undefined) this.recentCursors.delete(oldest);
          }
        }
        this.operations.applyFrame(frame);
        return;
      }
      case "reset":
        this.operations.reset();
        return;
      case "resync":
        this.presence.beginResync();
        return;
      case "presence":
        this.presence.applyFrame(fromBinary(SubscribePresenceResponseSchema, message.frame));
        return;
      case "inboxResync":
        this.inbox.beginResync();
        this.inboxJoined = true;
        return;
      case "inbox":
        if (this.inboxJoined) {
          this.inbox.applyFrame(fromBinary(WatchInboxResponseSchema, message.frame));
        }
        return;
      case "state":
        this.setState(message.state);
        return;
      case "ended":
        // The leader's token was refused. This tab's may be another, newer
        // one (a tab `reef ui` opened since): end only if the daemon
        // refuses it too. Otherwise the lock passes here and this tab leads.
        void this.checkSession();
        return;
    }
  }

  private async checkSession() {
    try {
      await client(this.connection, DaemonService).getDaemonStatus(
        {},
        { signal: this.stop.signal },
      );
    } catch (thrown) {
      if (isSessionInvalid(reefError(thrown))) this.end();
    }
  }

  private post(message: Message) {
    if (this.leading && !this.stop.signal.aborted) this.channel?.postMessage(message);
    else if (message.t === "hello") this.channel?.postMessage(message);
  }

  private updateState() {
    if (this.state === "ended") return;
    const live =
      this.operations.live &&
      this.presence.synced &&
      this.inbox.synced &&
      !this.operationsFailing &&
      !this.presenceFailing &&
      !this.inboxFailing;
    this.setState(live ? "live" : this.everLive ? "reconnecting" : "connecting");
  }

  private setState(state: ConnectionState) {
    if (this.state === state || this.state === "ended") return;
    if (state === "live") this.everLive = true;
    this.state = state;
    if (this.leading) this.post({ t: "state", state });
    this.stateNotifier.notify();
  }

  private end() {
    if (this.state === "ended") return;
    this.post({ t: "ended" });
    this.state = "ended";
    this.stateNotifier.notify();
    this.halt();
    for (const listener of [...this.endedListeners]) listener();
  }
}
