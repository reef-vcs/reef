// Test doubles for the stream layer: animation frames run by hand, streams
// fed by hand, and Web Locks and BroadcastChannel in one process.
import { create, type DescMethod, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code, ConnectError, type Transport } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import { DaemonService } from "../gen/reef/api/v1/daemon_pb";
import {
  InboxService,
  WatchInboxResponseSchema,
  type WatchInboxResponse,
} from "../gen/reef/api/v1/inbox_pb";
import { ErrorDetailSchema } from "../gen/reef/api/v1/error_pb";
import {
  OperationSchema,
  OperationService,
  SubscribeOperationsResponseSchema,
  type OperationKind,
  type SnapshotKind,
  type Operation,
  type SubscribeOperationsRequest,
  type SubscribeOperationsResponse,
} from "../gen/reef/api/v1/operations_pb";
import {
  PresenceEventSchema,
  PresenceSchema,
  type PresenceEndReason,
  PresenceService,
  SubscribePresenceResponseSchema,
  type SubscribePresenceResponse,
} from "../gen/reef/api/v1/presence_pb";
import type { FrameScheduler } from "../live/frame";
import type { Channel, LiveEnvironment } from "../live/share";

export function manualFrames(): {
  frame: FrameScheduler;
  flush: () => void;
  pending: () => number;
} {
  let queue: (() => void)[] = [];
  return {
    frame: (run) => {
      queue.push(run);
    },
    flush: () => {
      const runs = queue;
      queue = [];
      for (const run of runs) run();
    },
    pending: () => queue.length,
  };
}

/** Runs a frame's work at once. */
export const immediateFrames: FrameScheduler = (run) => {
  run();
};

/** An async iterable the test pushes values and errors into. */
export class Pushable<T> implements AsyncIterable<T> {
  private values: T[] = [];
  private error: Error | undefined;
  private done = false;
  private wake: (() => void) | undefined;

  constructor(signal?: AbortSignal) {
    signal?.addEventListener("abort", () => {
      this.fail(new ConnectError("aborted", Code.Canceled));
    });
  }

  push(...values: T[]) {
    this.values.push(...values);
    this.wake?.();
  }

  fail(error: Error) {
    this.error = error;
    this.wake?.();
  }

  end() {
    this.done = true;
    this.wake?.();
  }

  async *[Symbol.asyncIterator](): AsyncIterator<T> {
    for (;;) {
      const value = this.values.shift();
      if (value !== undefined) {
        yield value;
        continue;
      }
      if (this.error !== undefined) throw this.error;
      if (this.done) return;
      await new Promise<void>((resolve) => {
        this.wake = resolve;
      });
      this.wake = undefined;
    }
  }
}

/** Lets pending promise callbacks run. */
export async function settle(times = 10) {
  for (let i = 0; i < times; i++) await Promise.resolve();
}

export function operation(
  id: string,
  repoId: string,
  endedMs: number,
  init: {
    kind?: OperationKind;
    snapshotKind?: SnapshotKind;
    agent?: string;
    workspaceId?: string;
    description?: string;
  } = {},
): Operation {
  return create(OperationSchema, { id, repoId, endedAt: timestampFromMs(endedMs), ...init });
}

export function operationsFrame(
  init: MessageInitShape<typeof SubscribeOperationsResponseSchema>,
): SubscribeOperationsResponse {
  return create(SubscribeOperationsResponseSchema, init);
}

export function presenceFrame(
  entries: (MessageInitShape<typeof PresenceSchema> & {
    id: string;
    ended?: boolean;
    endReason?: PresenceEndReason;
  })[],
  synced = false,
): SubscribePresenceResponse {
  return create(SubscribePresenceResponseSchema, {
    synced,
    events: entries.map(({ ended = false, endReason, ...presence }) =>
      create(PresenceEventSchema, {
        ended,
        ...(endReason === undefined ? {} : { endReason }),
        presence: create(PresenceSchema, presence),
      }),
    ),
  });
}

/** A ConnectError carrying an `ErrorDetail`, as the daemon sends them. */
export function daemonError(
  code: Code,
  detail: MessageInitShape<typeof ErrorDetailSchema>,
  message = "refused",
): ConnectError {
  return new ConnectError(message, code, undefined, [{ desc: ErrorDetailSchema, value: detail }]);
}

export const sessionInvalid = () =>
  daemonError(Code.PermissionDenied, { reason: "WEB_SESSION_INVALID", next: ["reef ui"] });

/** In-memory methods, by service type name and then method name. */
export type FakeServices = Record<
  string,
  Record<string, (request: never, options: { signal: AbortSignal | undefined }) => unknown>
>;

/**
 * A transport that calls `services` in this process, without serializing, so
 * a stream the app reads is the very object the test feeds.
 */
export function fakeTransport(services: FakeServices): Transport {
  const fake = (method: DescMethod) => {
    const call = services[method.parent.typeName]?.[method.localName];
    if (!call)
      throw new ConnectError(`${method.parent.typeName}/${method.name}`, Code.Unimplemented);
    return call;
  };
  const response = (method: DescMethod, message: unknown) => ({
    service: method.parent,
    method,
    header: new Headers(),
    trailer: new Headers(),
    message,
  });
  return {
    unary: async (method, signal, _timeoutMs, _header, input) =>
      ({
        stream: false,
        ...response(method, await fake(method)(input as never, { signal })),
      }) as never,
    stream: async (method, signal, _timeoutMs, _header, input) => {
      const first = await input[Symbol.asyncIterator]().next();
      return {
        stream: true,
        ...response(method, fake(method)(first.value as never, { signal })),
      } as never;
    },
  };
}

/** A daemon whose streams the test feeds: each subscribe opens a new Pushable. */
export class FakeStreams {
  readonly operationCalls: {
    request: Partial<SubscribeOperationsRequest>;
    stream: Pushable<SubscribeOperationsResponse>;
  }[] = [];
  readonly presenceCalls: Pushable<SubscribePresenceResponse>[] = [];
  readonly inboxCalls: Pushable<WatchInboxResponse>[] = [];

  subscribeOperations = (
    request: Partial<SubscribeOperationsRequest>,
    options: { signal: AbortSignal | undefined },
  ): Pushable<SubscribeOperationsResponse> => {
    const stream = new Pushable<SubscribeOperationsResponse>(options.signal);
    this.operationCalls.push({ request, stream });
    return stream;
  };

  subscribePresence = (
    _request: unknown,
    options: { signal: AbortSignal | undefined },
  ): Pushable<SubscribePresenceResponse> => {
    const stream = new Pushable<SubscribePresenceResponse>(options.signal);
    this.presenceCalls.push(stream);
    return stream;
  };

  /** Each stream starts with an empty inbox, synced, as a daemon with nothing in it sends. */
  watchInbox = (
    _request: unknown,
    options: { signal: AbortSignal | undefined },
  ): Pushable<WatchInboxResponse> => {
    const stream = new Pushable<WatchInboxResponse>(options.signal);
    stream.push(create(WatchInboxResponseSchema, { synced: true }));
    this.inboxCalls.push(stream);
    return stream;
  };

  /**
   * A connection with this daemon's streams, and `services` beside or over
   * them. `refused` says whether the daemon refuses this connection's token,
   * for `GetDaemonStatus`.
   */
  connection({
    refused = () => false,
    services = {},
  }: { refused?: () => boolean; services?: FakeServices } = {}): Connection {
    const own: FakeServices = {
      [DaemonService.typeName]: {
        getDaemonStatus: async () => {
          await settle(1);
          if (refused()) throw sessionInvalid();
          return {};
        },
      },
      [OperationService.typeName]: { subscribeOperations: this.subscribeOperations },
      [PresenceService.typeName]: { subscribePresence: this.subscribePresence },
      [InboxService.typeName]: { watchInbox: this.watchInbox },
    };
    for (const [name, methods] of Object.entries(services))
      own[name] = { ...own[name], ...methods };
    return { transport: fakeTransport(own), label: "fake" };
  }

  lastOperations(): Pushable<SubscribeOperationsResponse> {
    const call = this.operationCalls.at(-1);
    if (!call) throw new Error("no operation stream is open");
    return call.stream;
  }

  lastInbox(): Pushable<WatchInboxResponse> {
    const stream = this.inboxCalls.at(-1);
    if (!stream) throw new Error("no inbox stream is open");
    return stream;
  }

  lastPresence(): Pushable<SubscribePresenceResponse> {
    const stream = this.presenceCalls.at(-1);
    if (!stream) throw new Error("no presence stream is open");
    return stream;
  }
}

/** `navigator.locks` for one exclusive lock name, in one process. */
export class FakeLocks {
  private held = false;
  private steal: (() => void) | undefined;
  private readonly waiting: (() => void)[] = [];

  request(
    _name: string,
    options: LockOptions,
    callback: (lock: Lock | null) => unknown,
  ): Promise<unknown> {
    return new Promise((resolve, reject) => {
      const grant = () => {
        let stolen = false;
        this.held = true;
        this.steal = () => {
          stolen = true;
          reject(new DOMException("stolen", "AbortError"));
        };
        void Promise.resolve(callback(null))
          .then(resolve, reject)
          .finally(() => {
            if (stolen) return;
            this.held = false;
            this.waiting.shift()?.();
          });
      };
      if (options.steal) {
        this.steal?.();
        grant();
        return;
      }
      options.signal?.addEventListener("abort", () => {
        const index = this.waiting.indexOf(grant);
        if (index >= 0) {
          this.waiting.splice(index, 1);
          reject(new DOMException("aborted", "AbortError"));
        }
      });
      if (this.held) this.waiting.push(grant);
      else grant();
    });
  }
}

/** BroadcastChannel between tabs in one process: delivers to every other member, later. */
export class FakeBus {
  private readonly members = new Set<{ deliver: (data: unknown) => void }>();
  private readonly frozen = new Set<object>();

  /**
   * Suspends the `nth` tab to join: it says nothing, and hears nothing until
   * the function returned runs, which delivers it all.
   */
  freeze(nth: number): () => void {
    const member = [...this.members][nth];
    if (!member) throw new Error("no such tab has joined");
    const { deliver } = member;
    const missed: unknown[] = [];
    this.frozen.add(member);
    member.deliver = (data) => missed.push(data);
    return () => {
      this.frozen.delete(member);
      member.deliver = deliver;
      for (const data of missed) deliver(data);
    };
  }

  channel = (): Channel => {
    const listeners: ((event: MessageEvent) => void)[] = [];
    const member = {
      deliver: (data: unknown) => {
        for (const listener of listeners) listener({ data } as MessageEvent);
      },
    };
    this.members.add(member);
    return {
      postMessage: (message: unknown) => {
        if (this.frozen.has(member)) return;
        const copy = structuredClone(message);
        for (const other of this.members) {
          if (other !== member)
            queueMicrotask(() => {
              other.deliver(copy);
            });
        }
      },
      addEventListener: (_type, listener) => {
        listeners.push(listener);
      },
      close: () => {
        this.members.delete(member);
      },
    };
  };
}

/** Two clocks and the timers on them, moved by hand. */
export class FakeTime {
  private at = { mono: 1_000, wall: 1_795_000_000_000 };
  private timers: { due: number; run: () => void }[] = [];
  private readonly wakers = new Set<() => void>();

  readonly parts: Pick<LiveEnvironment, "after" | "clock" | "onWake"> = {
    clock: { mono: () => this.at.mono, wall: () => this.at.wall },
    after: (ms, run) => {
      const timer = { due: this.at.mono + ms, run };
      this.timers.push(timer);
      return () => {
        this.timers = this.timers.filter((other) => other !== timer);
      };
    },
    onWake: (run) => {
      this.wakers.add(run);
      return () => this.wakers.delete(run);
    },
  };

  /** Runs the timers due in `ms`, in order, each at its time. */
  async pass(ms: number) {
    const end = this.at.mono + ms;
    for (;;) {
      const next = this.timers.filter((t) => t.due <= end).sort((a, b) => a.due - b.due)[0];
      if (!next) break;
      this.step(next.due - this.at.mono);
      this.timers = this.timers.filter((other) => other !== next);
      next.run();
      await settle(30);
    }
    this.step(end - this.at.mono);
    await settle(30);
  }

  /** The device sleeps: no timer runs, and only the wall clock moves. */
  sleep(ms: number) {
    this.at.wall += ms;
  }

  /** Someone sets the clock. */
  set(wall: number) {
    this.at.wall = wall;
  }

  async wake() {
    for (const run of this.wakers) run();
    await settle(30);
  }

  private step(ms: number) {
    this.at.mono += ms;
    this.at.wall += ms;
  }
}

export function environment(
  frame: FrameScheduler,
  shared?: { locks: FakeLocks; bus: FakeBus },
  time?: FakeTime,
): LiveEnvironment {
  return {
    locks: shared?.locks as LiveEnvironment["locks"],
    channel: shared?.bus.channel,
    frame,
    // Retries happen at once in tests unless a test controls time itself.
    sleep: async () => {
      await settle(1);
    },
    // Nothing falls silent in tests unless a test moves time itself.
    after: () => () => undefined,
    clock: { mono: () => 0, wall: () => 0 },
    onWake: () => () => undefined,
    ...time?.parts,
  };
}
