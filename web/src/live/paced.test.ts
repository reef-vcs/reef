// R2.20 test 5: the pacing rule the smartlog's model and the workspaces
// page share: one run in flight, one more for requests during it, spaced at
// max(250 ms, 4 × the last run) from the last start, and unspaced on request.
import { afterEach, describe, expect, it, vi } from "vitest";
import { settle } from "../test/fakes";
import { MIN_SPACING_MS, Paced } from "./paced";

afterEach(() => {
  vi.useRealTimers();
});

/** A run the test resolves, counting starts. */
function runs() {
  const state = { starts: 0, resolve: [] as (() => void)[], skip: false };
  const run = () => {
    if (state.skip) return undefined;
    state.starts += 1;
    return new Promise<void>((resolve) => {
      state.resolve.push(resolve);
    });
  };
  const finish = async () => {
    state.resolve.shift()?.();
    await settle();
  };
  return { state, run, finish };
}

describe("Paced", () => {
  it("runs at once, then holds requests during a run to exactly one more", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    expect(state.starts).toBe(1);
    expect(paced.busy).toBe(true);
    paced.request();
    paced.request();
    paced.request();
    expect(state.starts).toBe(1);
    await finish();
    // The trailing run waits out the spacing from the first start.
    expect(state.starts).toBe(1);
    vi.advanceTimersByTime(MIN_SPACING_MS);
    expect(state.starts).toBe(2);
    await finish();
    vi.advanceTimersByTime(10_000);
    expect(state.starts).toBe(2);
  });

  it("spaces runs by four times the last run's duration when that is longer", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    vi.advanceTimersByTime(200);
    await finish();
    paced.request();
    // 800 ms after the first start, not 250.
    vi.advanceTimersByTime(599);
    expect(state.starts).toBe(1);
    vi.advanceTimersByTime(1);
    expect(state.starts).toBe(2);
  });

  it("runs a requestNow unspaced, and right after the one in flight", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    await finish();
    paced.request();
    expect(state.starts).toBe(1);
    paced.requestNow();
    expect(state.starts).toBe(2);
    paced.requestNow();
    expect(state.starts).toBe(2);
    await finish();
    // The user's request does not wait out the spacing.
    expect(state.starts).toBe(3);
    paced.request();
    await finish();
    vi.advanceTimersByTime(MIN_SPACING_MS);
    expect(state.starts).toBe(4);
  });

  it("starts a spaced run through its gate", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const gated: (() => void)[] = [];
    const paced = new Paced(run, Date.now, (start) => gated.push(start));
    paced.request();
    await finish();
    paced.request();
    vi.advanceTimersByTime(MIN_SPACING_MS);
    // A hidden tab gives no frame: nothing starts.
    expect(state.starts).toBe(1);
    gated.shift()?.();
    expect(state.starts).toBe(2);
  });

  it("records no start for a run that did not start, and nothing after stop", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    state.skip = true;
    paced.request();
    expect(paced.busy).toBe(false);
    state.skip = false;
    // Not spaced: the skipped run recorded no start.
    paced.request();
    expect(state.starts).toBe(1);
    await finish();
    paced.request();
    paced.stop();
    vi.advanceTimersByTime(10_000);
    paced.requestNow();
    expect(state.starts).toBe(1);
  });
});
