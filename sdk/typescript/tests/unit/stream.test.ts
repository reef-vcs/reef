// The Stream wrapper's edge cases (review findings): one call however next() is called, no
// listener on a caller's signal until the stream starts, and cancellation.

import { describe, expect, it } from "vitest";

import { Stream } from "../../src/stream.js";

async function* three(): AsyncGenerator<number> {
  yield 1;
  yield 2;
  yield 3;
}

describe("stream", () => {
  it("opens the call once for concurrent next() calls", async () => {
    let opened = 0;
    const stream = new Stream(
      () => {
        opened += 1;
        return Promise.resolve(three());
      },
      (e) => Promise.resolve(e),
    );
    const results = await Promise.all([stream.next(), stream.next()]);
    expect(opened).toBe(1);
    expect(results.map((r) => r.value).sort()).toEqual([1, 2]);
  });

  it("adds no listener to the caller's signal until it starts, and removes it after", async () => {
    const controller = new AbortController();
    let added = 0;
    let removed = 0;
    const signal = controller.signal;
    const add = signal.addEventListener.bind(signal);
    const remove = signal.removeEventListener.bind(signal);
    signal.addEventListener = (...args: Parameters<typeof add>) => {
      added += 1;
      add(...args);
    };
    signal.removeEventListener = (...args: Parameters<typeof remove>) => {
      removed += 1;
      remove(...args);
    };
    const stream = new Stream(
      () => Promise.resolve(three()),
      (e) => Promise.resolve(e),
      signal,
    );
    expect(added).toBe(0);
    const values: number[] = [];
    for await (const value of stream) {
      values.push(value);
    }
    expect(values).toEqual([1, 2, 3]);
    expect(added).toBe(1);
    expect(removed).toBeGreaterThanOrEqual(1);
  });

  it("ends on break, on cancel and on the signal, without throwing", async () => {
    const broken = new Stream(
      () => Promise.resolve(three()),
      (e) => Promise.resolve(e),
    );
    for await (const value of broken) {
      expect(value).toBe(1);
      break;
    }
    expect(broken.cancelled).toBe(true);
    expect((await broken.next()).done).toBe(true);

    const controller = new AbortController();
    controller.abort();
    let opened = false;
    const aborted = new Stream(
      () => {
        opened = true;
        return Promise.resolve(three());
      },
      (e) => Promise.resolve(e),
      controller.signal,
    );
    expect((await aborted.next()).done).toBe(true);
    expect(opened).toBe(false);
  });

  it("maps a failure, and not one after cancel", async () => {
    const failing = new Stream<number>(
      () => Promise.reject(new Error("boom")),
      (e) => Promise.resolve(new Error(`mapped ${(e as Error).message}`)),
    );
    await expect(failing.next()).rejects.toThrow("mapped boom");
    expect((await failing.next()).done).toBe(true);
  });
});
