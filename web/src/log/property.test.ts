// R2.21 test 4: seeded scenarios against the in-memory log. Each grows a
// log (one or two writers, with concurrent heads and merges), delivers it in
// frames split at random, and injects replayed frames (reconnects,
// handovers), resets with skipped operations, a GC, older pages and
// reloads. At each quiet point the window holds each operation of its
// period exactly once, the counts agree, a fresh window shows the same, and
// the feed at a random point is what a list at it would show.
import { describe, expect, it } from "vitest";
import { OperationKind, type Operation } from "../gen/reef/api/v1/operations_pb";
import { OperationStore } from "../live/operations";
import { immediateFrames, operationsFrame, settle } from "../test/fakes";
import { FakeOplog, REPO, T } from "../test/oplog-daemon";
import { coalesce } from "./coalesce";
import { bucketWidth, buckets, summarize } from "./counts";
import { OperationWindow } from "./window";

const SCENARIOS = 200;

function random(seed: number) {
  let a = seed >>> 0;
  const next = () => {
    a = (a + 0x6d2b79f5) >>> 0;
    let t = a;
    t = Math.imul(t ^ (t >>> 15), t | 1);
    t ^= t + Math.imul(t ^ (t >>> 7), t | 61);
    return ((t ^ (t >>> 14)) >>> 0) / 2 ** 32;
  };
  return { next, int: (n: number) => Math.floor(next() * n), chance: (p: number) => next() < p };
}

class Harness {
  readonly store = new OperationStore(immediateFrames);
  /** Recorded, not yet delivered on the stream. */
  pending: Operation[] = [];
  readonly frames: Operation[][] = [];
  private cursor = 0;

  constructor(readonly oplog: FakeOplog) {
    this.frame([], true);
  }

  window(): OperationWindow {
    const window = new OperationWindow(
      this.oplog.connection(),
      this.store,
      immediateFrames,
      () => undefined,
      REPO,
    );
    window.start();
    return window;
  }

  frame(operations: Operation[], live = true) {
    this.cursor += 1;
    this.store.applyFrame(operationsFrame({ operations, live, cursor: `c${String(this.cursor)}` }));
    if (operations.length > 0) this.frames.push(operations);
  }

  deliver(count: number) {
    const sent = this.pending.splice(0, count);
    if (sent.length > 0) this.frame(sent);
  }

  replay(rng: ReturnType<typeof random>) {
    const old = this.frames[rng.int(this.frames.length)];
    if (old) this.frame(old);
  }
}

async function quiet(window: OperationWindow) {
  for (let i = 0; i < 40; i++) {
    await settle(50);
    const snap = window.getSnapshot();
    if (snap.records !== null && !snap.loadingOlder && !snap.reloading) {
      await settle(50);
      return;
    }
  }
}

function check(h: Harness, window: OperationWindow, rng: ReturnType<typeof random>, label: string) {
  const snap = window.getSnapshot();
  const records = snap.records ?? [];
  const log = h.oplog.walk().filter((o) => o.id !== h.oplog.rootId);
  // After a GC that no operation or page has met yet, the window may still
  // hold the pre-GC copy, whose ids answer reads: compare up to the rename.
  const canonical = (id: string) => id.slice(0, 14);
  // Each operation of the period once: the top of the log, in its order.
  expect(
    records.map((r) => canonical(r.id)),
    label,
  ).toEqual(log.slice(0, records.length).map((o) => canonical(o.id)));
  if (snap.reachedRoot) expect(records.length, label).toBe(log.length);
  // The five statements.
  const shown = records.filter((r) => !snap.held.has(r.id));
  const rows = coalesce(shown);
  const summary = summarize(shown);
  expect(
    rows.reduce((n, row) => n + row.operations.length, 0),
    label,
  ).toBe(summary.total);
  expect(summary.total + snap.held.size, label).toBe(records.length);
  if (records.length > 0) {
    const width = bucketWidth(records.at(-1)?.endedAt ?? 0, records[0]?.endedAt ?? 0);
    expect(
      buckets(records, width).reduce((n, b) => n + b.count, 0),
      label,
    ).toBe(records.length);
  }
  // The feed at a point: a list at it, cut to the window.
  const point = records[rng.int(records.length)];
  if (point) {
    const held = new Set(records.map((r) => r.id));
    const current = log.find((o) => canonical(o.id) === canonical(point.id))?.id ?? point.id;
    const inWindow = new Set([...held].map(canonical));
    expect(
      window.ancestors(point.id).map((r) => canonical(r.id)),
      label,
    ).toEqual(
      h.oplog
        .walk([current])
        .map((o) => canonical(o.id))
        .filter((id) => inWindow.has(id)),
    );
  }
}

async function scenario(seed: number) {
  const rng = random(seed);
  const oplog = new FakeOplog();
  oplog.pageCap = [1000, 40, 7][rng.int(3)] ?? 1000;
  let clock = T;
  const record = () => {
    clock += rng.int(4) * 60_000 + rng.int(1000);
    const heads = oplog.heads;
    // Two writers: sometimes on one head only (concurrent heads), sometimes a merge.
    // With several heads, usually extend one (concurrent writers), else merge them.
    const parents =
      heads.length > 1 && rng.chance(0.6) ? [heads[rng.int(heads.length)] as string] : heads;
    // Now and then, a writer on a stale head starts a concurrent branch: one
    // of the latest operations, or any older one (below a short window).
    const walked = oplog.walk();
    const concurrent = rng.chance(0.1)
      ? walked[rng.chance(0.5) ? rng.int(3) : rng.int(walked.length)]?.id
      : undefined;
    const kinds = [OperationKind.SNAPSHOT, OperationKind.EXTERNAL, OperationKind.DESCRIBE];
    return oplog.add(
      {
        endedAt: clock,
        kind: kinds[rng.int(kinds.length)] ?? OperationKind.SNAPSHOT,
        workspaceId: ["w1", "w2", ""][rng.int(3)] ?? "",
        agent: ["ann", "bob"][rng.int(2)] ?? "",
      },
      concurrent ? [concurrent] : parents,
    );
  };
  for (let i = rng.int(60); i > 0; i--) record();
  const h = new Harness(oplog);
  let window = h.window();
  await quiet(window);
  for (let step = 0; step < 25; step++) {
    const label = `seed ${String(seed)} step ${String(step)}`;
    const action = rng.int(10);
    if (action <= 3) {
      for (let i = rng.int(8) + 1; i > 0; i--) h.pending.push(record());
      while (h.pending.length > 0) h.deliver(rng.int(4) + 1);
    } else if (action === 4) {
      h.replay(rng);
    } else if (action === 5) {
      // A reset: what was recorded meanwhile is never delivered.
      h.store.reset();
      for (let i = rng.int(4); i > 0; i--) record();
      h.frame([], true);
    } else if (action === 6) {
      oplog.gc();
      if (rng.chance(0.5)) h.frame([record()]);
      else void window.loadOlder();
    } else if (action === 7) {
      void window.loadOlder();
    } else if (action === 8) {
      window.setHolding(rng.chance(0.5));
      h.frame([record()]);
    } else {
      // A reload: a fresh window over the same log.
      window.stop();
      window = h.window();
    }
    await quiet(window);
    check(h, window, rng, label);
  }
  // Reload equivalence: a fresh window, paged as far, holds the same records.
  window.setHolding(false);
  const fresh = h.window();
  await quiet(fresh);
  const size = window.getSnapshot().records?.length ?? 0;
  while ((fresh.getSnapshot().records?.length ?? 0) < size && fresh.getSnapshot().more) {
    await fresh.loadOlder();
    await quiet(fresh);
  }
  // Up to the GC rename, as in `check`.
  const cut = (w: OperationWindow) =>
    (w.getSnapshot().records ?? []).slice(0, size).map((r) => ({
      ...r,
      id: r.id.slice(0, 14),
      parentIds: r.parentIds.map((p) => p.slice(0, 14)),
    }));
  expect(cut(fresh), `seed ${String(seed)} reload`).toEqual(cut(window));
  expect(coalesce(cut(fresh))).toEqual(coalesce(cut(window)));
  window.stop();
  fresh.stop();
}

describe("the window under every interleaving", () => {
  it(`holds each operation once and keeps its counts in ${String(SCENARIOS)} scenarios`, async () => {
    for (let seed = 1; seed <= SCENARIOS; seed++) await scenario(seed);
  }, 120_000);
});
