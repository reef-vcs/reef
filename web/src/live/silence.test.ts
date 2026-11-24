// Streams that fall silent (R3.40 reviews): on the remote page each is cut
// and opened again alone, at a limit of its own, whatever the clocks do, and
// never on a connection the daemon's keepalives reach; a page that has not
// its new inbox whole is not live. The local page shares its streams, as
// before, and cuts none.
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  InboxItemSchema,
  InboxListing,
  InboxSectionChangeSchema,
  InboxService,
  WatchInboxResponseSchema,
  type WatchInboxResponse,
} from "../gen/reef/api/v1/inbox_pb";
import {
  environment,
  FakeBus,
  FakeLocks,
  FakeStreams,
  FakeTime,
  manualFrames,
  operationsFrame,
  presenceFrame,
  Pushable,
  sessionInvalid,
  settle,
} from "../test/fakes";
import { streamsOf } from "../api/streams";
import { after } from "./backoff";
import { browserEnvironment, Live } from "./share";
import {
  browserSilence,
  line,
  MAX_DOUBLINGS,
  onWake,
  SILENT_MS,
  SILENT_ON_WAKE_MS,
} from "./silence";

const KEEPALIVE_MS = 15_000;
const SECOND = 1_000;
const MINUTE = 60 * SECOND;

/** A daemon whose streams say nothing until the test does. */
function daemon() {
  const streams = new FakeStreams();
  const inboxes: Pushable<WatchInboxResponse>[] = [];
  const connection = streams.connection({
    services: {
      [InboxService.typeName]: {
        watchInbox: (_: never, options: { signal: AbortSignal | undefined }) => {
          const stream = new Pushable<WatchInboxResponse>(options.signal);
          inboxes.push(stream);
          return stream;
        },
      },
    },
  });
  const say = {
    operations: () => {
      streams.lastOperations().push(operationsFrame({ cursor: "c1", live: true }));
    },
    presence: () => {
      streams.lastPresence().push(presenceFrame([], true));
    },
    inbox: (frame = create(WatchInboxResponseSchema, { synced: true })) => {
      inboxes.at(-1)?.push(frame);
    },
  };
  const all = async () => {
    say.operations();
    say.presence();
    say.inbox();
    await settle(30);
  };
  const opened = () => ({
    operations: streams.operationCalls.length,
    presence: streams.presenceCalls.length,
    inbox: inboxes.length,
  });
  return { streams, inboxes, connection, say, all, opened };
}

const each = (n: number) => ({ operations: n, presence: n, inbox: n });

/** The remote page, live. */
async function page() {
  const time = new FakeTime();
  const served = daemon();
  const live = new Live(served.connection, environment(manualFrames().frame, undefined, time));
  live.start();
  await settle(30);
  await served.all();
  expect(live.state).toBe("live");
  expect(served.opened()).toEqual(each(1));
  return { time, live, ...served };
}

/** Passes time until the page is not live; how long that took. */
async function notLiveAfter(time: FakeTime, live: Live): Promise<number> {
  for (let s = 0; s < 1_200; s++) {
    if (live.state !== "live") return s * SECOND;
    await time.pass(SECOND);
  }
  throw new Error("it is live after 20 minutes of silence");
}

describe("a dead connection", () => {
  it("is found 40 s after its last frame, and the page is live again on the next", async () => {
    const { time, live, streams, all, opened } = await page();
    await time.pass(SILENT_MS - 1);
    expect(live.state).toBe("live");
    await time.pass(1);
    expect(live.state).toBe("reconnecting");
    expect(opened()).toEqual(each(2));
    expect(streams.operationCalls[1]?.request.start).toEqual({ case: "cursor", value: "c1" });
    await all();
    expect(live.state).toBe("live");
    live.close();
  });

  it("is found 40 s after it dies, having lived for a while", async () => {
    const { time, live, all } = await page();
    for (let i = 0; i < 3; i++) {
      await time.pass(KEEPALIVE_MS);
      await all();
    }
    expect(await notLiveAfter(time, live)).toBe(SILENT_MS);
    live.close();
  });

  it("is found 40 s after it dies, looked at before and healthy then", async () => {
    const { time, live } = await page();
    await time.pass(KEEPALIVE_MS);
    await time.wake();
    expect(await notLiveAfter(time, live)).toBe(SILENT_MS - KEEPALIVE_MS);
    live.close();
  });

  it.each([45 * SECOND, 340 * SECOND])(
    "is found 40 s after it dies again, after an outage of %i ms",
    async (outage) => {
      const { time, live, all } = await page();
      await time.pass(outage);
      expect(live.state).toBe("reconnecting");
      await all();
      expect(live.state).toBe("live");
      expect(await notLiveAfter(time, live)).toBe(SILENT_MS);
      live.close();
    },
  );

  it("is seen after a sleep that stopped the timers and the page's clock", async () => {
    const { time, live, opened } = await page();
    time.sleep(30 * MINUTE);
    await time.wake();
    expect(opened()).toEqual(each(2));
    expect(live.state).toBe("reconnecting");
    live.close();
  });

  it("is seen when the wall clock is set back", async () => {
    const { time, live, opened } = await page();
    time.set(0);
    await time.pass(SILENT_MS);
    expect(opened()).toEqual(each(2));
    live.close();
  });

  it("is seen on a wake after 20 s, once however often the page wakes", async () => {
    const { time, live, all, opened } = await page();
    await time.pass(SILENT_ON_WAKE_MS - 1);
    await time.wake();
    expect(live.state).toBe("live");
    await time.pass(1);
    await Promise.all([time.wake(), time.wake(), time.wake()]);
    await time.wake();
    expect(opened()).toEqual(each(2));
    expect(live.state).toBe("reconnecting");
    await all();
    expect(live.state).toBe("live");
    live.close();
  });
});

describe("one silent stream of three", () => {
  it.each(["operations", "presence", "inbox"] as const)(
    "is cut alone (%s), and the page is not live until it speaks",
    async (silent) => {
      const { time, live, say, opened } = await page();
      const others = (["operations", "presence", "inbox"] as const).filter((s) => s !== silent);
      for (let i = 0; i < 3; i++) {
        await time.pass(KEEPALIVE_MS);
        for (const other of others) say[other]();
        await settle(30);
      }
      expect(opened()).toEqual({ ...each(1), [silent]: 2 });
      expect(live.state).toBe("reconnecting");
      say[silent]();
      await settle(30);
      expect(live.state).toBe("live");
      live.close();
    },
  );
});

describe("a first frame that takes long", () => {
  /** Lives with a silent inbox for `ms`; when the inbox was opened. */
  async function silentInbox(ms: number) {
    const opens: number[] = [];
    const { time, live, say, opened } = await page();
    for (let at = 0; at < ms; at += SECOND) {
      if (at % KEEPALIVE_MS === 0) {
        say.operations();
        say.presence();
        await settle(30);
      }
      if (opened().inbox > opens.length + 1) opens.push(at / SECOND);
      await time.pass(SECOND);
    }
    return { time, live, say, opened, opens };
  }

  it("is given twice as long each time, whatever the other streams do", async () => {
    const { live, opens, opened } = await silentInbox(20 * MINUTE);
    // Cut after 40 s with a frame had; then, with none, 40, 80, 160, 320, 320 s.
    expect(opens).toEqual([40, 80, 160, 320, 640, 960]);
    expect(opened()).toEqual({ operations: 1, presence: 1, inbox: 7 });
    live.close();
  });

  it("loads in up to 320 s", async () => {
    const { time, live, say, opened } = await silentInbox(320 * SECOND);
    const before = opened().inbox;
    for (let at = 0; at < 310; at += 10) {
      await time.pass(10 * SECOND);
      say.operations();
      say.presence();
      await settle(30);
    }
    expect(opened().inbox).toBe(before);
    expect(live.state).toBe("reconnecting");
    say.inbox();
    await settle(30);
    expect(live.state).toBe("live");
    expect(SILENT_MS * 2 ** MAX_DOUBLINGS).toBe(320 * SECOND);
    live.close();
  });

  it("is asked again on a wake after 40 s, whatever its limit, and not before", async () => {
    const { time, live, say, opened } = await silentInbox(320 * SECOND);
    const before = opened().inbox;
    for (let n = 0; n < 3; n++) {
      await time.pass(SILENT_MS - 1);
      say.operations();
      say.presence();
      await time.wake();
      expect(opened().inbox).toBe(before + n);
      await time.pass(1);
      await time.wake();
      expect(opened().inbox).toBe(before + n + 1);
    }
    say.inbox();
    await settle(30);
    expect(live.state).toBe("live");
    live.close();
  });

  it("is given 40 s in all once the page woke, however soon after its start that was", async () => {
    const { time, live, say, opened } = await silentInbox(320 * SECOND);
    const before = opened().inbox;
    const pass = async (ms: number) => {
      await time.pass(ms);
      say.operations();
      say.presence();
      await settle(30);
    };
    await pass(10 * SECOND);
    await time.wake();
    await pass(SILENT_MS - 10 * SECOND - 1);
    expect(opened().inbox).toBe(before);
    await pass(1);
    expect(opened().inbox).toBe(before + 1);
    // The next has the longer limit again.
    for (let at = 0; at < 310; at += 10) await pass(10 * SECOND);
    expect(opened().inbox).toBe(before + 1);
    live.close();
  });

  it("is cut on a wake past its limit, when no timer ran", async () => {
    const { time, live, opened } = await page();
    await time.pass(SILENT_MS);
    expect(opened()).toEqual(each(2));
    time.sleep(SILENT_MS - 1);
    await time.wake();
    expect(opened()).toEqual(each(2));
    time.sleep(1);
    await time.wake();
    expect(opened()).toEqual(each(3));
    live.close();
  });

  it("is given 40 s again once one came", async () => {
    const { time, live, all, opened } = await page();
    await time.pass(5 * MINUTE);
    await all();
    const before = opened().inbox;
    await time.pass(SILENT_MS);
    expect(opened().inbox).toBe(before + 1);
    await time.pass(2 * SILENT_MS);
    expect(opened().inbox, "given 40 s, and the next 80 s").toBe(before + 2);
    live.close();
  });
});

describe("a working connection", () => {
  it("is never cut, and holds one timer and one listener a stream", async () => {
    const { time, live, all, opened } = await page();
    for (let i = 0; i < 40; i++) {
      await time.pass(KEEPALIVE_MS);
      await time.wake();
      await all();
      await time.wake();
      expect(time.held).toBe(6);
    }
    expect(opened()).toEqual(each(1));
    expect(live.state).toBe("live");
    live.close();
  });

  it("is cut once at most when the wall clock jumps forward", async () => {
    const { time, live, all, opened } = await page();
    time.set(4_000_000_000_000);
    for (let i = 0; i < 8; i++) {
      await time.wake();
      await all();
      await time.pass(KEEPALIVE_MS);
    }
    expect(opened()).toEqual(each(2));
    live.close();
  });
});

describe("a page that ends", () => {
  it("closed, leaves no timer, listener or stream", async () => {
    const { time, live, opened } = await page();
    live.close();
    await settle(30);
    expect(time.held).toBe(0);
    await time.pass(20 * MINUTE);
    await time.wake();
    expect(opened()).toEqual(each(1));
  });

  it("refused by the daemon, leaves none either", async () => {
    const { time, live, streams } = await page();
    const ended = vi.fn();
    live.onEnded(ended);
    streams.lastPresence().fail(sessionInvalid());
    await settle(30);
    expect(ended).toHaveBeenCalledOnce();
    expect(time.held).toBe(0);
  });

  it("closed while it waits to open again, leaves none", async () => {
    const { time, live } = await page();
    await time.pass(SILENT_MS);
    live.close();
    await settle(30);
    expect(time.held).toBe(0);
  });
});

describe("the header after a cut", () => {
  it("is live only once the new inbox is whole", async () => {
    const { time, live, say } = await page();
    const section = (repoId: string, key: string) =>
      create(InboxSectionChangeSchema, {
        repoId,
        listing: InboxListing.END,
        items: [create(InboxItemSchema, { key, repoId })],
      });
    say.inbox(
      create(WatchInboxResponseSchema, {
        sections: [section("a", "old a"), section("b", "old b")],
        synced: true,
      }),
    );
    await settle(30);
    await time.pass(SILENT_MS);
    say.operations();
    say.presence();
    for (const frame of [{}, { sections: [section("a", "new a")] }]) {
      say.inbox(create(WatchInboxResponseSchema, { ...frame, synced: false }));
      await settle(30);
      expect(live.state).toBe("reconnecting");
      expect([...(live.inbox.sections.get("b")?.items.keys() ?? [])]).toEqual(["old b"]);
    }
    say.inbox(
      create(WatchInboxResponseSchema, { sections: [section("b", "new b")], synced: true }),
    );
    await settle(30);
    expect(live.state).toBe("live");
    expect([...(live.inbox.sections.get("b")?.items.keys() ?? [])]).toEqual(["new b"]);
    live.close();
  });
});

describe("what reads over the connection", () => {
  it("is told when the streams are live again, and not the first time", async () => {
    const { time, live, connection, all } = await page();
    const streams = streamsOf(connection);
    const told = vi.fn();
    const stop = streams.onResume(told);
    expect([streams.live, streams.resumes]).toEqual([true, 0]);
    await time.pass(SILENT_MS);
    expect(streams.live).toBe(false);
    expect(told).not.toHaveBeenCalled();
    await all();
    expect([streams.live, streams.resumes]).toEqual([true, 1]);
    expect(told).toHaveBeenCalledOnce();
    stop();
    await time.pass(SILENT_MS);
    await all();
    expect(streams.resumes).toBe(2);
    expect(told).toHaveBeenCalledOnce();
    live.close();
  });

  it("is told in a tab that follows another's streams too", async () => {
    const shared = { locks: new FakeLocks(), bus: new FakeBus() };
    const lead = daemon();
    const leader = new Live(lead.connection, environment(manualFrames().frame, shared));
    leader.start();
    await settle(30);
    await lead.all();
    const follow = daemon();
    const follower = new Live(follow.connection, environment(manualFrames().frame, shared));
    follower.start();
    await settle(30);
    expect([follower.state, streamsOf(follow.connection).resumes]).toEqual(["live", 0]);
    lead.streams.lastPresence().fail(new ConnectError("down", Code.Unavailable));
    await settle(30);
    expect(streamsOf(follow.connection).live).toBe(false);
    await lead.all();
    expect([follower.state, streamsOf(follow.connection).resumes]).toEqual(["live", 1]);
    leader.close();
    follower.close();
  });
});

/** A stream that says nothing, whatever it is told. */
const never = { [Symbol.asyncIterator]: () => ({ next: () => new Promise<never>(() => {}) }) };

describe("the first time", () => {
  it.each(["operations", "presence", "inbox"] as const)(
    "is live only once %s has all it holds",
    async (last) => {
      const { connection, say, streams } = daemon();
      const live = new Live(connection, environment(manualFrames().frame));
      live.start();
      await settle(30);
      const part = {
        operations: () => {
          streams.lastOperations().push(operationsFrame({ cursor: "c0", live: false }));
        },
        presence: () => {
          streams.lastPresence().push(presenceFrame([], false));
        },
        inbox: () => {
          say.inbox(create(WatchInboxResponseSchema, { synced: false }));
        },
      };
      for (const stream of ["operations", "presence", "inbox"] as const) {
        if (stream === last) part[stream]();
        else say[stream]();
      }
      await settle(30);
      expect(live.state).toBe("connecting");
      say[last]();
      await settle(30);
      expect(live.state).toBe("live");
      live.close();
    },
  );

  it.each(["operations", "presence", "inbox"] as const)(
    "is not live from when %s fails until that stream speaks",
    async (failing) => {
      const { connection, all, say, streams, inboxes } = daemon();
      let retry = () => {};
      const live = new Live(connection, {
        ...environment(manualFrames().frame),
        sleep: () =>
          new Promise((resolve) => {
            retry = resolve;
          }),
      });
      live.start();
      await settle(30);
      await all();
      expect(live.state).toBe("live");
      const down = new ConnectError("down", Code.Unavailable);
      if (failing === "operations") streams.lastOperations().fail(down);
      else if (failing === "presence") streams.lastPresence().fail(down);
      else inboxes.at(-1)?.fail(down);
      await settle(30);
      const others = (["operations", "presence", "inbox"] as const).filter((s) => s !== failing);
      for (const step of [() => undefined, retry]) {
        step();
        for (const other of others) say[other]();
        await settle(30);
        expect(live.state).toBe("reconnecting");
      }
      say[failing]();
      await settle(30);
      expect(live.state).toBe("live");
      live.close();
    },
  );
});

describe("a line", () => {
  it("is cut at once when its signal is aborted already", async () => {
    const time = new FakeTime();
    const page = new AbortController();
    const stream = line(time.silence)(page.signal, () => {
      page.abort();
      return never;
    });
    await expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow("silent");
    expect(time.held).toBe(0);
  });

  it("is cut though its stream ignores that, and its request is aborted", async () => {
    const time = new FakeTime();
    let request: AbortSignal | undefined;
    const stream = line(time.silence)(new AbortController().signal, (signal) => {
      request = signal;
      return never;
    });
    const next = stream[Symbol.asyncIterator]().next();
    const refused = expect(next).rejects.toMatchObject({ code: Code.Unavailable });
    await time.pass(SILENT_MS);
    await refused;
    expect(request?.aborted).toBe(true);
    expect(time.held).toBe(0);
  });

  it("passes on frames, the end and an error, and then holds nothing", async () => {
    const time = new FakeTime();
    const page = new AbortController();
    const added = vi.spyOn(page.signal, "addEventListener");
    const removed = vi.spyOn(page.signal, "removeEventListener");
    const read = line(time.silence);
    async function* two() {
      yield 1;
      yield 2;
    }
    const got: number[] = [];
    const request: { added?: number; removed?: number } = {};
    const opened = (signal: AbortSignal) => {
      const add = vi.spyOn(signal, "addEventListener");
      const remove = vi.spyOn(signal, "removeEventListener");
      return (async function* () {
        for await (const frame of two()) {
          yield frame;
          request.added = add.mock.calls.length;
          request.removed = remove.mock.calls.length;
        }
      })();
    };
    for await (const frame of read(page.signal, opened)) got.push(frame);
    expect(got).toEqual([1, 2]);
    expect(request, "no listener is left a frame").toEqual({ added: 2, removed: 2 });
    const fails = () => ({
      [Symbol.asyncIterator]: () => ({
        next: () => Promise.reject(new ConnectError("down", Code.Unavailable)),
      }),
    });
    await expect(read(page.signal, fails)[Symbol.asyncIterator]().next()).rejects.toThrow("down");
    expect(time.held).toBe(0);
    expect(removed).toHaveBeenCalledTimes(added.mock.calls.length);
  });

  it("ends the request of a stream its reader leaves", async () => {
    let request: AbortSignal | undefined;
    const read = line(new FakeTime().silence)(new AbortController().signal, async function* (s) {
      request = s;
      yield 1;
      yield 2;
    });
    for await (const frame of read) if (frame === 1) break;
    expect(request?.aborted).toBe(true);
  });

  it("opens nothing on a page that has ended", async () => {
    const open = vi.fn();
    const read = line(new FakeTime().silence)(AbortSignal.abort(), open);
    for await (const frame of read) expect(frame).toBeUndefined();
    expect(open).not.toHaveBeenCalled();
  });
});

describe("a line's timer", () => {
  it("that runs early is set again", async () => {
    const time = new FakeTime();
    const early = {
      ...time.silence,
      after: (ms: number, run: () => void) => time.silence.after(Math.max(ms - 1, 1), run),
    };
    const stream = line(early)(new AbortController().signal, () => never);
    const refused = expect(stream[Symbol.asyncIterator]().next()).rejects.toThrow("silent");
    await time.pass(SILENT_MS - 1);
    expect(time.held).toBe(2);
    await time.pass(1);
    await refused;
  });

  it("is doubled once for an opening that is cut on several wakes", async () => {
    const { time, live, opened } = await page();
    await time.pass(SILENT_MS);
    time.sleep(30 * MINUTE);
    await Promise.all([time.wake(), time.wake(), time.wake()]);
    expect(opened()).toEqual(each(3));
    await time.pass(2 * SILENT_MS);
    expect(opened()).toEqual(each(4));
    live.close();
  });
});

describe("waking", () => {
  afterEach(() => {
    vi.restoreAllMocks();
  });

  it.each([
    ["visibilitychange", document],
    ["pageshow", window],
    ["online", window],
  ])("is each of three events alone: %s", async (event, target) => {
    const time = new FakeTime();
    const { connection, all, opened } = daemon();
    const live = new Live(connection, {
      ...environment(manualFrames().frame),
      line: () => line({ ...time.silence, onWake }),
    });
    live.start();
    await settle(30);
    await all();
    await time.pass(SILENT_ON_WAKE_MS);
    target.dispatchEvent(new Event(event));
    await settle(30);
    expect(opened()).toEqual(each(2));
    live.close();
    await settle(30);
    target.dispatchEvent(new Event(event));
    await settle(30);
    expect(opened()).toEqual(each(2));
  });

  it("is not a page being hidden, and stops when asked", () => {
    const woken = vi.fn();
    const stop = onWake(woken);
    const hidden = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(woken).not.toHaveBeenCalled();
    hidden.mockReturnValue("visible");
    document.dispatchEvent(new Event("visibilitychange"));
    expect(woken).toHaveBeenCalledOnce();
    stop();
    document.dispatchEvent(new Event("visibilitychange"));
    window.dispatchEvent(new Event("pageshow"));
    window.dispatchEvent(new Event("online"));
    expect(woken).toHaveBeenCalledOnce();
  });
});

describe("the page's environment", () => {
  it("over https takes no lock, shares nothing and cuts a silent stream", () => {
    vi.stubGlobal("navigator", { locks: { request: vi.fn() } });
    const remote = browserEnvironment(true);
    vi.unstubAllGlobals();
    expect(remote.locks).toBeUndefined();
    expect(remote.channel).toBeUndefined();
    expect(remote.line).toBe(line);
    expect(browserSilence).toMatchObject({ after, onWake });
    expect(browserSilence.clock.wall()).toBeCloseTo(Date.now(), -3);
    expect(browserSilence.clock.mono()).toBeCloseTo(performance.now(), -3);
  });

  it("over http shares, and reads each stream as it comes", async () => {
    vi.useFakeTimers();
    vi.stubGlobal("BroadcastChannel", EventTarget);
    vi.stubGlobal("navigator", { locks: { request: vi.fn() } });
    const local = browserEnvironment(false);
    expect(local.locks).toBe(navigator.locks);
    expect(local.channel?.("x")).toBeInstanceOf(EventTarget);
    let request: AbortSignal | undefined;
    const stream = local.line()(new AbortController().signal, (signal) => {
      request = signal;
      return never;
    });
    void stream[Symbol.asyncIterator]().next();
    vi.advanceTimersByTime(60 * MINUTE);
    window.dispatchEvent(new Event("online"));
    await settle(30);
    expect(vi.getTimerCount()).toBe(0);
    expect(request?.aborted).toBe(false);
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("is what the page is served over", () => {
    expect(location.protocol).toBe("http:");
    expect(browserEnvironment().line).not.toBe(line);
    vi.stubGlobal("location", { protocol: "https:" });
    expect(browserEnvironment().line).toBe(line);
    vi.unstubAllGlobals();
  });

  it("over http keeps one set of streams for every tab, silent or not", async () => {
    const shared = { locks: new FakeLocks(), bus: new FakeBus() };
    const { connection, all, opened } = daemon();
    const tabs = [1, 2, 3].map(() => {
      const live = new Live(connection, environment(manualFrames().frame, shared));
      live.start();
      return live;
    });
    await settle(30);
    await all();
    window.dispatchEvent(new Event("online"));
    await settle(30);
    expect(opened()).toEqual(each(1));
    expect(tabs.map((tab) => tab.leading)).toEqual([true, false, false]);
    for (const tab of tabs) tab.close();
  });
});
