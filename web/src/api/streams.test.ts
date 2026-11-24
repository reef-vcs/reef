// When what reads over a connection is told that its streams are live again
// (R3.40): not the first time, and a hidden page only once it is shown.
import { afterEach, describe, expect, it, vi } from "vitest";
import { Streams } from "./streams";

afterEach(() => {
  vi.restoreAllMocks();
});

function told() {
  const streams = new Streams();
  const resumed = vi.fn();
  const stop = streams.onResume(resumed);
  return { streams, stop, times: () => resumed.mock.calls.length };
}

const hide = (state: DocumentVisibilityState) => {
  vi.spyOn(document, "visibilityState", "get").mockReturnValue(state);
  document.dispatchEvent(new Event("visibilitychange"));
};

describe("streams", () => {
  it("tell that they are live again after a loss, and not the first time", () => {
    const { streams, times, stop } = told();
    streams.change(false);
    expect(times()).toBe(0);
    streams.change(true);
    expect(times()).toBe(0);
    streams.change(false);
    streams.change(true);
    expect(times()).toBe(1);
    stop();
    streams.change(false);
    streams.change(true);
    expect(times()).toBe(1);
  });

  it("tell a hidden page that they are again once it is shown, and once", () => {
    const { streams, times } = told();
    streams.change(true);
    hide("hidden");
    for (let i = 0; i < 3; i++) {
      streams.change(false);
      streams.change(true);
    }
    expect(times()).toBe(0);
    hide("hidden");
    expect(times()).toBe(0);
    hide("visible");
    expect(times()).toBe(1);
    hide("visible");
    expect(times()).toBe(1);
  });

  it("tell a page shown while they are lost once they are live", () => {
    const { streams, times } = told();
    streams.change(true);
    hide("hidden");
    streams.change(false);
    streams.change(true);
    streams.change(false);
    hide("visible");
    expect(times()).toBe(0);
    streams.change(true);
    expect(times()).toBe(1);
  });
});
