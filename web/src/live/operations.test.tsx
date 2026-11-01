// W4: the operation stream delivers in order, one batch per animation frame;
// resumes from the last cursor after a drop; starts over on TOO_FAR_BEHIND
// and OPERATION_LOG_REWRITTEN; and ends the session on WEB_SESSION_INVALID.
import { Code, ConnectError } from "@connectrpc/connect";
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import { SESSION_KEY } from "../session";
import {
  FakeStreams,
  daemonError,
  environment,
  manualFrames,
  operation,
  operationsFrame,
  sessionInvalid,
  settle,
} from "../test/fakes";
import { Backoff, sleep } from "./backoff";
import { OperationStore, PENDING_LIMIT, runOperations, type OperationBatch } from "./operations";

afterEach(() => {
  cleanup();
  localStorage.clear();
  vi.useRealTimers();
});

function collect(store: OperationStore): OperationBatch[] {
  const batches: OperationBatch[] = [];
  store.subscribe((batch) => batches.push(batch));
  return batches;
}

describe("OperationStore", () => {
  it("delivers frames in order, coalesced per animation frame", () => {
    const frames = manualFrames();
    const store = new OperationStore(frames.frame);
    const batches = collect(store);
    store.applyFrame(operationsFrame({ operations: [operation("a", "r", 1)], cursor: "c1" }));
    store.applyFrame(operationsFrame({ operations: [operation("b", "r", 2)], cursor: "c2" }));
    store.applyFrame(
      operationsFrame({ operations: [operation("c", "r", 3)], cursor: "c3", live: true }),
    );
    expect(frames.pending()).toBe(1);
    expect(batches).toHaveLength(0);
    frames.flush();
    expect(batches).toHaveLength(1);
    expect(batches[0]!.operations.map((o) => o.id)).toEqual(["a", "b", "c"]);
    expect(batches[0]!.live).toBe(true);
    expect(store.cursor).toBe("c3");
  });

  it("a keepalive schedules nothing", () => {
    const frames = manualFrames();
    const store = new OperationStore(frames.frame);
    store.applyFrame(operationsFrame({ live: true, cursor: "c1" }));
    frames.flush();
    store.applyFrame(operationsFrame({ live: true, cursor: "c2" }));
    expect(frames.pending()).toBe(0);
    expect(store.cursor).toBe("c2");
  });

  it("turns a backlog past the limit into a reset", () => {
    const frames = manualFrames();
    const store = new OperationStore(frames.frame);
    const batches = collect(store);
    const many = Array.from({ length: PENDING_LIMIT + 1 }, (_, i) => operation(`o${i}`, "r", i));
    store.applyFrame(operationsFrame({ operations: many, live: true }));
    frames.flush();
    expect(batches[0]!.reset).toBe(true);
    expect(batches[0]!.operations).toHaveLength(0);
  });
});

describe("runOperations", () => {
  function setup() {
    const frames = manualFrames();
    const store = new OperationStore(frames.frame);
    const opened: (string | undefined)[] = [];
    const streams = new FakeStreams();
    const controller = new AbortController();
    const onSessionInvalid = vi.fn();
    const onReset = vi.fn();
    const done = runOperations(
      (cursor, signal) => {
        opened.push(cursor);
        return streams
          .connection()
          .clients.operations.subscribeOperations(
            cursor === undefined ? {} : { start: { case: "cursor", value: cursor } },
            { signal },
          );
      },
      store,
      {
        signal: controller.signal,
        backoff: new Backoff(),
        sleep: async () => {
          await settle(1);
        },
        onSessionInvalid,
      },
      onReset,
    );
    return { frames, store, opened, streams, controller, onSessionInvalid, onReset, done };
  }

  it("resumes from the last cursor after UNAVAILABLE with no reason, delivering nothing twice", async () => {
    const t = setup();
    const batches = collect(t.store);
    await settle();
    t.streams
      .lastOperations()
      .push(
        operationsFrame({ operations: [operation("a", "r", 1)], cursor: "c1", live: true }),
        operationsFrame({ operations: [operation("b", "r", 2)], cursor: "c2" }),
      );
    await settle();
    t.streams.lastOperations().fail(new ConnectError("shutting down", Code.Unavailable));
    await settle();
    expect(t.opened).toEqual([undefined, "c2"]);
    // The daemon resumes after c2.
    t.streams
      .lastOperations()
      .push(operationsFrame({ operations: [operation("c", "r", 3)], cursor: "c3" }));
    await settle();
    t.frames.flush();
    expect(batches.flatMap((b) => b.operations.map((o) => o.id))).toEqual(["a", "b", "c"]);
    expect(batches.some((b) => b.reset)).toBe(false);
    t.controller.abort();
    await t.done;
  });

  it("resumes after a transport error", async () => {
    const t = setup();
    await settle();
    t.streams.lastOperations().push(operationsFrame({ cursor: "c9", live: true }));
    await settle();
    t.streams.lastOperations().fail(new TypeError("Failed to fetch"));
    await settle();
    expect(t.opened).toEqual([undefined, "c9"]);
    t.controller.abort();
    await t.done;
  });

  for (const reason of ["TOO_FAR_BEHIND", "OPERATION_LOG_REWRITTEN"]) {
    it(`subscribes fresh and emits reset on ${reason}`, async () => {
      const t = setup();
      const batches = collect(t.store);
      await settle();
      t.streams.lastOperations().push(operationsFrame({ cursor: "c1", live: true }));
      await settle();
      t.streams.lastOperations().fail(daemonError(Code.ResourceExhausted, { reason }));
      await settle();
      expect(t.opened).toEqual([undefined, undefined]);
      expect(t.onReset).toHaveBeenCalledOnce();
      t.frames.flush();
      expect(batches.at(-1)!.reset).toBe(true);
      expect(t.store.live).toBe(false);
      t.controller.abort();
      await t.done;
    });
  }

  // W10: a token over its stream limit (REMOTE_BUSY) is retried, not ended.
  it("retries on REMOTE_BUSY", async () => {
    const t = setup();
    await settle();
    t.streams.lastOperations().fail(daemonError(Code.Unavailable, { reason: "REMOTE_BUSY" }));
    await settle();
    expect(t.onSessionInvalid).not.toHaveBeenCalled();
    expect(t.opened.length).toBeGreaterThan(1);
    t.controller.abort();
    await t.done;
  });

  it("stops on WEB_SESSION_INVALID", async () => {
    const t = setup();
    await settle();
    t.streams.lastOperations().fail(sessionInvalid());
    await t.done;
    expect(t.onSessionInvalid).toHaveBeenCalledOnce();
    expect(t.opened).toHaveLength(1);
  });
});

describe("backoff", () => {
  it("grows from 100 ms to 2 s and stays there", () => {
    const backoff = new Backoff();
    const delays = Array.from({ length: 8 }, () => backoff.next());
    expect(delays).toEqual([100, 200, 400, 800, 1600, 2000, 2000, 2000]);
    backoff.reset();
    expect(backoff.next()).toBe(100);
  });

  it("waits the delay between retries", async () => {
    vi.useFakeTimers();
    const store = new OperationStore(manualFrames().frame);
    const streams = new FakeStreams();
    const controller = new AbortController();
    let opens = 0;
    const done = runOperations(
      (cursor, signal) => {
        opens += 1;
        const stream = streams
          .connection()
          .clients.operations.subscribeOperations(
            cursor === undefined ? {} : { start: { case: "cursor", value: cursor } },
            { signal },
          );
        streams.lastOperations().fail(new ConnectError("down", Code.Unavailable));
        return stream;
      },
      store,
      { signal: controller.signal, backoff: new Backoff(), sleep, onSessionInvalid: vi.fn() },
    );
    await vi.advanceTimersByTimeAsync(0);
    expect(opens).toBe(1);
    await vi.advanceTimersByTimeAsync(99);
    expect(opens).toBe(1);
    await vi.advanceTimersByTimeAsync(1);
    expect(opens).toBe(2);
    await vi.advanceTimersByTimeAsync(200);
    expect(opens).toBe(3);
    await vi.advanceTimersByTimeAsync(400 + 800 + 1600);
    expect(opens).toBe(6);
    await vi.advanceTimersByTimeAsync(2000);
    expect(opens).toBe(7);
    controller.abort();
    await done;
  });
});

describe("a refused session", () => {
  it("ends the live layer, clears the stored token and shows the no-session page", async () => {
    localStorage.setItem(SESSION_KEY, "tok");
    const streams = new FakeStreams();
    const connection = streams.connection();
    Object.assign(connection.clients, {
      daemon: { getDaemonStatus: () => new Promise(() => undefined) },
      repositories: { listRepositories: () => new Promise(() => undefined) },
    });
    const frames = manualFrames();
    render(<App session={{ connection, token: "tok" }} environment={environment(frames.frame)} />);
    await act(async () => {
      await settle();
      streams.lastOperations().fail(sessionInvalid());
      await settle();
    });
    expect(screen.getByText(/This page has no session with the daemon/)).toBeTruthy();
    expect(localStorage.getItem(SESSION_KEY)).toBeNull();
  });
});
