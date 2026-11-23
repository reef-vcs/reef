// Streams that fall silent (review R1): cut and opened again, together, once;
// never on a connection the daemon's keepalives reach; whatever the clocks do;
// and a follower that takes over from a leader its browser suspended.
import { Code, ConnectError } from "@connectrpc/connect";
import { describe, expect, it } from "vitest";
import {
  environment,
  FakeBus,
  FakeLocks,
  FakeStreams,
  FakeTime,
  manualFrames,
  operationsFrame,
  presenceFrame,
  settle,
} from "../test/fakes";
import { create } from "@bufbuild/protobuf";
import { WatchInboxResponseSchema } from "../gen/reef/api/v1/inbox_pb";
import { after } from "./backoff";
import { browserClock, SILENT_MS, SILENT_ON_WAKE_MS, Silence } from "./silence";
import { ANSWER_MS, Live } from "./share";

const KEEPALIVE_MS = 15_000;
const MINUTE = 60_000;

function daemon() {
  const streams = new FakeStreams();
  /** Every open stream's frame, as on a working connection. */
  const frames = async () => {
    streams.lastOperations().push(operationsFrame({ cursor: "c1", live: true }));
    streams.lastPresence().push(presenceFrame([], true));
    streams.lastInbox().push(create(WatchInboxResponseSchema, { synced: true }));
    await settle(30);
  };
  const opened = () => ({
    operations: streams.operationCalls.length,
    presence: streams.presenceCalls.length,
    inbox: streams.inboxCalls.length,
  });
  return { streams, frames, opened };
}

const each = (n: number) => ({ operations: n, presence: n, inbox: n });

async function alone() {
  const time = new FakeTime();
  const { streams, frames, opened } = daemon();
  const live = new Live(streams.connection(), environment(manualFrames().frame, undefined, time));
  live.start();
  await settle(30);
  await frames();
  expect(live.state).toBe("live");
  expect(opened()).toEqual(each(1));
  return { time, streams, frames, opened, live };
}

describe("a silent connection", () => {
  it("is cut and opened again, and is not live until frames arrive", async () => {
    const { time, streams, frames, opened, live } = await alone();
    await time.pass(SILENT_MS - 1);
    expect(live.state).toBe("live");
    await time.pass(1);
    expect(live.state).toBe("reconnecting");
    expect(opened()).toEqual(each(2));
    expect(streams.operationCalls[1]?.request.start).toEqual({ case: "cursor", value: "c1" });
    await frames();
    expect(live.state).toBe("live");
    live.close();
  });

  it("is cut when one stream of several is silent", async () => {
    const { time, streams, opened, live } = await alone();
    for (let i = 0; i < 3; i++) {
      await time.pass(KEEPALIVE_MS);
      streams.lastOperations().push(operationsFrame({ cursor: "c1", live: true }));
      streams.lastPresence().push(presenceFrame([], true));
      await settle(30);
    }
    expect(opened()).toEqual(each(2));
    live.close();
  });

  it("is cut at once when the page is shown again, once however many events say so", async () => {
    const { time, frames, opened, live } = await alone();
    await time.pass(SILENT_ON_WAKE_MS);
    await Promise.all([time.wake(), time.wake(), time.wake()]);
    await time.wake();
    expect(opened()).toEqual(each(2));
    expect(live.state).toBe("reconnecting");
    await frames();
    expect(live.state).toBe("live");
    live.close();
  });

  it("is seen after a sleep that stopped the timers and the page's clock", async () => {
    const { time, opened, live } = await alone();
    time.sleep(30 * MINUTE);
    await time.wake();
    expect(opened()).toEqual(each(2));
    live.close();
  });

  it("is seen when the wall clock is set back", async () => {
    const { time, opened, live } = await alone();
    time.set(0);
    await time.pass(SILENT_MS);
    expect(opened()).toEqual(each(2));
    live.close();
  });

  it("is opened again less and less often while no stream gets a frame", async () => {
    const { time, frames, opened, live } = await alone();
    // The fake inbox sends a first frame at each open; here none does.
    await time.pass(10 * MINUTE);
    const { operations } = opened();
    expect(operations).toBeGreaterThan(3);
    expect(operations).toBeLessThan(12);
    await frames();
    expect(live.state).toBe("live");
    live.close();
  });
});

describe("a working connection", () => {
  it("is never cut, shown again or not", async () => {
    const { time, frames, opened, live } = await alone();
    for (let i = 0; i < 40; i++) {
      await time.pass(KEEPALIVE_MS);
      await time.wake();
      await frames();
      await time.wake();
    }
    expect(opened()).toEqual(each(1));
    expect(live.state).toBe("live");
    live.close();
  });

  it("is cut once at most when the wall clock jumps forward", async () => {
    const { time, frames, opened, live } = await alone();
    time.set(4_000_000_000_000);
    for (let i = 0; i < 8; i++) {
      await time.wake();
      await frames();
      await time.pass(KEEPALIVE_MS);
    }
    expect(opened()).toEqual(each(2));
    live.close();
  });

  it("ends its timers with the page", async () => {
    const { time, opened, live } = await alone();
    live.close();
    await time.pass(10 * MINUTE);
    await time.wake();
    expect(opened()).toEqual(each(1));
  });
});

describe("Silence", () => {
  it("cuts a stream that ignores its signal, and ends its request", async () => {
    let signalled: AbortSignal | undefined;
    const silence = new Silence(browserClock, after);
    const stream = silence.watch(new AbortController().signal, (signal) => {
      signalled = signal;
      return {
        [Symbol.asyncIterator]: () => ({ next: () => new Promise<never>(() => undefined) }),
      };
    });
    const next = stream.next();
    await settle();
    silence.check(0);
    await expect(next).rejects.toMatchObject({ code: Code.Unavailable });
    expect(signalled?.aborted).toBe(true);
  });

  it("passes on the frames, the end and the error", async () => {
    const silence = new Silence(browserClock, after);
    const { signal } = new AbortController();
    async function* two() {
      yield 1;
      yield 2;
    }
    const got: number[] = [];
    for await (const frame of silence.watch(signal, two)) got.push(frame);
    expect(got).toEqual([1, 2]);
    const fails = () => ({
      [Symbol.asyncIterator]: () => ({
        next: () => Promise.reject(new ConnectError("down", Code.Unavailable)),
      }),
    });
    await expect(silence.watch(signal, fails).next()).rejects.toThrow("down");
  });
});

describe("a follower", () => {
  async function two() {
    const time = new FakeTime();
    const shared = { locks: new FakeLocks(), bus: new FakeBus() };
    const { streams, frames, opened } = daemon();
    const tab = (shown = true) => {
      const env = environment(manualFrames().frame, shared, time);
      const live = new Live(
        streams.connection(),
        shown ? env : { ...env, onWake: () => () => undefined },
      );
      live.start();
      return live;
    };
    // The leader's page is hidden: nothing wakes it.
    const leader = tab(false);
    await settle(30);
    await frames();
    const follower = tab();
    await settle(30);
    expect(follower.state).toBe("live");
    return { time, leader, follower, streams, frames, opened, bus: shared.bus };
  }

  it("shown again, has its hidden leader look at its streams", async () => {
    const { time, leader, follower, frames, opened } = await two();
    await time.pass(SILENT_ON_WAKE_MS);
    await time.wake();
    expect(opened()).toEqual(each(2));
    expect(follower.state).toBe("reconnecting");
    await frames();
    expect(follower.state).toBe("live");
    expect(leader.leading).toBe(true);
    leader.close();
    follower.close();
  });

  it("takes over from a leader that does not answer, which then follows", async () => {
    const { time, leader, follower, streams, frames, opened, bus } = await two();
    const thaw = bus.freeze(0);
    await time.pass(KEEPALIVE_MS);
    time.sleep(MINUTE);
    await time.wake();
    expect(follower.leading).toBe(false);
    await time.pass(ANSWER_MS);
    expect(follower.leading).toBe(true);
    streams.lastPresence().push(presenceFrame([], true));
    await settle(30);
    expect(follower.state, "with no operations frame").toBe("reconnecting");
    thaw();
    await settle(50);
    expect(leader.leading).toBe(false);
    await frames();
    expect(follower.state).toBe("live");
    expect(leader.state).toBe("live");
    const { operations } = opened();
    await time.pass(KEEPALIVE_MS);
    await frames();
    await time.wake();
    expect(opened().operations, "two tabs do not take the lock from each other").toBe(operations);
    follower.close();
    await settle(50);
    expect(leader.leading, "the lock passes back").toBe(true);
    leader.close();
  });

  it("waits for the lock once, however often it was taken from it", async () => {
    const { time, leader, follower, frames, opened, bus } = await two();
    const thaw = bus.freeze(0);
    time.sleep(MINUTE);
    await time.wake();
    await time.pass(ANSWER_MS);
    expect(follower.leading).toBe(true);
    thaw();
    await settle(50);
    await frames();
    // Now the tab that took over is the one suspended.
    const thawed = bus.freeze(1);
    await time.pass(SILENT_MS + ANSWER_MS);
    expect(leader.leading).toBe(true);
    thawed();
    await settle(50);
    expect(follower.leading).toBe(false);
    const { operations } = opened();
    leader.close();
    await settle(50);
    expect(follower.leading).toBe(true);
    expect(opened().operations).toBe(operations + 1);
    follower.close();
  });

  it("leaves a leader alone that answers but has no daemon", async () => {
    const { time, leader, follower } = await two();
    for (let i = 0; i < 60; i++) {
      await time.pass(5_000);
      expect(follower.leading, `after ${String(i * 5)} s`).toBe(false);
    }
    expect(leader.leading).toBe(true);
    expect(follower.state).toBe("reconnecting");
    leader.close();
    follower.close();
  });
});
