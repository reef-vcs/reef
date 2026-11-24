// Reads over a connection that dies (R3.40), on the page a phone opens:
// what the connection failed is sent again, what is silent is cut, a slow
// answer is not, and a write or the daemon's own refusal passes at once.
import { create, toBinary } from "@bufbuild/protobuf";
import { Code } from "@connectrpc/connect";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { ChangeService } from "../gen/reef/api/v1/changes_pb";
import { DaemonService, GetDaemonStatusResponseSchema } from "../gen/reef/api/v1/daemon_pb";
import { InboxService } from "../gen/reef/api/v1/inbox_pb";
import { MAX_DOUBLINGS, SILENT_MS } from "../live/silence";
import { settle } from "../test/fakes";
import { browserConnection, client } from "./connection";
import { reefError } from "./errors";
import { MOST, TRIES } from "./steady";
import { streamsOf } from "./streams";

const SECOND = 1_000;

/** One gRPC-web envelope. */
function envelope(flags: number, data: Uint8Array): Uint8Array<ArrayBuffer> {
  const out = new Uint8Array(5 + data.length);
  out[0] = flags;
  new DataView(out.buffer).setUint32(1, data.length);
  out.set(data, 5);
  return out;
}

const ANSWER = Uint8Array.from([
  ...envelope(
    0,
    toBinary(
      GetDaemonStatusResponseSchema,
      create(GetDaemonStatusResponseSchema, { version: "0.4.2" }),
    ),
  ),
  ...envelope(0x80, new TextEncoder().encode("grpc-status: 0\r\n")),
]);

/** A response whose body the test sends. */
function opened() {
  let body: ReadableStreamDefaultController<Uint8Array> | undefined;
  let at = 0;
  const response = new Response(
    new ReadableStream<Uint8Array>({
      start(controller) {
        body = controller;
      },
    }),
    { headers: { "content-type": "application/grpc-web+proto" } },
  );
  return {
    response,
    byte: () => body?.enqueue(ANSWER.slice(at, ++at)),
    rest: () => {
      body?.enqueue(ANSWER.slice(at));
      body?.close();
    },
    fail: () => {
      body?.error(new TypeError("Load failed"));
    },
  };
}

function answer(): Response {
  const { response, rest } = opened();
  rest();
  return response;
}

const refusal = (code: Code, detail = "") =>
  new Response(null, {
    headers: {
      "content-type": "application/grpc-web+proto",
      "grpc-status": String(code),
      "grpc-message": "no",
      ...(detail ? { "grpc-status-details-bin": detail } : {}),
    },
  });

/** The page over `remote`, and what each `fetch` of it does. */
function page(remote = true) {
  const connection = browserConnection("token", "https://reef.example", remote);
  const sent: { at: number; signal: AbortSignal; answer: (response: Response) => void }[] = [];
  const started = Date.now();
  const plan: (() => Promise<Response>)[] = [];
  vi.stubGlobal("fetch", (_: unknown, init: RequestInit) => {
    const signal = init.signal ?? new AbortController().signal;
    const next = plan.shift();
    return new Promise<Response>((resolve, reject) => {
      sent.push({ at: Date.now() - started, signal, answer: resolve });
      signal.addEventListener("abort", () => {
        reject(new DOMException("aborted", "AbortError"));
      });
      next?.().then(resolve, reject);
    });
  });
  const streams = streamsOf(connection);
  streams.change(true);
  const held = { listeners: 0 };
  const { onResume } = streams;
  streams.onResume = (run) => {
    held.listeners += 1;
    const stop = onResume(run);
    return () => {
      held.listeners -= 1;
      stop();
    };
  };
  return {
    streams,
    held,
    sent,
    at: () => sent.map(({ at }) => at),
    plan: (...steps: (() => Promise<Response>)[]) => plan.push(...steps),
    /** Reads; how it ended, once it has. */
    read: (signal?: AbortSignal) => {
      const ended: { version?: string; code?: Code } = {};
      client(connection, DaemonService)
        .getDaemonStatus({}, signal ? { signal } : {})
        .then(
          (response) => (ended.version = response.version),
          (thrown: unknown) => (ended.code = reefError(thrown).code),
        );
      return ended;
    },
    connection,
  };
}

const dead = () => Promise.reject(new TypeError("Load failed"));
const alive = () => Promise.resolve(answer());

async function pass(ms: number) {
  await vi.advanceTimersByTimeAsync(ms);
  await flush();
}

/** Streams hand their chunks over in tasks that fake timers do not hold. */
async function flush() {
  for (let i = 0; i < 5; i++) await new Promise((resolve) => setImmediate(resolve));
  await settle(30);
}

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
});

afterEach(() => {
  vi.useRealTimers();
  vi.unstubAllGlobals();
});

describe("a read the connection failed", () => {
  it("is sent again until it is answered, and holds nothing then", async () => {
    const { plan, read, at, held } = page();
    plan(dead, dead, alive);
    const ended = read();
    await pass(10 * SECOND);
    expect(ended).toEqual({ version: "0.4.2" });
    expect(at()).toEqual([0, 100, 300]);
    expect(vi.getTimerCount()).toBe(0);
    expect(held.listeners).toBe(0);
  });

  it("is sent again when its answer breaks off, or something between answers", async () => {
    const { plan, read, sent } = page();
    const broken = opened();
    plan(
      () => Promise.resolve(broken.response),
      () => Promise.resolve(new Response("down", { status: 502 })),
      alive,
    );
    const ended = read();
    await flush();
    broken.byte();
    broken.fail();
    await flush();
    await pass(SECOND);
    expect(ended).toEqual({ version: "0.4.2" });
    expect(sent).toHaveLength(3);
  });

  it(`is the page's error after ${String(TRIES)} tries, while the streams are live`, async () => {
    const { read, plan, sent } = page();
    plan(...Array.from({ length: 10 }, () => dead));
    const ended = read();
    await pass(60 * SECOND);
    expect(ended).toEqual({ code: Code.Unknown });
    expect(sent).toHaveLength(TRIES);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("is sent every 2 s for as long as the streams are lost, and at once when they are back", async () => {
    const { read, plan, sent, streams } = page();
    streams.change(false);
    plan(...Array.from({ length: 34 }, () => dead), alive);
    const ended = read();
    await pass(60 * SECOND);
    expect(sent).toHaveLength(34);
    streams.change(true);
    await flush();
    expect(ended).toEqual({ version: "0.4.2" });
    expect(sent).toHaveLength(35);
  });

  it("has its tries again once the streams are back", async () => {
    const { read, plan, sent, streams } = page();
    plan(...Array.from({ length: TRIES - 1 }, () => dead));
    const ended = read();
    await pass(10 * SECOND);
    expect(sent).toHaveLength(TRIES);
    streams.change(false);
    plan(dead, dead, alive);
    streams.change(true);
    await flush();
    await pass(10 * SECOND);
    expect(ended).toEqual({ version: "0.4.2" });
  });

  it("is not sent again once the page wants it no more", async () => {
    const { read, sent, plan } = page();
    const left = new AbortController();
    plan(dead);
    const ended = read(left.signal);
    await flush();
    left.abort();
    await pass(60 * SECOND);
    expect(ended.code).toBeDefined();
    expect(sent).toHaveLength(1);
    expect(vi.getTimerCount()).toBe(0);
  });
});

describe("a read that hangs", () => {
  it("is not sent for a page that wants it no more already", async () => {
    const { read, sent, held } = page();
    const ended = read(AbortSignal.abort());
    await flush();
    expect(sent.filter(({ signal }) => !signal.aborted)).toEqual([]);
    await pass(60 * SECOND);
    expect(ended.code).toBeDefined();
    expect(held.listeners).toBe(0);
  });

  it.each([
    ["live", true],
    ["lost", false],
  ])(
    "is held with 31 others at most, the streams %s, the oldest failing for one more",
    async (_, live) => {
      const { read, sent, held, streams } = page();
      streams.change(live);
      const ends = Array.from({ length: MOST + 10 }, () => read());
      await flush();
      expect(ends.map(({ code }) => code !== undefined)).toEqual([
        ...Array.from({ length: 10 }, () => true),
        ...Array.from({ length: MOST }, () => false),
      ]);
      expect(sent.filter(({ signal }) => !signal.aborted)).toHaveLength(MOST);
      for (const { answer: reply } of sent) reply(answer());
      await flush();
      expect(ends.filter(({ version }) => version)).toHaveLength(MOST);
      expect(vi.getTimerCount()).toBe(0);
      expect(held.listeners).toBe(0);
      // What was answered, refused or left is held no more.
      const left = new AbortController();
      for (let i = 0; i < MOST; i++) read(left.signal);
      await flush();
      left.abort();
      await flush();
      const more = Array.from({ length: MOST }, () => read());
      await flush();
      expect(more.map(({ code }) => code)).toEqual(Array.from({ length: MOST }, () => undefined));
    },
  );

  it.each([
    ["answered", () => Promise.resolve(answer())],
    ["refused", () => Promise.resolve(refusal(Code.NotFound))],
  ])("is not failed for the many that were %s since", async (_, reply) => {
    const { read, plan } = page();
    plan(() => new Promise(() => {}));
    const held = read();
    await flush();
    plan(...Array.from({ length: 2 * MOST }, () => reply));
    for (let i = 0; i < 2 * MOST; i++) {
      read();
      await flush();
    }
    expect(held).toEqual({});
  });

  it("ends with the page that wants it", async () => {
    const { read, sent, held } = page();
    const left = new AbortController();
    const ended = read(left.signal);
    await flush();
    left.abort();
    await flush();
    expect(ended.code).toBeDefined();
    expect(sent.map(({ signal }) => signal.aborted)).toEqual([true]);
    await pass(60 * SECOND);
    expect(sent).toHaveLength(1);
    expect([vi.getTimerCount(), held.listeners]).toEqual([0, 0]);
  });

  it("is cut after 40 s, and given twice as long each time, up to 320 s", async () => {
    const { read, at, sent, streams } = page();
    streams.change(false);
    read();
    const limits = [40, 80, 160, 320, 320].map((s) => s * SECOND);
    const starts: number[] = [];
    for (let n = 0, start = 0; n < limits.length; n++) {
      starts.push(start);
      start += limits[n] ?? 0;
    }
    starts.push(920 * SECOND);
    await pass(921 * SECOND);
    expect(at()).toEqual(starts);
    expect(sent.slice(0, -1).every(({ signal }) => signal.aborted)).toBe(true);
    expect(SILENT_MS * 2 ** MAX_DOUBLINGS).toBe(320 * SECOND);
  });

  it("is sent again at once when the streams are back, whenever it was sent", async () => {
    const { read, sent, streams } = page();
    const before = read();
    await pass(10 * SECOND);
    streams.change(false);
    const during = read();
    streams.change(true);
    await flush();
    expect(sent.map(({ signal }) => signal.aborted)).toEqual([true, true, false, false]);
    for (const { answer: reply } of sent.slice(2)) reply(answer());
    await flush();
    expect([before, during]).toEqual([{ version: "0.4.2" }, { version: "0.4.2" }]);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("is left alone when the streams open for the first time", async () => {
    const connection = browserConnection("token", "https://reef.example", true);
    const signals: AbortSignal[] = [];
    vi.stubGlobal("fetch", (_: unknown, init: RequestInit) => {
      if (init.signal) signals.push(init.signal);
      return new Promise(() => undefined);
    });
    void client(connection, DaemonService).getDaemonStatus({});
    await flush();
    streamsOf(connection).change(true);
    await flush();
    expect(signals.map((signal) => signal.aborted)).toEqual([false]);
  });
});

describe("a slow answer", () => {
  it("is not cut while its bytes arrive", async () => {
    const { read, sent, plan } = page();
    const slow = opened();
    plan(() => Promise.resolve(slow.response));
    const ended = read();
    await flush();
    for (let i = 0; i < 10; i++) {
      await pass(SILENT_MS - 1);
      slow.byte();
      await flush();
    }
    slow.rest();
    await flush();
    expect(ended).toEqual({ version: "0.4.2" });
    expect(sent).toHaveLength(1);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("is cut 40 s after its last byte, and the next is given 40 s", async () => {
    const { read, sent, plan } = page();
    const slow = opened();
    plan(() => Promise.resolve(slow.response));
    read();
    await flush();
    await pass(30 * SECOND);
    slow.byte();
    await flush();
    await pass(SILENT_MS - 1);
    expect(sent).toHaveLength(1);
    await pass(1);
    expect(sent).toHaveLength(2);
    await pass(SILENT_MS);
    expect(sent).toHaveLength(3);
  });

  it("goes on when the streams are back, if a byte came while they were lost", async () => {
    const { read, sent, plan, streams } = page();
    const slow = opened();
    plan(() => Promise.resolve(slow.response));
    const ended = read();
    await flush();
    streams.change(false);
    slow.byte();
    await flush();
    streams.change(true);
    await flush();
    expect(sent.map(({ signal }) => signal.aborted)).toEqual([false]);
    slow.rest();
    await flush();
    expect(ended).toEqual({ version: "0.4.2" });
  });

  it("starts over when the streams are back, if none did", async () => {
    const { read, sent, plan, streams } = page();
    const slow = opened();
    plan(() => Promise.resolve(slow.response), alive);
    const ended = read();
    await flush();
    slow.byte();
    await flush();
    streams.change(false);
    streams.change(true);
    await flush();
    expect(ended).toEqual({ version: "0.4.2" });
    expect(sent).toHaveLength(2);
  });
});

describe("what is not a read, or is answered", () => {
  it("a write is sent once, and no limit is set to it", async () => {
    const { connection, sent, plan } = page();
    const write = () => client(connection, ChangeService).describeChange({ repo: "r" });
    plan(dead);
    await expect(write()).rejects.toMatchObject({ code: Code.Unknown });
    void write();
    await flush();
    streamsOf(connection).change(false);
    streamsOf(connection).change(true);
    await pass(60 * 60 * SECOND);
    expect(sent.map(({ signal }) => signal.aborted)).toEqual([true, false]);
  });

  it("the daemon's refusal is the page's at once", async () => {
    const { read, sent, plan } = page();
    plan(() => Promise.resolve(refusal(Code.NotFound)));
    const ended = read();
    await flush();
    expect(ended).toEqual({ code: Code.NotFound });
    expect(sent).toHaveLength(1);
  });

  it("a stream is live/silence.ts's", async () => {
    const { connection, sent, plan } = page();
    plan(dead);
    const stream = client(connection, InboxService).watchInbox({});
    await expect(stream[Symbol.asyncIterator]().next()).rejects.toMatchObject({
      code: Code.Unknown,
    });
    await pass(60 * SECOND);
    expect(sent).toHaveLength(1);
  });

  it("over http, a read is sent once, and is not cut", async () => {
    const { read, sent, plan, streams } = page(false);
    plan(dead);
    const ended = read();
    read();
    await flush();
    streams.change(false);
    streams.change(true);
    await pass(60 * 60 * SECOND);
    expect(ended).toEqual({ code: Code.Unknown });
    expect(sent.map(({ signal }) => signal.aborted)).toEqual([true, false]);
    expect(vi.getTimerCount()).toBe(0);
  });

  it("is what the page is served over", async () => {
    vi.stubGlobal("location", { protocol: "https:", origin: "https://reef.example" });
    const connection = browserConnection("token");
    const fetched = vi.fn(dead);
    vi.stubGlobal("fetch", fetched);
    const read = client(connection, DaemonService).getDaemonStatus({});
    const failed = expect(read).rejects.toMatchObject({ code: Code.Unknown });
    streamsOf(connection).change(true);
    await pass(60 * SECOND);
    await failed;
    expect(fetched).toHaveBeenCalledTimes(TRIES);
  });
});
