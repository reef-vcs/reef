// R2.20 test 4: disk measured one walk at a time, paced, in order, from
// frames, re-measured only when touched or asked, never with a snapshot.
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
  const disk = new DiskScheduler(
    daemon.connection(),
    () => REPO,
    frames.frame,
    onChange,
    onSessionInvalid,
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
  return { daemon, frames, disk, measured, step, onChange };
}

describe("DiskScheduler", () => {
  it("measures one at a time, in list order, spaced, and nothing without a frame", async () => {
    const { disk, frames, measured, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    await settle(200);
    expect(measured()).toEqual([]);
    frames.flush();
    await settle(200);
    expect(measured()).toEqual(["w3"]);
    await step(MIN_SPACING_MS - 1);
    expect(measured()).toEqual(["w3"]);
    await step(1);
    expect(measured()).toEqual(["w3", "w2"]);
    await step(MIN_SPACING_MS);
    await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w3", "w2", "w1"]);
    await step(10_000);
    expect(measured()).toEqual(["w3", "w2", "w1"]);
  });

  it("measures the selection first, then the never measured, then the dirty, oldest first", async () => {
    const { disk, measured, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    disk.select("w1");
    await step();
    for (let i = 0; i < 3; i++) await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w1", "w3", "w2"]);
    disk.markDirty("w2");
    disk.markDirty("w3");
    for (let i = 0; i < 3; i++) await step(MIN_SPACING_MS);
    // w3 was measured before w2.
    expect(measured()).toEqual(["w1", "w3", "w2", "w3", "w2"]);
  });

  it("marks only the workspace touched", async () => {
    const { disk, measured, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    for (let i = 0; i < 4; i++) await step(MIN_SPACING_MS);
    disk.markDirty("w2");
    for (let i = 0; i < 3; i++) await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w3", "w2", "w1", "w2"]);
  });

  it("measures again unspaced and first when asked", async () => {
    const { disk, measured, step, frames } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    for (let i = 0; i < 4; i++) await step(MIN_SPACING_MS);
    expect(measured()).toHaveLength(3);
    disk.markDirty("w3");
    disk.measureAgain("w1");
    frames.flush();
    await settle(200);
    expect(measured().at(-1)).toBe("w1");
    await step(MIN_SPACING_MS);
    expect(measured().at(-1)).toBe("w3");
  });

  it("reads not measurable without a figure, and retries only once dirty", async () => {
    const { daemon, disk, measured, step } = setup(["w1"]);
    daemon.noDisk = true;
    disk.setWorkspaces(["w1"]);
    await step();
    expect(disk.results.get("w1")!.notMeasurable).toBe(true);
    await step(10_000);
    expect(measured()).toEqual(["w1"]);
    daemon.noDisk = false;
    disk.markDirty("w1");
    await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w1", "w1"]);
    expect(disk.results.get("w1")!.notMeasurable).toBe(false);
    expect(disk.results.get("w1")!.response?.diskUsage?.filesChanged).toBe(3n);
  });

  it("keeps the last figure when a call fails, and never sends snapshot_first", async () => {
    const { daemon, disk, step } = setup(["w1"]);
    disk.setWorkspaces(["w1"]);
    await step();
    daemon.failGetWorkspace = daemonError(Code.Unavailable, { reason: "BOOM" }, "boom");
    disk.markDirty("w1");
    await step(MIN_SPACING_MS);
    const result = disk.results.get("w1")!;
    expect(result.error?.reason).toBe("BOOM");
    expect(result.response?.diskUsage).toBeDefined();
    await step(10_000);
    expect(daemon.getCalls).toHaveLength(2);
    expect(daemon.getCalls.every((c) => !c.snapshotFirst && !c.skipDiskUsage)).toBe(true);
  });

  it("measures nothing after stop", async () => {
    const { disk, measured, step } = setup();
    disk.setWorkspaces(["w3", "w2", "w1"]);
    await step();
    disk.stop();
    for (let i = 0; i < 3; i++) await step(MIN_SPACING_MS);
    expect(measured()).toEqual(["w3"]);
  });
});

describe("DiskScheduler marks", () => {
  it("keeps a workspace dirty when an operation touched it during its walk", async () => {
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
