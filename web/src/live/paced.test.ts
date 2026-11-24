// R2.20 test 5: the pacing rule the smartlog's model and the workspaces
// page share: one run in flight, one more for requests during it, spaced at
// max(250 ms, 4 × the last run) from the last start, at most 10 s after its
// end (R3.40), and unspaced on request; and again by itself, backing off,
// after a run no answer came to.
import { afterEach, describe, expect, it, vi } from "vitest";
import { settle } from "../test/fakes";
import { Backoff } from "./backoff";
import { MAX_SPACING_MS, MIN_SPACING_MS, Paced } from "./paced";

afterEach(() => {
  vi.useRealTimers();
});

/** A run the test resolves, counting starts. */
function runs() {
  const state = { starts: 0, resolve: [] as ((answered: boolean) => void)[], skip: false };
  const run = () => {
    if (state.skip) return undefined;
    state.starts += 1;
    return new Promise<boolean>((resolve) => {
      state.resolve.push(resolve);
    });
  };
  const finish = async (answered = true) => {
    state.resolve.shift()?.(answered);
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

  it("waits 10 s at most after a run, though it hung or the device slept for an hour", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    for (const ms of [3_000, 60_000, 3_600_000]) {
      const { state, run, finish } = runs();
      const paced = new Paced(run);
      paced.request();
      vi.setSystemTime(Date.now() + ms);
      await finish();
      paced.request();
      const wait = Math.min(3 * ms, MAX_SPACING_MS);
      vi.advanceTimersByTime(wait - 1);
      expect(state.starts).toBe(1);
      vi.advanceTimersByTime(1);
      expect(state.starts).toBe(2);
    }
  });

  it("counts those 10 s from the run's end", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    vi.setSystemTime(Date.now() + 3_600_000);
    await finish();
    vi.advanceTimersByTime(MAX_SPACING_MS - 1_000);
    paced.request();
    vi.advanceTimersByTime(1_000);
    expect(state.starts).toBe(2);
  });

  it("waits 10 s at most when the clock was set back", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    await finish();
    vi.setSystemTime(Date.now() - 3_600_000);
    paced.request();
    vi.advanceTimersByTime(MAX_SPACING_MS);
    expect(state.starts).toBe(2);
  });

  it("runs again after a run no answer came to, backing off, whatever it took", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    vi.setSystemTime(Date.now() + 3_600_000);
    for (const wait of [100, 200, 400, 800, 1_600, Backoff.MAX_MS, Backoff.MAX_MS]) {
      const before = state.starts;
      paced.request();
      await finish(false);
      paced.request();
      vi.advanceTimersByTime(wait - 1);
      expect(state.starts).toBe(before);
      vi.advanceTimersByTime(1);
      expect(state.starts).toBe(before + 1);
    }
    await finish();
    vi.advanceTimersByTime(60_000);
    expect(state.starts).toBe(8);
    // And backs off from the start the next time.
    paced.request();
    await finish(false);
    vi.advanceTimersByTime(Backoff.FIRST_MS);
    expect(state.starts).toBe(10);
  });

  it("runs at once when asked to after a run no answer came to, and once", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    paced.requestNow();
    await finish(false);
    expect(state.starts).toBe(2);
    await finish(false);
    paced.requestNow();
    expect(state.starts).toBe(3);
    await finish();
    vi.advanceTimersByTime(60_000);
    expect(state.starts).toBe(3);
  });

  it("does not run again after stop, though no answer came", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { state, run, finish } = runs();
    const paced = new Paced(run);
    paced.request();
    paced.stop();
    await finish(false);
    expect(vi.getTimerCount()).toBe(0);
    vi.advanceTimersByTime(60_000);
    expect(state.starts).toBe(1);
  });
});
