// R2.21 test 5: the window over an in-memory log and a hand-fed stream.
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { afterEach, describe, expect, it, vi } from "vitest";
import { OperationStore } from "../live/operations";
import { Code } from "@connectrpc/connect";
import { PENDING_LIMIT } from "../live/operations";
import {
  daemonError,
  immediateFrames,
  manualFrames,
  operationsFrame,
  sessionInvalid,
  settle,
} from "../test/fakes";
import { FakeOplog, REPO, T } from "../test/oplog-daemon";
import { OperationWindow, WINDOW_CAP } from "./window";

const SEC = 1000;

function grow(oplog: FakeOplog, count: number, from = T): Operation[] {
  const out: Operation[] = [];
  for (let i = 0; i < count; i++) {
    out.push(oplog.add({ endedAt: from + i * SEC, workspaceId: "w1", agent: "ann" }));
  }
  return out;
}

function setup(oplog: FakeOplog = new FakeOplog(), { live = true } = {}) {
  const store = new OperationStore(immediateFrames);
  let cursor = 0;
  const push = (operations: Operation[], isLive = true) => {
    cursor += 1;
    store.applyFrame(operationsFrame({ operations, live: isLive, cursor: `c${String(cursor)}` }));
  };
  if (live) push([]);
  const onSessionInvalid = vi.fn();
  const window = new OperationWindow(
    oplog.connection(),
    store,
    immediateFrames,
    onSessionInvalid,
    REPO,
  );
  window.start();
  return { oplog, store, window, push, onSessionInvalid, snap: () => window.getSnapshot() };
}

afterEach(() => {
  vi.useRealTimers();
});

const ids = (records: { id: string }[] | null) => (records ?? []).map((r) => r.id);
const quiet = () => settle(200);

describe("reads", () => {
  it("lists nothing before the stream is live, then one page of 1,000 without effects", async () => {
    const t = setup(new FakeOplog(), { live: false });
    grow(t.oplog, 3);
    await quiet();
    expect(t.oplog.listCalls).toEqual([]);
    t.push([], true);
    await quiet();
    expect(t.oplog.listCalls).toHaveLength(1);
    expect(t.oplog.listCalls[0]).toMatchObject({ repo: REPO, pageSize: 1000, pageToken: "" });
    expect(t.oplog.listCalls[0]?.includeEffects).toBe(false);
    expect(ids(t.snap().records)).toHaveLength(3);
  });

  it("marks the root and never holds it", async () => {
    const t = setup();
    grow(t.oplog, 2);
    await quiet();
    expect(t.snap().reachedRoot).toBe(true);
    expect(ids(t.snap().records)).not.toContain(t.oplog.rootId);
    expect(t.snap().more).toBe(false);
  });
});

describe("the join", () => {
  it("merges operations that arrive during the list, once each, and ignores other repositories", async () => {
    const oplog = new FakeOplog();
    const before = grow(oplog, 3);
    let release: () => void = () => undefined;
    oplog.hold = new Promise((resolve) => {
      release = resolve;
    });
    const t = setup(oplog);
    await quiet();
    const during = grow(oplog, 2, T + 100 * SEC);
    const other = { ...during[0], repoId: "elsewhere", id: "x".repeat(16) } as Operation;
    // The stream repeats the last listed operation and brings the new ones.
    t.push([before[2] as Operation, ...during, other]);
    release();
    await quiet();
    expect(ids(t.snap().records)).toEqual([...before, ...during].map((o) => o.id).reverse());
  });

  it("applies later operations at once, and a repeated frame changes nothing", async () => {
    const t = setup();
    grow(t.oplog, 2);
    await quiet();
    const [next] = grow(t.oplog, 1, T + 10 * SEC);
    t.push([next as Operation]);
    t.push([next as Operation]);
    expect(t.snap().records?.[0]?.id).toBe(next?.id);
    expect(t.snap().records).toHaveLength(3);
  });
});

describe("older pages", () => {
  it("follow the pinned token, one in flight", async () => {
    const oplog = new FakeOplog();
    grow(oplog, 2500);
    const t = setup(oplog);
    await quiet();
    expect(t.snap().records).toHaveLength(1000);
    expect(t.snap().more).toBe(true);
    void t.window.loadOlder();
    void t.window.loadOlder();
    await quiet();
    expect(oplog.listCalls).toHaveLength(2);
    expect(oplog.listCalls[1]?.pageToken).not.toBe("");
    expect(t.snap().records).toHaveLength(2000);
    await t.window.loadOlder();
    await quiet();
    expect(t.snap().records).toHaveLength(2500);
    expect(t.snap().reachedRoot).toBe(true);
    expect(t.snap().more).toBe(false);
  });

  it("stop at the cap, which then slides from the bottom", async () => {
    const oplog = new FakeOplog();
    grow(oplog, WINDOW_CAP + 500);
    const t = setup(oplog);
    await quiet();
    while (t.snap().more) {
      await t.window.loadOlder();
      await quiet();
    }
    expect(t.snap().records).toHaveLength(WINDOW_CAP);
    expect(t.snap().capped).toBe(true);
    expect(t.snap().reachedRoot).toBe(false);
    const bottom = t.snap().records?.at(-1)?.id;
    t.push(grow(oplog, 3, T + 20_000 * SEC));
    expect(t.snap().records).toHaveLength(WINDOW_CAP);
    expect(t.snap().records?.at(-1)?.id).not.toBe(bottom);
  });
});

describe("starting over", () => {
  it("reloads after a reset down to the old bottom's time", async () => {
    const oplog = new FakeOplog();
    grow(oplog, 1500);
    const t = setup(oplog);
    await quiet();
    await t.window.loadOlder();
    await quiet();
    expect(t.snap().records).toHaveLength(1500);
    t.store.reset();
    // Recorded while the stream was starting over: never delivered.
    grow(oplog, 2, T + 5000 * SEC);
    t.push([], true);
    await quiet();
    expect(t.snap().note?.kind).toBe("reset");
    expect(ids(t.snap().records)).toEqual(
      oplog
        .walk()
        .filter((o) => o.id !== oplog.rootId)
        .map((o) => o.id),
    );
  });

  it("starts over on the first operation after a GC", async () => {
    const oplog = new FakeOplog();
    grow(oplog, 5);
    const t = setup(oplog);
    await quiet();
    const before = ids(t.snap().records);
    oplog.gc();
    const [after] = grow(oplog, 1, T + 100 * SEC);
    t.push([after as Operation]);
    await quiet();
    expect(t.snap().note?.kind).toBe("gc");
    const now = ids(t.snap().records);
    expect(now).toHaveLength(6);
    expect(now.filter((id) => before.includes(id))).toEqual([]);
  });

  it("starts over when an older page meets a GC with no operation since", async () => {
    const oplog = new FakeOplog();
    grow(oplog, 1500);
    const t = setup(oplog);
    await quiet();
    oplog.gc();
    await t.window.loadOlder();
    await quiet();
    expect(t.snap().note?.kind).toBe("gc");
    expect(t.snap().records).toHaveLength(1000);
    expect(t.snap().records?.[0]?.id.endsWith("g1")).toBe(true);
  });

  it("never starts over for an operation whose parents are held", async () => {
    const t = setup();
    grow(t.oplog, 3);
    await quiet();
    t.push(grow(t.oplog, 2, T + 10 * SEC));
    await quiet();
    expect(t.snap().note).toBeNull();
    expect(t.oplog.listCalls).toHaveLength(1);
  });
});

describe("holding", () => {
  it("holds new operations while on and applies them on show", async () => {
    const t = setup();
    grow(t.oplog, 2);
    await quiet();
    t.window.setHolding(true);
    const fresh = grow(t.oplog, 2, T + 10 * SEC);
    t.push(fresh);
    expect([...t.snap().held]).toEqual(fresh.map((o) => o.id));
    expect(t.snap().records).toHaveLength(4);
    t.window.showHeld();
    expect(t.snap().held.size).toBe(0);
    // A replayed frame of operations already shown holds nothing.
    t.push(fresh);
    expect(t.snap().held.size).toBe(0);
  });
});

describe("points", () => {
  it("resolve a prefix inside the window", async () => {
    const t = setup();
    const [first] = grow(t.oplog, 3);
    await quiet();
    // Held by the window: inside at once.
    const point = t.window.point(first?.id.slice(0, 8) ?? "");
    expect(point.state).toBe("inside");
    expect(point.state === "inside" && point.id).toBe(first?.id);
  });

  it("load older pages to reach a point, and detach one past the cap", async () => {
    const oplog = new FakeOplog();
    const all = grow(oplog, WINDOW_CAP + 1500);
    const t = setup(oplog);
    await quiet();
    const near = all[all.length - 2500] as Operation;
    t.window.locatePoint(near.id);
    await settle(2000);
    expect(t.window.point(near.id).state).toBe("inside");
    const far = all[10] as Operation;
    t.window.locatePoint(far.id);
    await settle(4000);
    const point = t.window.point(far.id);
    expect(point.state).toBe("detached");
    expect(point.state === "detached" && point.records[0]?.id).toBe(far.id);
    expect(oplog.listCalls.at(-1)?.atOperation).toBe(far.id);
  });

  it("give the feed at a point as its ancestors in the window", async () => {
    const oplog = new FakeOplog();
    const base = grow(oplog, 3);
    // A concurrent head beside the main line.
    const side = oplog.add({ endedAt: T + 50 * SEC, kind: OperationKind.EXTERNAL }, [
      base[1]?.id ?? "",
    ]);
    const t = setup(oplog);
    await quiet();
    expect(ids(t.window.ancestors(base[2]?.id ?? ""))).toEqual(
      [base[2], base[1], base[0]].map((o) => o?.id),
    );
    expect(ids(t.window.ancestors(side.id))).toEqual([side, base[1], base[0]].map((o) => o?.id));
  });
});

describe("errors", () => {
  it("ends the session on WEB_SESSION_INVALID", async () => {
    const oplog = new FakeOplog();
    oplog.failList = sessionInvalid();
    const t = setup(oplog);
    await quiet();
    expect(t.onSessionInvalid).toHaveBeenCalled();
  });
});

describe("review fixes", () => {
  it("resolves a point looked up before the first list answers, once it does", async () => {
    const oplog = new FakeOplog();
    const all = grow(oplog, 10);
    let release: () => void = () => undefined;
    oplog.hold = new Promise((resolve) => {
      release = resolve;
    });
    const t = setup(oplog);
    // The list is in flight (held): the repository is known, nothing is listed yet.
    while (oplog.listCalls.length === 0) await settle(20);
    expect(t.window.repositoryId).toBe(REPO);
    const at = all[3]?.id ?? "";
    t.window.locatePoint(at);
    await quiet();
    expect(t.window.point(at).state).toBe("loading");
    release();
    await quiet();
    t.window.locatePoint(at);
    const point = t.window.point(at);
    expect(point.state === "inside" && point.id).toBe(at);
    // Inside the window: no list at the point.
    expect(oplog.listCalls.filter((c) => c.atOperation)).toEqual([]);
  });

  it("resolves an id the window holds with no read, and fetches the banner's operation after", async () => {
    const t = setup();
    const all = grow(t.oplog, 5);
    await quiet();
    const at = all[2]?.id.slice(0, 10) ?? "";
    const point = t.window.point(at);
    expect(point).toMatchObject({ state: "inside", id: all[2]?.id, operation: undefined });
    expect(t.oplog.getCalls).toEqual([]);
    t.window.locatePoint(at);
    await quiet();
    const filled = t.window.point(at);
    expect(filled.state === "inside" && filled.operation?.id).toBe(all[2]?.id);
  });

  it("does not page older for a pre-GC point, and says so with the daemon's reason", async () => {
    const oplog = new FakeOplog();
    const all = grow(oplog, 1500);
    const t = setup(oplog);
    await quiet();
    const old = all[1400]?.id ?? "";
    oplog.gc();
    t.push(grow(oplog, 1, T + 5000 * SEC));
    await quiet();
    expect(t.snap().note?.kind).toBe("gc");
    const lists = oplog.listCalls.length;
    t.window.locatePoint(old);
    await quiet();
    const point = t.window.point(old);
    expect(point.state === "error" && point.error.reason).toBe("OPERATION_LOG_REWRITTEN");
    // One list at the point, and no older pages.
    expect(oplog.listCalls.slice(lists).map((c) => c.atOperation)).toEqual([old]);
  });

  it("forgets the root once the cap drops the bottom", async () => {
    const oplog = new FakeOplog();
    grow(oplog, WINDOW_CAP - 5);
    const t = setup(oplog);
    await quiet();
    while (t.snap().more) {
      await t.window.loadOlder();
      await quiet();
    }
    expect(t.snap().reachedRoot).toBe(true);
    t.push(grow(oplog, 20, T + 20_000 * SEC));
    await quiet();
    expect(t.snap().records).toHaveLength(WINDOW_CAP);
    expect(t.snap().reachedRoot).toBe(false);
    expect(t.snap().capped).toBe(true);
  });

  it("takes a concurrent operation whose parent is below the bottom, without starting over", async () => {
    const oplog = new FakeOplog();
    const all = grow(oplog, 1500);
    const t = setup(oplog);
    await quiet();
    const stale = oplog.add({ endedAt: T + 3000 * SEC, kind: OperationKind.EXTERNAL }, [
      all[100]?.id ?? "",
    ]);
    t.push([stale]);
    await quiet();
    expect(t.snap().note).toBeNull();
    expect(t.snap().records?.[0]?.id).toBe(stale.id);
    // No reload from the head; one read of the parent.
    expect(oplog.listCalls.filter((c) => !c.atOperation)).toHaveLength(1);
    expect(oplog.listCalls.filter((c) => c.atOperation).map((c) => c.atOperation)).toEqual([
      all[100]?.id,
    ]);
  });

  it("retries a failed list after the backoff, not on every batch", async () => {
    const oplog = new FakeOplog();
    grow(oplog, 3);
    oplog.failList = daemonError(Code.Unavailable, { hint: "busy" });
    vi.useFakeTimers();
    const t = setup(oplog);
    await quiet();
    expect(oplog.listCalls).toHaveLength(1);
    expect(t.snap().error?.hint).toBe("busy");
    t.push(grow(oplog, 1, T + 10 * SEC));
    t.push(grow(oplog, 1, T + 11 * SEC));
    await quiet();
    expect(oplog.listCalls).toHaveLength(1);
    oplog.failList = undefined;
    vi.advanceTimersByTime(100);
    await quiet();
    expect(oplog.listCalls).toHaveLength(2);
    expect(t.snap().records).toHaveLength(5);
    expect(t.snap().error).toBeNull();
  });

  it("starts over when a hidden tab's batch passes the store's limit", async () => {
    const oplog = new FakeOplog();
    grow(oplog, 3);
    const frames = manualFrames();
    const store = new OperationStore(frames.frame);
    store.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
    frames.flush();
    const window = new OperationWindow(
      oplog.connection(),
      store,
      immediateFrames,
      () => undefined,
      REPO,
    );
    window.start();
    await quiet();
    expect(window.getSnapshot().records).toHaveLength(3);
    // No frames run while hidden; the store passes its limit.
    const burst = grow(oplog, PENDING_LIMIT + 1, T + 100 * SEC);
    store.applyFrame(operationsFrame({ operations: burst, live: true, cursor: "c1" }));
    frames.flush();
    await quiet();
    expect(window.getSnapshot().note?.kind).toBe("reset");
    // The reload goes down to the old bottom's time: here, the cap.
    for (let i = 0; i < 50 && window.getSnapshot().reloading; i++) await settle(200);
    expect(window.getSnapshot().records).toHaveLength(WINDOW_CAP);
  });
});
