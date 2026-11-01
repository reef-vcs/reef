// R2.19 test 8: the smartlog's model over an in-memory daemon and an
// operation store the test feeds, with fake timers.
import { create } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code } from "@connectrpc/connect";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ChangeEffectKind,
  OperationEffectsSchema,
  OperationKind,
  OperationSchema,
  type Operation,
} from "../gen/reef/api/v1/operations_pb";
import { DEFAULT_LIMIT, SMARTLOG_REVSET } from "../graph/revset";
import { after } from "../live/backoff";
import { OperationStore } from "../live/operations";
import {
  daemonError,
  immediateFrames,
  operationsFrame,
  sessionInvalid,
  settle,
} from "../test/fakes";
import { entries, FakeRepoDaemon, hexId, REPO } from "../test/smartlog-daemon";
import {
  INVALID_REVSET,
  MIN_SPACING_MS,
  SmartlogModel,
  type SmartlogOptions,
} from "./smartlog-model";

beforeEach(() => {
  vi.useFakeTimers();
});

afterEach(() => {
  vi.useRealTimers();
});

const LOG = `
a2 a1
a1 t4
b1 t4
t4! :t2
t2! ~t1
`;

/** An operation on `repo` that rewrote `rewrote` (names, as in the log's text): a2 by default. */
function op(id: string, { repo = REPO, rewrote = ["a2"], effects = true } = {}): Operation {
  return create(OperationSchema, {
    id,
    repoId: repo,
    kind: OperationKind.DESCRIBE,
    agent: "agent-a",
    endedAt: timestampFromMs(1_761_919_380_000),
    ...(effects
      ? {
          effects: create(OperationEffectsSchema, {
            changes: rewrote.map((name) => ({
              changeId: hexId(name),
              kind: ChangeEffectKind.REWRITTEN,
              commitId: name,
            })),
          }),
        }
      : {}),
  });
}

function setup({
  live = true,
  options = {},
}: { live?: boolean; options?: Partial<SmartlogOptions> } = {}) {
  const daemon = new FakeRepoDaemon();
  daemon.log = entries(LOG);
  const operations = new OperationStore(immediateFrames);
  const onSessionInvalid = vi.fn();
  const model = new SmartlogModel(
    daemon.connection(),
    operations,
    immediateFrames,
    onSessionInvalid,
    { repo: REPO, revset: SMARTLOG_REVSET, ...options },
    () => Date.now(),
  );
  if (live) operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const stop = model.start();
  const push = (...ops: Operation[]) => {
    operations.applyFrame(operationsFrame({ operations: ops, live: true }));
  };
  return { daemon, operations, model, stop, push, onSessionInvalid };
}

const ids = (model: SmartlogModel) => model.getSnapshot().rows?.map((r) => r.change.commitId);

/** Lets reads answer, and moves time past the spacing. */
async function idle(ms = MIN_SPACING_MS) {
  await settle(40);
  await vi.advanceTimersByTimeAsync(ms);
  await settle(40);
}

describe("SmartlogModel", () => {
  it("reads nothing before the stream is live, then the revset in repository scope", async () => {
    const t = setup({ live: false });
    await idle();
    expect(t.daemon.listCalls).toHaveLength(0);
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(1);
    const call = t.daemon.listCalls[0]!;
    expect(call.repo).toBe(REPO);
    expect(call.workspace).toBe("");
    expect(call.revset).toBe(SMARTLOG_REVSET);
    expect(call.pageSize).toBe(DEFAULT_LIMIT);
    expect(call.skipSnapshot).toBe(true);
    expect(t.daemon.statusCalls.map((c) => c.atOperation)).toEqual(["op-1"]);
    expect(ids(t.model)).toEqual(["a2", "a1", "b1", "t4", "t2"]);
    expect(t.model.getSnapshot().status?.trunkBookmark).toBe("main");
  });

  it("follows a page cut short by bytes until the limit", async () => {
    const daemon = new FakeRepoDaemon();
    daemon.log = entries(LOG);
    daemon.pageCap = 2;
    const operations = new OperationStore(immediateFrames);
    operations.applyFrame(operationsFrame({ live: true }));
    const model = new SmartlogModel(daemon.connection(), operations, immediateFrames, vi.fn(), {
      repo: REPO,
      revset: SMARTLOG_REVSET,
      limit: 4,
    });
    model.start();
    await idle();
    expect(daemon.listCalls.map((c) => [c.pageToken, c.pageSize])).toEqual([
      ["", 4],
      ["2", 2],
    ]);
    expect(model.getSnapshot().rows).toHaveLength(4);
    expect(model.getSnapshot().more).toBe(true);
  });

  it("makes one refresh of a burst, one more for operations during it, spaced", async () => {
    const t = setup();
    await idle();
    expect(t.daemon.listCalls).toHaveLength(1);

    t.push(...Array.from({ length: 1000 }, (_, i) => op(`o${String(i)}`)));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(2);

    // Operations during a refresh ask for exactly one more, after it.
    let release: () => void = () => undefined;
    t.daemon.hold = new Promise((resolve) => {
      release = resolve;
    });
    t.push(op("p1"));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(3);
    t.push(op("p2"));
    t.push(op("p3"));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(3);
    t.daemon.hold = undefined;
    release();
    // The held read took about 500 ms: the trailing one waits 4 × that from its start.
    await idle();
    expect(t.daemon.listCalls).toHaveLength(3);
    await idle(2000);
    expect(t.daemon.listCalls).toHaveLength(4);
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(4);
  });

  it("spaces refreshes by max(250 ms, 4 × the last refresh)", async () => {
    const t = setup();
    await idle();
    // The next read takes 200 ms, so the one after waits 800 ms from its start.
    t.daemon.hold = new Promise((resolve) => after(200, resolve));
    t.push(op("p1"));
    await settle(40);
    expect(t.daemon.listCalls).toHaveLength(2);
    t.daemon.hold = undefined;
    await vi.advanceTimersByTimeAsync(200);
    await settle(40);
    t.push(op("p2"));
    await settle(40);
    expect(t.daemon.listCalls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(590);
    await settle(40);
    expect(t.daemon.listCalls).toHaveLength(2);
    await vi.advanceTimersByTimeAsync(20);
    await settle(40);
    expect(t.daemon.listCalls).toHaveLength(3);
  });

  it("ignores other repositories and operations whose effects are empty", async () => {
    const t = setup();
    await idle();
    t.push(op("x", { repo: "ffffffffffffffff" }), op("y", { rewrote: [] }));
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(1);
    // Effects not reported at all: read.
    t.push(op("z", { effects: false }));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(2);
  });

  it("refreshes on a reset, and restarts a read whose operation GC removed", async () => {
    const t = setup();
    await idle();
    t.operations.reset();
    await idle();
    expect(t.daemon.listCalls).toHaveLength(1);
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c1" }));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(2);

    t.daemon.failStatusOnce = daemonError(Code.NotFound, { reason: "OPERATION_NOT_FOUND" });
    t.push(op("p1"));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(4);
    expect(t.daemon.statusCalls).toHaveLength(4);
    expect(t.model.getSnapshot().error).toBeNull();
  });

  it("records causes, and marks rewritten and new rows changed once", async () => {
    const t = setup();
    await idle();
    expect(t.model.getSnapshot().rows?.some((r) => r.changed)).toBe(false);

    // b1 rewritten (a new commit of change b1), c1 new; a2 and a1 untouched.
    t.daemon.log = entries(
      `
      c1 t4
      b1x/b1 t4
      ${LOG}`.replace(/\n\s*b1 t4/, ""),
    );
    t.push(op("p1", { rewrote: ["b1"] }));
    await idle();
    const rows = new Map(t.model.getSnapshot().rows?.map((r) => [r.change.commitId, r]));
    expect(rows.get("b1x")?.changed).toBe(true);
    expect(rows.get("b1x")?.cause?.id).toBe("p1");
    expect(rows.get("c1")?.changed).toBe(true);
    expect(rows.get("a2")?.changed).toBe(false);
    // Stacks kept their places; the new one is on top.
    expect(ids(t.model)).toEqual(["c1", "a2", "a1", "b1x", "t4", "t2"]);
  });

  it("does not animate rows revealed by Show more, and puts them below", async () => {
    const daemon = new FakeRepoDaemon();
    daemon.log = entries(`
      a2 a1
      a1 t4
      b1 t4
      t4!
    `);
    const operations = new OperationStore(immediateFrames);
    operations.applyFrame(operationsFrame({ live: true }));
    const model = new SmartlogModel(daemon.connection(), operations, immediateFrames, vi.fn(), {
      repo: REPO,
      revset: SMARTLOG_REVSET,
      limit: 2,
    });
    model.start();
    await idle();
    expect(ids(model)).toEqual(["a2", "a1"]);
    model.showMore(2);
    await idle();
    expect(daemon.listCalls.at(-1)?.pageSize).toBe(4);
    expect(ids(model)).toEqual(["a2", "a1", "b1", "t4"]);
    expect(model.getSnapshot().rows?.some((r) => r.changed)).toBe(false);
  });

  it("keeps the last graph on a bad revset, and ends the session when refused", async () => {
    const t = setup();
    await idle();
    t.daemon.failList = daemonError(Code.InvalidArgument, {
      reason: INVALID_REVSET,
      hint: "Check the revset",
    });
    t.push(op("p1"));
    await idle();
    expect(t.model.getSnapshot().error?.reason).toBe(INVALID_REVSET);
    expect(ids(t.model)).toEqual(["a2", "a1", "b1", "t4", "t2"]);

    t.daemon.failList = sessionInvalid();
    t.push(op("p2"));
    await idle();
    expect(t.onSessionInvalid).toHaveBeenCalled();
  });

  it("reads once at an operation, and never from the stream", async () => {
    const t = setup({ options: { at: "abcd" } });
    await idle();
    expect(t.daemon.operationCalls.map((c) => c.operationId)).toEqual(["abcd"]);
    expect(t.daemon.listCalls.map((c) => c.atOperation)).toEqual(["abcd-full"]);
    expect(t.daemon.statusCalls.map((c) => c.atOperation)).toEqual(["abcd-full"]);
    expect(t.model.frozen).toBe(true);
    expect(t.model.getSnapshot().frozenAt?.id).toBe("abcd-full");

    t.push(op("p1", { rewrote: ["a2"] }));
    t.operations.reset();
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(1);
    expect(t.model.getSnapshot().rows?.every((r) => r.cause === undefined)).toBe(true);
  });

  it("matches operations by the resolved id when the route names a prefix", async () => {
    const t = setup({ options: { repo: "a3dd" } });
    await idle();
    expect(t.daemon.listCalls[0]?.repo).toBe("a3dd");
    t.push(op("p1"));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(2);
    t.push(op("p2", { repo: "ffffffffffffffff" }));
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(2);
  });

  it("refreshes for kinds whose effects leave things out, such as a tag import", async () => {
    const t = setup();
    await idle();
    const imported = op("p1", { rewrote: [] });
    imported.kind = OperationKind.IMPORT_REFS;
    t.push(imported);
    await idle();
    expect(t.daemon.listCalls).toHaveLength(2);
  });

  it("makes one read of a first live batch that carries operations", async () => {
    const t = setup({ live: false });
    t.operations.applyFrame(operationsFrame({ live: true, operations: [op("p1")] }));
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(1);
  });

  it("reads nothing while the stream is not live, even when Show more asks", async () => {
    const t = setup();
    await idle();
    t.operations.reset();
    await idle();
    t.model.showMore();
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(1);
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c1" }));
    await idle();
    expect(t.daemon.listCalls).toHaveLength(2);
    expect(t.daemon.listCalls[1]?.pageSize).toBe(2 * DEFAULT_LIMIT);
  });

  it("keeps the last graph when a new revset is invalid, and does not animate a new one", async () => {
    const t = setup();
    await idle();
    t.daemon.failList = daemonError(Code.InvalidArgument, {
      reason: INVALID_REVSET,
      hint: "Check",
    });
    t.model.setRevset("nonsense(");
    await idle();
    expect(t.daemon.listCalls.at(-1)?.revset).toBe("nonsense(");
    expect(t.model.getSnapshot().error?.hint).toBe("Check");
    expect(ids(t.model)).toEqual(["a2", "a1", "b1", "t4", "t2"]);
    t.daemon.failList = undefined;
    t.daemon.log = entries("b1 t4\nt4!");
    t.model.setRevset("b1 | t4");
    await idle();
    expect(ids(t.model)).toEqual(["b1", "t4"]);
    expect(t.model.getSnapshot().error).toBeNull();
    expect(t.model.getSnapshot().rows?.some((r) => r.changed)).toBe(false);
  });

  it("says so when the daemon finds no operation to read at", async () => {
    const daemon = new FakeRepoDaemon();
    daemon.noOperation = true;
    const model = new SmartlogModel(
      daemon.connection(),
      new OperationStore(immediateFrames),
      immediateFrames,
      vi.fn(),
      { repo: REPO, revset: SMARTLOG_REVSET, at: "abcd" },
    );
    model.start();
    await idle();
    expect(model.getSnapshot().error?.code).toBe(Code.NotFound);
    expect(daemon.listCalls).toHaveLength(0);
  });

  it("stops reading once stopped", async () => {
    const t = setup();
    await idle();
    t.stop();
    t.push(op("p1"));
    await idle(5000);
    expect(t.daemon.listCalls).toHaveLength(1);
  });
});
