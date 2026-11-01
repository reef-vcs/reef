// W5: presence is a set held by id; after a resubscribe, entries the new
// synced set lacks are ended; and the layer is live only once synced.
import { Code, ConnectError } from "@connectrpc/connect";
import { describe, expect, it, vi } from "vitest";
import {
  FakeStreams,
  environment,
  manualFrames,
  operationsFrame,
  presenceFrame,
  settle,
} from "../test/fakes";
import { Backoff } from "./backoff";
import { PresenceEndReason } from "../gen/reef/api/v1/presence_pb";
import { ENDINGS_LIMIT, PresenceStore, runPresence } from "./presence";
import { Live } from "./share";

describe("PresenceStore", () => {
  it("holds the set by id", () => {
    const store = new PresenceStore(manualFrames().frame);
    store.applyFrame(
      presenceFrame([
        { id: "p1", agent: "a" },
        { id: "p2", agent: "b" },
      ]),
    );
    store.applyFrame(presenceFrame([{ id: "p1", agent: "a2" }]));
    store.applyFrame(presenceFrame([{ id: "p2", ended: true }], true));
    expect([...store.entries.keys()]).toEqual(["p1"]);
    expect(store.entries.get("p1")!.agent).toBe("a2");
    expect(store.synced).toBe(true);
  });

  it("notifies once per animation frame", () => {
    const frames = manualFrames();
    const store = new PresenceStore(frames.frame);
    const listener = vi.fn();
    store.subscribe(listener);
    store.applyFrame(presenceFrame([{ id: "p1" }]));
    store.applyFrame(presenceFrame([{ id: "p2" }], true));
    frames.flush();
    expect(listener).toHaveBeenCalledOnce();
  });

  it("ends what a resubscription's synced set lacks", () => {
    const store = new PresenceStore(manualFrames().frame);
    store.beginResync();
    store.applyFrame(presenceFrame([{ id: "p1" }, { id: "p2" }, { id: "p3" }], true));
    store.beginResync();
    expect(store.synced).toBe(false);
    // The new set arrives in two frames; until synced, the old entries stay.
    store.applyFrame(presenceFrame([{ id: "p1" }]));
    expect(store.entries.size).toBe(3);
    store.applyFrame(presenceFrame([{ id: "p4" }], true));
    expect([...store.entries.keys()].sort()).toEqual(["p1", "p4"]);
    // Later frames are changes again.
    store.applyFrame(presenceFrame([{ id: "p5" }]));
    expect(store.entries.size).toBe(3);
  });

  it("resyncs after a dropped stream", async () => {
    const store = new PresenceStore(manualFrames().frame);
    const streams = new FakeStreams();
    const controller = new AbortController();
    const done = runPresence(
      (signal) => streams.connection().clients.presence.subscribePresence({}, { signal }),
      store,
      {
        signal: controller.signal,
        backoff: new Backoff(),
        sleep: async () => {
          await settle(1);
        },
        onSessionInvalid: vi.fn(),
      },
    );
    await settle();
    streams.lastPresence().push(presenceFrame([{ id: "p1" }, { id: "p2" }], true));
    await settle();
    streams.lastPresence().fail(new ConnectError("down", Code.Unavailable));
    await settle();
    expect(streams.presenceCalls).toHaveLength(2);
    streams.lastPresence().push(presenceFrame([{ id: "p2" }], true));
    await settle();
    expect([...store.entries.keys()]).toEqual(["p2"]);
    controller.abort();
    await done;
  });
});

describe("connection state", () => {
  it("is live only when operations are live and presence is synced", async () => {
    const streams = new FakeStreams();
    const frames = manualFrames();
    const live = new Live(streams.connection(), environment(frames.frame));
    live.start();
    await settle();
    expect(live.state).toBe("connecting");
    streams.lastOperations().push(operationsFrame({ live: true, cursor: "c1" }));
    await settle();
    expect(live.state).toBe("connecting");
    streams.lastPresence().push(presenceFrame([{ id: "p1" }]));
    await settle();
    expect(live.state).toBe("connecting");
    streams.lastPresence().push(presenceFrame([], true));
    await settle();
    expect(live.state).toBe("live");
    streams.lastPresence().fail(new ConnectError("down", Code.Unavailable));
    await settle(3);
    expect(live.state).toBe("reconnecting");
    live.close();
  });
});

describe("PresenceStore endings (R2.20 test 8)", () => {
  it("keeps each ended entry's last state, reason and time", () => {
    const store = new PresenceStore(manualFrames().frame, () => 42);
    store.applyFrame(presenceFrame([{ id: "p1", agent: "a", activity: "tests" }], true));
    store.applyFrame(
      presenceFrame([
        {
          id: "p1",
          agent: "a",
          activity: "tests",
          ended: true,
          endReason: PresenceEndReason.CLEARED,
        },
      ]),
    );
    expect(store.entries.size).toBe(0);
    const ending = store.endings.get("p1")!;
    expect(ending.reason).toBe(PresenceEndReason.CLEARED);
    expect(ending.presence.activity).toBe("tests");
    expect(ending.at).toBe(42);
    // Reported again: live, and no longer an ending.
    store.applyFrame(presenceFrame([{ id: "p1", agent: "a" }]));
    expect(store.endings.has("p1")).toBe(false);
  });

  it("records entries a resync's set drops as UNSPECIFIED, and counts resyncs", () => {
    const store = new PresenceStore(manualFrames().frame);
    store.beginResync();
    store.applyFrame(presenceFrame([{ id: "p1" }, { id: "p2" }], true));
    expect(store.resyncs).toBe(1);
    store.beginResync();
    store.applyFrame(presenceFrame([{ id: "p2" }], true));
    expect(store.resyncs).toBe(2);
    expect(store.endings.get("p1")!.reason).toBe(PresenceEndReason.UNSPECIFIED);
    expect(store.endings.has("p2")).toBe(false);
  });

  it("holds at most ENDINGS_LIMIT endings, dropping the oldest", () => {
    const store = new PresenceStore(manualFrames().frame);
    const ids = Array.from({ length: ENDINGS_LIMIT + 2 }, (_, i) => `p${String(i)}`);
    store.applyFrame(presenceFrame(ids.map((id) => ({ id, ended: true }))));
    expect(store.endings.size).toBe(ENDINGS_LIMIT);
    expect(store.endings.has("p0")).toBe(false);
    expect(store.endings.has("p1")).toBe(false);
    expect(store.endings.has("p2")).toBe(true);
  });

  it("leaves the joining tab's frame unchanged: live entries only", () => {
    const store = new PresenceStore(manualFrames().frame);
    store.applyFrame(presenceFrame([{ id: "p1" }, { id: "p2" }], true));
    store.applyFrame(presenceFrame([{ id: "p2", ended: true }]));
    const frame = store.snapshotFrame();
    expect(frame.events.map((e) => [e.presence?.id, e.ended])).toEqual([["p1", false]]);
  });
});
