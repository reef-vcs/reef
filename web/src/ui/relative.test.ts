// The design pass: relative times, pure, against a fixed zone.
process.env.TZ = "UTC";
import { describe, expect, it } from "vitest";
import { ago, span, until } from "./relative";

const NOW = Date.UTC(2026, 10, 1, 12, 0, 0); // Sun 2026-11-01 12:00 UTC
const S = 1000;
const M = 60 * S;
const H = 60 * M;

describe("ago", () => {
  it("says just now under a minute, and for skew up to 2 minutes", () => {
    expect(ago(NOW, NOW)).toBe("just now");
    expect(ago(NOW - 59 * S, NOW)).toBe("just now");
    expect(ago(NOW + 2 * M, NOW)).toBe("just now");
  });

  it("floors minutes and hours", () => {
    expect(ago(NOW - 60 * S, NOW)).toBe("1 min ago");
    expect(ago(NOW - (59 * M + 59 * S), NOW)).toBe("59 min ago");
    expect(ago(NOW - 60 * M, NOW)).toBe("1 h ago");
    expect(ago(NOW - (11 * H + 59 * M), NOW)).toBe("11 h ago");
  });

  it("names yesterday, then the weekday, then the date", () => {
    expect(ago(Date.UTC(2026, 9, 31, 23, 59), NOW)).toBe("yesterday 23:59");
    expect(ago(Date.UTC(2026, 9, 31, 0, 1), NOW)).toBe("yesterday 00:01");
    expect(ago(NOW - 6 * 24 * H, NOW)).toBe("Mon 12:00");
    expect(ago(NOW - 7 * 24 * H, NOW)).toBe("Oct 25");
    // Last Sunday, later in the day than now: not a weekday, which would read as today.
    expect(ago(NOW - 6 * 24 * H - 23 * H, NOW)).toBe("Oct 25");
    expect(ago(Date.UTC(2025, 11, 31, 12), NOW)).toBe("2025-12-31");
  });

  it("reads a time further in the future than skew as its clock time", () => {
    expect(ago(NOW + 3 * H, NOW)).toBe("15:00");
  });
});

describe("until", () => {
  it("ceils minutes and hours", () => {
    expect(until(NOW + 30 * S, NOW)).toBe("in under a minute");
    expect(until(NOW + 61 * S, NOW)).toBe("in 2 min");
    expect(until(NOW + 60 * M, NOW)).toBe("in 1 h");
    expect(until(NOW + 23 * H + 1, NOW)).toBe("in 24 h");
    expect(until(NOW + 3 * 24 * H, NOW)).toBe("Nov 4");
  });
});

describe("span", () => {
  it("rounds a run's length", () => {
    expect(span(200)).toBe("1 s");
    expect(span(47 * S)).toBe("47 s");
    expect(span(3 * M + 10 * S)).toBe("3 min");
    expect(span(59 * S + 500)).toBe("59 s");
    expect(span(2 * H)).toBe("2 h");
  });
});
