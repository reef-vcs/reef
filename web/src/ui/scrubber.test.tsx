// R2.21 test 6: the scrubber over a window of an in-memory log.
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import { OperationStore } from "../live/operations";
import { OperationWindow } from "../log/window";
import { immediateFrames, operationsFrame, settle } from "../test/fakes";
import { FakeOplog, REPO, T } from "../test/oplog-daemon";
import { navigate, useSearch } from "./router";
import { COMMIT_DELAY_MS, Scrubber } from "./Scrubber";
import { withDevMode } from "../test/dev-mode";

// These tests check the ids, commands and exact times developer mode shows.
withDevMode();

const MIN = 60_000;
const PAGE = `/repos/${REPO}/smartlog`;

beforeEach(() => {
  window.history.replaceState(null, "", `${PAGE}?revset=all()`);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
  vi.restoreAllMocks();
});

async function setup(count = 5) {
  const oplog = new FakeOplog();
  const ops: Operation[] = [];
  for (let i = 0; i < count; i++) {
    ops.push(oplog.add({ endedAt: T + i * MIN, workspaceId: "w1", agent: "ann" }));
  }
  const store = new OperationStore(immediateFrames);
  store.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const connection = oplog.connection();
  const win = new OperationWindow(connection, store, immediateFrames, () => undefined, REPO);
  win.start();
  function Harness() {
    const at = new URLSearchParams(useSearch()).get("at") ?? "";
    return (
      <Scrubber window={win} at={at} connection={connection} onSessionInvalid={() => undefined} />
    );
  }
  render(<Harness />);
  await act(async () => {
    await settle(200);
  });
  return { oplog, ops, win, slider: screen.getByRole("slider") };
}

const at = () => new URLSearchParams(window.location.search).get("at");

async function elapse(ms: number) {
  await act(async () => {
    vi.advanceTimersByTime(ms);
    await settle(50);
  });
}

describe("Scrubber", () => {
  it("is one slider with a value and a text", async () => {
    const { slider } = await setup();
    expect(slider.getAttribute("aria-valuemin")).toBe("0");
    expect(slider.getAttribute("aria-valuemax")).toBe("5");
    expect(slider.getAttribute("aria-valuenow")).toBe("5");
    expect(slider.getAttribute("aria-valuetext")).toBe("Live");
  });

  it("commits a burst of steps as one navigation after the pause, pushing once from live", async () => {
    const { slider, ops } = await setup();
    vi.useFakeTimers();
    const push = vi.spyOn(window.history, "pushState");
    const replace = vi.spyOn(window.history, "replaceState");
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(slider.getAttribute("aria-valuetext")).toContain(ops[4]?.id.slice(0, 8));
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(slider.getAttribute("aria-valuenow")).toBe("3");
    expect(slider.getAttribute("aria-valuetext")).toContain("4 of 5");
    await elapse(COMMIT_DELAY_MS - 1);
    expect(at()).toBeNull();
    await elapse(1);
    expect(at()).toBe(ops[3]?.id);
    expect(push).toHaveBeenCalledTimes(1);
    // The rest of the query is kept.
    expect(new URLSearchParams(window.location.search).get("revset")).toBe("all()");
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    await elapse(COMMIT_DELAY_MS);
    expect(at()).toBe(ops[2]?.id);
    expect(push).toHaveBeenCalledTimes(1);
    expect(replace).toHaveBeenCalledTimes(1);
  });

  it("steps with Right, Home, PageUp and PageDown, and commits End and Enter at once", async () => {
    const { slider, ops } = await setup();
    vi.useFakeTimers();
    fireEvent.keyDown(slider, { key: "Home" });
    expect(slider.getAttribute("aria-valuenow")).toBe("0");
    fireEvent.keyDown(slider, { key: "Enter" });
    await elapse(0);
    expect(at()).toBe(ops[0]?.id);
    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(slider.getAttribute("aria-valuenow")).toBe("1");
    // One bucket (a minute apart: 10-second buckets) older and newer.
    fireEvent.keyDown(slider, { key: "PageUp" });
    expect(slider.getAttribute("aria-valuenow")).toBe("2");
    fireEvent.keyDown(slider, { key: "PageDown" });
    expect(slider.getAttribute("aria-valuenow")).toBe("1");
    fireEvent.keyDown(slider, { key: "End" });
    await elapse(0);
    expect(at()).toBeNull();
    expect(slider.getAttribute("aria-valuetext")).toBe("Live");
  });

  it("goes from Live to the newest operation with Left, and back to Live with Right", async () => {
    const { slider, ops } = await setup();
    vi.useFakeTimers();
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    await elapse(COMMIT_DELAY_MS);
    expect(at()).toBe(ops[4]?.id);
    fireEvent.keyDown(slider, { key: "ArrowRight" });
    await elapse(COMMIT_DELAY_MS);
    expect(at()).toBeNull();
  });

  it("previews a drag and commits it on release", async () => {
    const { slider, ops } = await setup();
    vi.spyOn(slider, "getBoundingClientRect").mockReturnValue({
      left: 0,
      width: 100,
      top: 0,
      height: 44,
    } as DOMRect);
    slider.setPointerCapture = () => undefined;
    fireEvent.pointerDown(slider, { clientX: 1, pointerId: 1 });
    expect(slider.getAttribute("aria-valuenow")).toBe("0");
    fireEvent.pointerMove(slider, { clientX: 99, pointerId: 1 });
    expect(slider.getAttribute("aria-valuenow")).toBe("4");
    expect(at()).toBeNull();
    fireEvent.pointerUp(slider, { clientX: 99, pointerId: 1 });
    expect(at()).toBe(ops[4]?.id);
  });

  it("goes to a time inside the window without a read, and asks the daemon outside it", async () => {
    const { oplog, ops } = await setup();
    const input = screen.getByLabelText("Go to time");
    const local = (ms: number) => {
      const d = new Date(ms);
      const pad = (n: number) => String(n).padStart(2, "0");
      return `${String(d.getFullYear())}-${pad(d.getMonth() + 1)}-${pad(d.getDate())}T${pad(d.getHours())}:${pad(d.getMinutes())}:${pad(d.getSeconds())}`;
    };
    fireEvent.change(input, { target: { value: local(T + 2 * MIN + 30_000) } });
    fireEvent.click(screen.getByRole("button", { name: "Go" }));
    await act(async () => {
      await settle(50);
    });
    expect(at()).toBe(ops[2]?.id);
    expect(oplog.getCalls.filter((c) => c.endedAtOrBefore)).toEqual([]);
    fireEvent.change(input, { target: { value: local(T - 3_600_000) } });
    fireEvent.click(screen.getByRole("button", { name: "Go" }));
    await act(async () => {
      await settle(100);
    });
    expect(oplog.getCalls.filter((c) => c.endedAtOrBefore)).toHaveLength(1);
    expect(screen.getByRole("alert").textContent).toContain("older than the oldest kept operation");
  });

  it("marks a detached point and steps it to its first parent", async () => {
    const { win, ops } = await setup();
    const old = { ...ops[0], id: "f".repeat(16), parentIds: ["e".repeat(16)] } as Operation;
    vi.spyOn(win, "point").mockReturnValue({ state: "detached", operation: old, records: [] });
    window.history.replaceState(null, "", `${PAGE}?at=${old.id}`);
    await act(async () => {
      window.dispatchEvent(new PopStateEvent("popstate"));
      await settle(20);
    });
    const slider = screen.getByRole("slider");
    expect(slider.getAttribute("aria-valuetext")).toContain("older than the loaded period");
    fireEvent.keyDown(slider, { key: "ArrowRight" });
    expect(at()).toBe(old.id);
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    expect(at()).toBe("e".repeat(16));
  });

  it("reads and navigates nothing on its own, and nothing moves while no operation arrives", async () => {
    const { oplog, slider } = await setup();
    vi.useFakeTimers();
    const before = window.location.href;
    const value = slider.getAttribute("aria-valuetext");
    await elapse(60_000);
    expect(window.location.href).toBe(before);
    expect(slider.getAttribute("aria-valuetext")).toBe(value);
    expect(oplog.listCalls).toHaveLength(1);
    expect(oplog.getCalls).toHaveLength(0);
  });
});

describe("Scrubber, after review", () => {
  it("drops a pending commit when the page navigates elsewhere first", async () => {
    const { slider } = await setup();
    vi.useFakeTimers();
    fireEvent.keyDown(slider, { key: "ArrowLeft" });
    await act(async () => {
      navigate(`/repos/${REPO}/workspaces`);
      await settle(10);
    });
    await elapse(COMMIT_DELAY_MS * 2);
    expect(window.location.pathname).toBe(`/repos/${REPO}/workspaces`);
    expect(at()).toBeNull();
    expect(screen.getByRole("slider").getAttribute("aria-valuetext")).toBe("Live");
  });

  it("shows a point inside the window at once, with no read", async () => {
    const { oplog, ops } = await setup();
    window.history.replaceState(null, "", `${PAGE}?at=${ops[1]?.id ?? ""}`);
    await act(async () => {
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    expect(screen.getByRole("slider").getAttribute("aria-valuenow")).toBe("1");
    expect(oplog.listCalls.filter((c) => c.atOperation)).toEqual([]);
  });
});
