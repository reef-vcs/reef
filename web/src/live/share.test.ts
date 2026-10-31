// W6: one set of streams per browser, shared across tabs through a Web Lock
// and a BroadcastChannel.
import { toBinary } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { SubscribeOperationsResponseSchema } from "../gen/reef/api/v1/operations_pb";
import {
  sessionInvalid as sessionInvalidError,
  FakeBus,
  FakeLocks,
  FakeStreams,
  environment,
  manualFrames,
  operation,
  operationsFrame,
  presenceFrame,
  settle,
} from "../test/fakes";
import type { OperationBatch } from "./operations";
import { Live } from "./share";

function browser() {
  const shared = { locks: new FakeLocks(), bus: new FakeBus() };
  const streams = new FakeStreams();
  /** A tab; `refused` says whether the daemon refuses its token. */
  const tab = (frame = manualFrames(), refused: () => boolean = () => false) => {
    const live = new Live(streams.connection(refused), environment(frame.frame, shared));
    const ended = { value: false };
    live.onEnded(() => {
      ended.value = true;
    });
    live.start();
    return { live, frames: frame, ended };
  };
  return { streams, tab, bus: shared.bus };
}

describe("shared streams", () => {
  it("opens streams only in the lock holder", async () => {
    const { streams, tab } = browser();
    const first = tab();
    const second = tab();
    const third = tab();
    await settle();
    expect(streams.operationCalls).toHaveLength(1);
    expect(streams.presenceCalls).toHaveLength(1);
    expect(first.live.leading).toBe(true);
    expect(second.live.leading).toBe(false);
    expect(third.live.leading).toBe(false);
    for (const t of [first, second, third]) t.live.close();
  });

  it("a leader without animation frames still republishes every frame as it arrives", async () => {
    const { streams, tab } = browser();
    // A hidden leader: its animation frames never run.
    const leader = tab({ frame: () => undefined, flush: () => undefined, pending: () => 0 });
    const follower = tab();
    await settle();
    const batches: OperationBatch[] = [];
    follower.live.operations.subscribe((batch) => batches.push(batch));
    streams
      .lastOperations()
      .push(
        operationsFrame({ operations: [operation("a", "r", 1)], cursor: "c1", live: true }),
        operationsFrame({ operations: [operation("b", "r", 2)], cursor: "c2" }),
      );
    await settle(20);
    follower.frames.flush();
    expect(batches.flatMap((b) => b.operations.map((o) => o.id))).toEqual(["a", "b"]);
    expect(follower.live.operations.cursor).toBe("c2");
    leader.live.close();
    follower.live.close();
  });

  it("a joining tab gets the presence set and the stream's position from hello", async () => {
    const { streams, tab } = browser();
    const leader = tab();
    await settle();
    streams.lastPresence().push(presenceFrame([{ id: "p1", agent: "a", repoId: "r" }], true));
    streams.lastOperations().push(operationsFrame({ cursor: "c5", live: true }));
    await settle(20);
    expect(leader.live.state).toBe("live");
    const joiner = tab();
    await settle(20);
    expect([...joiner.live.presence.entries.keys()]).toEqual(["p1"]);
    expect(joiner.live.presence.synced).toBe(true);
    expect(joiner.live.operations.cursor).toBe("c5");
    expect(joiner.live.operations.live).toBe(true);
    expect(joiner.live.state).toBe("live");
    leader.live.close();
    joiner.live.close();
  });

  it("when the leader goes, the next holder resumes from the last cursor it saw", async () => {
    const { streams, tab } = browser();
    const leader = tab();
    const follower = tab();
    await settle();
    streams.lastOperations().push(operationsFrame({ cursor: "c7", live: true }));
    await settle(20);
    leader.live.close();
    await settle(20);
    expect(follower.live.leading).toBe(true);
    expect(streams.operationCalls).toHaveLength(2);
    expect(streams.operationCalls[1]!.request).toEqual({ start: { case: "cursor", value: "c7" } });
    expect(streams.presenceCalls).toHaveLength(2);
    follower.live.close();
  });

  it("each tab opens its own streams without navigator.locks", async () => {
    const streams = new FakeStreams();
    const tabs = [0, 1].map(() => {
      const live = new Live(streams.connection(), environment(manualFrames().frame));
      live.start();
      return live;
    });
    await settle();
    expect(streams.operationCalls).toHaveLength(2);
    expect(tabs.every((live) => live.leading)).toBe(true);
    for (const live of tabs) live.close();
  });

  it("answers a hello only to the tab that sent it", async () => {
    const { streams, tab } = browser();
    const leader = tab();
    const old = tab();
    await settle();
    streams.lastPresence().push(presenceFrame([{ id: "p1", repoId: "r" }], true));
    await settle(20);
    // The leader resubscribes after a drop: p1 is not in the new set so far.
    streams.lastPresence().fail(new Error("dropped"));
    await settle(20);
    streams.lastPresence().push(presenceFrame([{ id: "p2", repoId: "r" }]));
    await settle(20);
    const joiner = tab();
    await settle(20);
    // The joiner gets only what the new set has sent; the old follower is untouched by the answer.
    expect([...joiner.live.presence.entries.keys()]).toEqual(["p2"]);
    expect(joiner.live.presence.synced).toBe(false);
    streams.lastPresence().push(presenceFrame([], true));
    await settle(20);
    for (const t of [leader, old, joiner]) {
      expect([...t.live.presence.entries.keys()]).toEqual(["p2"]);
      t.live.close();
    }
  });

  it("a follower whose own token is valid takes over when the leader's session ends", async () => {
    const { streams, tab } = browser();
    let leaderRefused = false;
    const leader = tab(manualFrames(), () => leaderRefused);
    const follower = tab();
    await settle();
    streams.lastOperations().push(operationsFrame({ cursor: "c3", live: true }));
    await settle(20);
    leaderRefused = true;
    streams.operationCalls[0]!.stream.fail(sessionInvalidError());
    await settle(40);
    expect(leader.ended.value).toBe(true);
    expect(follower.ended.value).toBe(false);
    expect(follower.live.leading).toBe(true);
    expect(streams.operationCalls.at(-1)!.request).toEqual({
      start: { case: "cursor", value: "c3" },
    });
    follower.live.close();
  });

  it("a follower whose token is refused too ends with the leader", async () => {
    const { streams, tab } = browser();
    const leader = tab(manualFrames(), () => true);
    const follower = tab(manualFrames(), () => true);
    await settle();
    streams.lastOperations().fail(sessionInvalidError());
    await settle(40);
    expect(leader.ended.value).toBe(true);
    expect(follower.ended.value).toBe(true);
  });

  it("a follower drops frames it already has, as a new leader resuming from behind sends them", async () => {
    const { tab, bus } = browser();
    const leader = tab();
    const follower = tab();
    await settle();
    const ops: string[] = [];
    follower.live.operations.subscribe((batch) => ops.push(...batch.operations.map((o) => o.id)));
    // What two leaders in turn post: the second resumed from before c2.
    const sender = bus.channel();
    const post = (cursor: string, id: string) => {
      sender.postMessage({
        t: "operations",
        frame: toBinary(
          SubscribeOperationsResponseSchema,
          operationsFrame({ cursor, operations: [operation(id, "r", 1)] }),
        ),
      });
    };
    post("c2", "a");
    post("c2", "a");
    post("c3", "b");
    await settle(20);
    follower.frames.flush();
    expect(ops).toEqual(["a", "b"]);
    expect(follower.live.operations.cursor).toBe("c3");
    sender.close();
    leader.live.close();
    follower.live.close();
  });
});
