// R2.20 test 6: the workspaces page's model over an in-memory daemon and
// stores the test feeds.
import { create } from "@bufbuild/protobuf";
import { Code } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  OperationEffectsSchema,
  OperationKind,
  WorkingCopyEffectSchema,
} from "../gen/reef/api/v1/operations_pb";
import { WorkspaceState } from "../gen/reef/api/v1/workspaces_pb";
import { OperationStore } from "../live/operations";
import { MIN_SPACING_MS } from "../live/paced";
import { PresenceStore } from "../live/presence";
import {
  daemonError,
  immediateFrames,
  operation,
  operationsFrame,
  presenceFrame,
  sessionInvalid,
  settle,
} from "../test/fakes";
import { change, FakeFleetDaemon, REPO, T, workspace } from "../test/fleet-daemon";
import { FleetModel, REPOSITORY_NOT_FOUND } from "./fleet-model";

const C1 = "c1".padEnd(32, "0");
const C2 = "c2".padEnd(32, "0");

afterEach(() => {
  vi.useRealTimers();
});

function setup({
  daemon = withTwo(),
  live = true,
  at,
}: { daemon?: FakeFleetDaemon; live?: boolean; at?: string } = {}) {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  const operations = new OperationStore(immediateFrames);
  if (live) operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const presence = new PresenceStore(immediateFrames);
  presence.beginResync();
  presence.applyFrame(presenceFrame([], true));
  const onSessionInvalid = vi.fn();
  const model = new FleetModel(
    daemon.connection(),
    operations,
    presence,
    immediateFrames,
    onSessionInvalid,
    { repo: REPO.slice(0, 8), at },
  );
  const stop = model.start();
  let cursor = 1;
  const push = (...ops: ReturnType<typeof operation>[]) => {
    operations.applyFrame(
      operationsFrame({ operations: ops, live: true, cursor: `c${String(cursor++)}` }),
    );
  };
  return { daemon, operations, presence, model, stop, onSessionInvalid, push };
}

function withTwo(): FakeFleetDaemon {
  const daemon = new FakeFleetDaemon();
  daemon.workspaces = [workspace("w2", C2), workspace("w1", C1)];
  daemon.changes = [change(C1), change(C2)];
  return daemon;
}

async function flush(ms = 0) {
  await settle(200);
  vi.advanceTimersByTime(ms);
  await settle(200);
}

const ids = (model: FleetModel) => model.getSnapshot().rows?.map((r) => r.id);

describe("FleetModel", () => {
  it("reads nothing before the stream is live, then the repository, the status and both lists", async () => {
    const t = setup({ live: false });
    await flush();
    expect(t.daemon.repositoryCalls).toHaveLength(0);
    expect(t.daemon.listCalls).toHaveLength(0);
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
    await flush();
    expect(t.daemon.repositoryCalls.map((c) => c.repo)).toEqual([REPO.slice(0, 8)]);
    expect(t.daemon.statusCalls.map((c) => c.skipBackends)).toEqual([true]);
    const list = t.daemon.listCalls[0]!;
    expect(list.repo).toBe(REPO);
    expect(list.states).toEqual([WorkspaceState.ACTIVE, WorkspaceState.RELEASING]);
    expect(list.changedFileCounts).toBe(true);
    expect(list.pageSize).toBe(1000);
    const changes = t.daemon.changeCalls[0]!;
    expect(changes.revset).toBe("working_copies()");
    expect(changes.skipSnapshot).toBe(true);
    expect(ids(t.model)).toEqual(["w2", "w1"]);
    expect(t.model.getSnapshot().rows![0]!.change?.changeId).toBe(C2);
  });

  it("follows page tokens", async () => {
    const daemon = withTwo();
    daemon.pageCap = 1;
    const t = setup({ daemon });
    await flush();
    expect(t.daemon.listCalls.map((c) => c.pageToken)).toEqual(["", "1"]);
    expect(ids(t.model)).toEqual(["w2", "w1"]);
  });

  it("refreshes for this repository's operations only, one for a burst and one more after", async () => {
    const t = setup();
    await flush();
    expect(t.daemon.listCalls).toHaveLength(1);
    t.push(operation("o-other", "ffff", T));
    await flush(MIN_SPACING_MS);
    expect(t.daemon.listCalls).toHaveLength(1);
    t.push(...Array.from({ length: 1000 }, (_, i) => operation(`o${String(i)}`, REPO, T)));
    await flush(MIN_SPACING_MS);
    expect(t.daemon.listCalls).toHaveLength(2);
    // Operations during a refresh ask for exactly one more.
    let release: () => void = () => undefined;
    t.daemon.hold = new Promise((resolve) => {
      release = resolve;
    });
    t.push(operation("o-a", REPO, T));
    await flush(MIN_SPACING_MS);
    expect(t.daemon.listCalls).toHaveLength(3);
    t.push(operation("o-b", REPO, T), operation("o-c", REPO, T));
    t.push(operation("o-d", REPO, T));
    t.daemon.hold = undefined;
    release();
    // The held read took 250 ms of fake time, so the next is spaced 1 s.
    await flush(MIN_SPACING_MS);
    await flush(4 * MIN_SPACING_MS);
    expect(t.daemon.listCalls).toHaveLength(4);
    await flush(10_000);
    expect(t.daemon.listCalls).toHaveLength(4);
  });

  it("keeps a workspace whose change the second read missed, without its change", async () => {
    const daemon = withTwo();
    daemon.changes = [change(C1)];
    const t = setup({ daemon });
    await flush();
    const row = t.model.getSnapshot().rows!.find((r) => r.id === "w2")!;
    expect(row.change).toBeUndefined();
    expect(row.workspace.changeId).toBe(C2);
  });

  it("keeps a workspace that ended in place, read once, until hidden", async () => {
    const t = setup();
    await flush();
    t.daemon.workspaces = [workspace("w3", C1), workspace("w1", C1)];
    t.daemon.finals.set("w2", workspace("w2", C2, { state: WorkspaceState.RELEASED }));
    t.push(
      operation("o-rel", REPO, T, {
        kind: OperationKind.RELEASE_WORKSPACE,
        workspaceId: "w2",
        agent: "ann",
      }),
    );
    await flush(MIN_SPACING_MS);
    expect(ids(t.model)).toEqual(["w3", "w2", "w1"]);
    const ended = t.model.getSnapshot().rows!.find((r) => r.id === "w2")!;
    expect(ended.ended?.word).toBe("released");
    expect(ended.ended?.operation?.id).toBe("o-rel");
    expect(t.daemon.getCalls.filter((c) => c.workspace === "w2" && c.skipDiskUsage)).toHaveLength(
      1,
    );
    expect(t.model.getSnapshot().rows!.find((r) => r.id === "w3")!.arrived).toBe(true);
    // Another refresh does not read it again.
    t.push(operation("o-x", REPO, T));
    await flush(MIN_SPACING_MS);
    expect(t.daemon.getCalls.filter((c) => c.workspace === "w2" && c.skipDiskUsage)).toHaveLength(
      1,
    );
    t.model.hideEnded();
    expect(ids(t.model)).toEqual(["w3", "w1"]);
  });

  it("reads a reclaimed workspace's word from its state", async () => {
    const t = setup();
    await flush();
    t.daemon.workspaces = [workspace("w1", C1)];
    t.daemon.finals.set("w2", workspace("w2", C2, { state: WorkspaceState.RECLAIMED }));
    t.push(
      operation("o-rel", REPO, T, { kind: OperationKind.RELEASE_WORKSPACE, workspaceId: "w2" }),
    );
    await flush(MIN_SPACING_MS);
    expect(t.model.getSnapshot().rows!.find((r) => r.id === "w2")!.ended?.word).toBe(
      "reclaimed (lease expired)",
    );
  });

  it("keeps the agents' order: by name first, new ones on top, gone ones until hidden", async () => {
    const t = setup();
    t.presence.applyFrame(
      presenceFrame([
        { id: "p-b", agent: "bob", repoId: REPO, workspaceId: "w1" },
        { id: "p-a", agent: "ann", repoId: REPO, workspaceId: "w2" },
      ]),
    );
    await flush();
    expect(t.model.getSnapshot().agentOrder).toEqual(["ann", "bob"]);
    t.presence.applyFrame(presenceFrame([{ id: "p-c", agent: "cat", repoId: REPO }]));
    t.presence.applyFrame(presenceFrame([{ id: "p-a", ended: true }]));
    expect(t.model.getSnapshot().agentOrder).toEqual(["cat", "ann", "bob"]);
    t.model.hideGone(["ann"]);
    expect(t.model.getSnapshot().agentOrder).toEqual(["cat", "bob"]);
  });

  it("records causes from operations' workspace ids", async () => {
    const t = setup();
    await flush();
    t.push(
      operation("o-s", REPO, T, { kind: OperationKind.SNAPSHOT, workspaceId: "w1", agent: "ann" }),
    );
    await flush(MIN_SPACING_MS);
    expect(t.model.getSnapshot().rows!.find((r) => r.id === "w1")!.cause?.id).toBe("o-s");
  });

  it("marks the disk of workspaces that snapshots, edits, moves and working-copy effects touch", async () => {
    const t = setup();
    await flush();
    const dirty = vi.spyOn(t.model.disk, "markDirty");
    t.push(
      operation("o-s", REPO, T, { kind: OperationKind.SNAPSHOT, workspaceId: "w1" }),
      operation("o-d", REPO, T, { kind: OperationKind.DESCRIBE, workspaceId: "w2" }),
    );
    const moved = operation("o-m", REPO, T, { kind: OperationKind.ABANDON });
    moved.effects = create(OperationEffectsSchema, {
      workingCopies: [create(WorkingCopyEffectSchema, { workspaceId: "w2", commitId: "x" })],
    });
    t.push(moved);
    expect(dirty.mock.calls.map((c) => c[0])).toEqual(["w1", "w2"]);
  });

  it("opens the restart window from the daemon's uptime and closes it with one delay", async () => {
    const daemon = withTwo();
    daemon.uptimeMs = 30_000;
    const t = setup({ daemon });
    await flush();
    // Two 60 s heartbeats less 30 s of uptime.
    expect(t.model.getSnapshot().restarting).toBe(true);
    const reads = t.daemon.listCalls.length + t.daemon.statusCalls.length;
    vi.advanceTimersByTime(89_999);
    expect(t.model.getSnapshot().restarting).toBe(true);
    vi.advanceTimersByTime(1);
    expect(t.model.getSnapshot().restarting).toBe(false);
    await flush();
    expect(t.daemon.listCalls.length + t.daemon.statusCalls.length).toBe(reads);
  });

  it("reads the status again at a presence resync", async () => {
    const t = setup();
    await flush();
    expect(t.daemon.statusCalls).toHaveLength(1);
    t.daemon.uptimeMs = 1000;
    t.presence.beginResync();
    await flush();
    expect(t.daemon.statusCalls).toHaveLength(2);
    expect(t.model.getSnapshot().restarting).toBe(true);
  });

  it("keeps the last rows on an error, and retries on the next operation", async () => {
    const t = setup();
    await flush();
    t.daemon.failList = daemonError(Code.Unavailable, { reason: "BUSY", hint: "later" });
    t.push(operation("o1", REPO, T));
    await flush(MIN_SPACING_MS);
    expect(t.model.getSnapshot().error?.reason).toBe("BUSY");
    expect(ids(t.model)).toEqual(["w2", "w1"]);
    t.daemon.failList = undefined;
    t.push(operation("o2", REPO, T));
    await flush(MIN_SPACING_MS);
    expect(t.model.getSnapshot().error).toBeNull();
  });

  it("replaces the page when the repository is not found, and ends an invalid session", async () => {
    const daemon = withTwo();
    daemon.failRepository = daemonError(Code.NotFound, { reason: REPOSITORY_NOT_FOUND });
    const t = setup({ daemon });
    await flush();
    expect(t.model.getSnapshot().fatal?.reason).toBe(REPOSITORY_NOT_FOUND);
    const other = withTwo();
    other.failRepository = sessionInvalid();
    const u = setup({ daemon: other });
    await flush();
    expect(u.onSessionInvalid).toHaveBeenCalled();
  });

  it("reads at an operation once, frozen, without presence or disk", async () => {
    const t = setup({ at: "abcd" });
    await flush();
    expect(t.daemon.operationCalls.map((c) => c.operationId)).toEqual(["abcd"]);
    expect(t.daemon.listCalls[0]!.atOperation).toBe("abcd-full");
    expect(t.daemon.changeCalls[0]!.atOperation).toBe("abcd-full");
    expect(t.model.getSnapshot().frozenAt?.id).toBe("abcd-full");
    t.push(operation("o1", REPO, T));
    t.presence.beginResync();
    await flush(10_000);
    expect(t.daemon.listCalls).toHaveLength(1);
    expect(t.daemon.statusCalls).toHaveLength(0);
    expect(t.daemon.getCalls).toHaveLength(0);
    await t.model.selectAt("w1");
    expect(t.daemon.getCalls.map((c) => [c.workspace, c.atOperation, c.skipDiskUsage])).toEqual([
      ["w1", "abcd-full", true],
    ]);
  });

  it("drops a frozen selection's answer once another is selected (review finding 2)", async () => {
    const t = setup({ at: "abcd" });
    await flush();
    let release: () => void = () => undefined;
    t.daemon.holdGet = new Promise((resolve) => {
      release = resolve;
    });
    const first = t.model.selectAt("w1");
    await settle(50);
    t.daemon.holdGet = undefined;
    await t.model.selectAt("w2");
    expect(t.model.getSnapshot().frozenWorkspace?.workspace?.id).toBe("w2");
    release();
    await first;
    expect(t.model.getSnapshot().frozenWorkspace?.workspace?.id).toBe("w2");
  });

  it("measures the restart window from when the uptime was read (review finding 7)", async () => {
    const daemon = new FakeFleetDaemon();
    daemon.uptimeMs = 10_000;
    const t = setup({ daemon });
    await flush();
    expect(t.model.getSnapshot().restarting).toBe(false);
    vi.advanceTimersByTime(200_000);
    t.daemon.workspaces = [workspace("w1", C1)];
    t.push(operation("o-create", REPO, T, { workspaceId: "w1" }));
    await flush(MIN_SPACING_MS);
    expect(t.model.getSnapshot().rows).toHaveLength(1);
    expect(t.model.getSnapshot().restarting).toBe(false);
  });
});
