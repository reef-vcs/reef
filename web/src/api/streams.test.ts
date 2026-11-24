// When what reads over a connection is told that its streams are live again
// (R3.40): not the first time, and a hidden page only once it is shown.
import { afterEach, describe, expect, it, vi } from "vitest";
import { Streams } from "./streams";

afterEach(() => {
  vi.restoreAllMocks();
});

function told() {
  const streams = new Streams();
  const live = vi.fn();
  const resumed = vi.fn();
  streams.onLive(live);
  const stop = streams.onResume(resumed);
  return {
    streams,
    live,
    resumed,
    stop,
    counts: () => [live, resumed].map((f) => f.mock.calls.length),
  };
}

const hide = (state: DocumentVisibilityState) => {
  vi.spyOn(document, "visibilityState", "get").mockReturnValue(state);
  document.dispatchEvent(new Event("visibilitychange"));
};

describe("streams", () => {
  it("tell that they are live each time, and that they are again after a loss", () => {
    const { streams, counts, stop } = told();
    streams.change(false);
    expect(counts()).toEqual([0, 0]);
    streams.change(true);
    expect(counts()).toEqual([1, 0]);
    streams.change(false);
    streams.change(true);
    expect(counts()).toEqual([2, 1]);
    stop();
    streams.change(false);
    streams.change(true);
    expect(counts()).toEqual([3, 1]);
  });

  it("tell a hidden page that they are again once it is shown, and once", () => {
    const { streams, counts } = told();
    streams.change(true);
    hide("hidden");
    for (let i = 0; i < 3; i++) {
      streams.change(false);
      streams.change(true);
    }
    expect(counts()).toEqual([4, 0]);
    hide("hidden");
    expect(counts()).toEqual([4, 0]);
    hide("visible");
    expect(counts()).toEqual([4, 1]);
    hide("visible");
    expect(counts()).toEqual([4, 1]);
  });

  it("tell a page shown while they are lost once they are live", () => {
    const { streams, counts } = told();
    streams.change(true);
    hide("hidden");
    streams.change(false);
    streams.change(true);
    streams.change(false);
    hide("visible");
    expect(counts()).toEqual([2, 0]);
    streams.change(true);
    expect(counts()).toEqual([3, 1]);
  });
});
