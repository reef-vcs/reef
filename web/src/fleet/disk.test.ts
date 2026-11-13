// R2.20 test 4, as #272 left it: the daemon keeps every disk figure, so the
// page reads only the selection (without a walk), again when touched, and
// walks one workspace only when asked ("Measure again"); paced, from frames,
// never with a snapshot.
import { afterEach, describe, expect, it, vi } from "vitest";
import { Code } from "@connectrpc/connect";
import { daemonError, manualFrames, settle } from "../test/fakes";
import { FakeFleetDaemon, REPO, workspace } from "../test/fleet-daemon";
import { MIN_SPACING_MS } from "../live/paced";
import { DiskScheduler } from "./disk";

afterEach(() => {
  vi.useRealTimers();
});

function setup(ids = ["w3", "w2", "w1"]) {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  const daemon = new FakeFleetDaemon();
  daemon.workspaces = ids.map((id) => workspace(id, `c${id}`.padEnd(32, "0")));
  const frames = manualFrames();
  const onChange = vi.fn();
  const onSessionInvalid = vi.fn();
  const onMeasured = vi.fn();
  const disk = new DiskScheduler(
    daemon.connection(),
    () => REPO,
    frames.frame,
    onChange,
    onSessionInvalid,
    Date.now,
    onMeasured,
  );
  const measured = () => daemon.getCalls.map((c) => c.workspace);
  /** Runs frames and answers until nothing more is due within `ms`. */
  const step = async (ms = 0) => {
    frames.flush();
    await settle(200);
    vi.advanceTimersByTime(ms);
    frames.flush();
    await settle(200);
  };
  const walked = () => daemon.getCalls.filter((c) => !c.skipDiskUsage).map((c) => c.workspace);
  return { daemon, frames, disk, measured, walked, step, onChange, onMeasured };
}

describe("DiskScheduler", () => {
  it("reads nothing without a selection: the list carries every figure (#146)", async () => {
    const { disk, measured, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    for (let i = 0; i < 4; i++) await step(MIN_SPACING_MS);
    expect(measured()).toEqual([]);
  });

  it("reads the selection without a walk, from a frame, and again once touched", async () => {
    const { disk, frames, measured, walked, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    disk.select("w1");
    await settle(200);
    expect(measured()).toEqual([]);
    frames.flush();
    await settle(200);
    expect(measured()).toEqual(["w1"]);
    expect(walked()).toEqual([]);
    await step(10_000);
    expect(measured()).toEqual(["w1"]);
    disk.markDirty("w2");
    await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w1"]);
    disk.markDirty("w1");
    await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w1", "w1"]);
  });

  it("walks when asked, unspaced and first, and then refreshes the list", async () => {
    const { disk, frames, walked, step, onMeasured } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    disk.measureAgain("w2");
    frames.flush();
    await settle(200);
    expect(walked()).toEqual(["w2"]);
    expect(onMeasured).toHaveBeenCalledTimes(1);
    expect(disk.results.get("w2")!.response?.diskUsage?.filesChanged).toBe(3n);
    expect(disk.results.get("w2")!.measuredAt).toBeDefined();
    await step(10_000);
    expect(walked()).toEqual(["w2"]);
  });

  it("reads not measurable when a walk answers without a figure", async () => {
    const { daemon, disk, frames } = setup(["w1"]);
    daemon.noDisk = true;
    disk.setWorkspaces(["w1"]);
    disk.measureAgain("w1");
    frames.flush();
    await settle(200);
    expect(disk.results.get("w1")!.notMeasurable).toBe(true);
  });

  it("keeps the last answer when a call fails, and never sends snapshot_first", async () => {
    const { daemon, disk, step } = setup(["w1"]);
    disk.setWorkspaces(["w1"]);
    disk.select("w1");
    await step();
    daemon.failGetWorkspace = daemonError(Code.Unavailable, { reason: "BOOM" }, "boom");
    disk.markDirty("w1");
    await step(MIN_SPACING_MS);
    const result = disk.results.get("w1")!;
    expect(result.error?.reason).toBe("BOOM");
    expect(result.response).toBeDefined();
    await step(10_000);
    expect(daemon.getCalls).toHaveLength(2);
    expect(daemon.getCalls.every((c) => !c.snapshotFirst)).toBe(true);
  });

  it("reads nothing after stop", async () => {
    const { disk, measured, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    disk.stop();
    disk.select("w1");
    for (let i = 0; i < 3; i++) await step(MIN_SPACING_MS);
    expect(measured()).toEqual([]);
  });
});

describe("DiskScheduler marks", () => {
  it("keeps the selection dirty when an operation touched it during its read", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const daemon = new FakeFleetDaemon();
    daemon.workspaces = [workspace("w1", "c1".padEnd(32, "0"))];
    let release: () => void = () => undefined;
    daemon.holdGet = new Promise((resolve) => {
      release = resolve;
    });
    const frames = manualFrames();
    const disk = new DiskScheduler(daemon.connection(), () => REPO, frames.frame, vi.fn(), vi.fn());
    disk.setWorkspaces(["w1"]);
    disk.select("w1");
    frames.flush();
    await settle(200);
    expect(daemon.getCalls).toHaveLength(1);
    disk.markDirty("w1");
    daemon.holdGet = undefined;
    release();
    await settle(200);
    expect(disk.results.get("w1")!.dirty).toBe(true);
    frames.flush();
    await settle(200);
    vi.advanceTimersByTime(10_000);
    frames.flush();
    await settle(200);
    expect(daemon.getCalls).toHaveLength(2);
  });
});
