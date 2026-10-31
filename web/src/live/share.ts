// One operation stream and one presence stream per browser, shared by every
// tab of the origin (R2.18, ADR 0047). On loopback the browser speaks
// HTTP/1.1, where each open stream holds one of six connections per origin,
// so streams per tab would starve unary calls by the third tab.
//
// The tab holding the Web Lock `reef-live` leads: it opens both streams and
// republishes every frame, still encoded, on the `BroadcastChannel`
// `reef-live`, as it arrives (never on an animation frame, which a hidden
// leader does not get). Other tabs decode the same frames. A tab that joins
// says `hello`, and the leader answers with its presence set and its
// operation stream's position. When the leader closes, the lock passes to
// another tab, which resumes from the last cursor it saw and resubscribes to
// presence. Without `navigator.locks`, every tab leads alone.
import { fromBinary, toBinary } from "@bufbuild/protobuf";
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError } from "../api/errors";
import { SubscribeOperationsResponseSchema } from "../gen/reef/api/v1/operations_pb";
import { SubscribePresenceResponseSchema } from "../gen/reef/api/v1/presence_pb";
import { Backoff, sleep } from "./backoff";
import { animationFrame, FrameNotifier, type FrameScheduler } from "./frame";
import { OperationStore, positionFrame, runOperations } from "./operations";
import { PresenceStore, runPresence } from "./presence";

export const LIVE_NAME = "reef-live";

/** Operation-frame cursors a follower remembers, to drop repeats after a handover. */
const RECENT_CURSORS = 1024;

export type ConnectionState = "connecting" | "live" | "reconnecting" | "ended";

type Message =
  | { t: "hello"; from: string }
  | {
      t: "welcome";
      to: string;
      operations: Uint8Array;
      presence: Uint8Array;
      state: ConnectionState;
    }
  | { t: "operations"; frame: Uint8Array }
  | { t: "reset" }
  | { t: "resync" }
  | { t: "presence"; frame: Uint8Array }
  | { t: "state"; state: ConnectionState }
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
}

export function browserEnvironment(): LiveEnvironment {
  return {
    locks: typeof navigator !== "undefined" ? navigator.locks : undefined,
    channel:
      typeof BroadcastChannel !== "undefined" ? (name) => new BroadcastChannel(name) : undefined,
    frame: animationFrame,
    sleep,
  };
}

export class Live {
  readonly operations: OperationStore;
  readonly presence: PresenceStore;
  state: ConnectionState = "connecting";
  /** This tab holds the lock and the streams. */
  leading = false;
  private readonly stateNotifier: FrameNotifier;
  private readonly endedListeners = new Set<() => void>();
  private readonly stop = new AbortController();
  private channel: Channel | undefined;
  private releaseLock: (() => void) | undefined;
  private operationsFailing = false;
  private presenceFailing = false;
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
    this.stateNotifier = new FrameNotifier(env.frame);
  }

  start() {
    const { locks, channel } = this.env;
    if (!locks || !channel) {
      this.lead();
      return;
    }
    this.channel = channel(LIVE_NAME);
    this.channel.addEventListener("message", (event) => {
      this.receive(event.data as Message);
    });
    this.post({ t: "hello", from: this.id });
    locks
      .request(LIVE_NAME, { mode: "exclusive", signal: this.stop.signal }, () => {
        if (this.stop.signal.aborted) return;
        this.lead();
        // Held until this tab closes or `close` runs.
        return new Promise<void>((release) => {
          this.releaseLock = release;
        });
      })
      .catch(() => {
        // Aborted by `close` while waiting for the lock.
      });
  }

  close() {
    this.stop.abort();
    this.releaseLock?.();
    this.channel?.close();
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
    const { signal } = this.stop;
    const { operations, presence } = this.connection.clients;
    const shared = {
      signal,
      sleep: this.env.sleep,
      onSessionInvalid: () => {
        this.end();
      },
    };
    void runOperations(
      (cursor, signal) =>
        operations.subscribeOperations(
          cursor === undefined ? {} : { start: { case: "cursor", value: cursor } },
          { signal },
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
    void runPresence((signal) => presence.subscribePresence({}, { signal }), this.presence, {
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
          state: this.state,
        });
      }
      return;
    }
    switch (message.t) {
      case "hello":
        return;
      case "welcome":
        // Another tab's answer: this tab already holds what it carries.
        if (message.to !== this.id) return;
        this.presence.beginResync();
        this.presence.applyFrame(fromBinary(SubscribePresenceResponseSchema, message.presence));
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
      await this.connection.clients.daemon.getDaemonStatus({}, { signal: this.stop.signal });
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
      !this.operationsFailing &&
      !this.presenceFailing;
    if (live) this.everLive = true;
    this.setState(live ? "live" : this.everLive ? "reconnecting" : "connecting");
  }

  private setState(state: ConnectionState) {
    if (this.state === state || this.state === "ended") return;
    this.state = state;
    if (this.leading) this.post({ t: "state", state });
    this.stateNotifier.notify();
  }

  private end() {
    if (this.state === "ended") return;
    this.post({ t: "ended" });
    this.state = "ended";
    this.stateNotifier.notify();
    this.stop.abort();
    this.releaseLock?.();
    for (const listener of [...this.endedListeners]) listener();
  }
}
