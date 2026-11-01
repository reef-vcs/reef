// The design pass: the one-minute clock ticks at minute boundaries, only
// while read, and through `after` (setTimeout), never setInterval.
import { act, cleanup, render } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { useNow } from "./clock";

function Reader({ onRender }: { onRender: (now: number) => void }) {
  onRender(useNow());
  return null;
}

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

describe("useNow", () => {
  it("ticks at the next minute boundary, then each minute", () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "setInterval", "Date"] });
    vi.setSystemTime(Date.UTC(2026, 10, 1, 12, 0, 30));
    const interval = vi.spyOn(globalThis, "setInterval");
    const seen: number[] = [];
    render(<Reader onRender={(now) => seen.push(now)} />);
    expect(seen.at(-1)).toBe(Date.UTC(2026, 10, 1, 12, 0, 30));
    act(() => {
      vi.advanceTimersByTime(29_000);
    });
    expect(seen.at(-1)).toBe(Date.UTC(2026, 10, 1, 12, 0, 30));
    act(() => {
      vi.advanceTimersByTime(1_100);
    });
    expect(new Date(seen.at(-1)!).getUTCMinutes()).toBe(1);
    act(() => {
      vi.advanceTimersByTime(60_000);
    });
    expect(new Date(seen.at(-1)!).getUTCMinutes()).toBe(2);
    expect(interval).not.toHaveBeenCalled();
  });

  it("schedules nothing once nothing reads it", () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const { unmount } = render(<Reader onRender={() => undefined} />);
    expect(vi.getTimerCount()).toBe(1);
    unmount();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("ticks at once when the page becomes visible", () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    vi.setSystemTime(Date.UTC(2026, 10, 1, 12, 0, 0));
    const seen: number[] = [];
    render(<Reader onRender={(now) => seen.push(now)} />);
    vi.setSystemTime(Date.UTC(2026, 10, 1, 12, 0, 20));
    act(() => {
      document.dispatchEvent(new Event("visibilitychange"));
    });
    expect(seen.at(-1)).toBe(Date.UTC(2026, 10, 1, 12, 0, 20));
  });
});
