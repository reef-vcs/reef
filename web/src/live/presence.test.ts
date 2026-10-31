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
import { PresenceStore, runPresence } from "./presence";
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
