// R2.21 test 3: the summary's counts and the scrubber's buckets.
import { describe, expect, it } from "vitest";
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { BUCKET_LADDER_MS, bucketWidth, buckets, MAX_BUCKETS, summarize } from "./counts";
import { rec, reefLog } from "./fixture";

const T = Date.UTC(2026, 10, 1, 12);

describe("summarize", () => {
  it("counts kinds, agents, workspaces and the period's ends, leaving out the root", () => {
    const summary = summarize([
      rec("root", 0, { parentIds: [] }),
      rec("a", T, { workspaceId: "w1", agent: "ann" }),
      rec("b", T + 5, { workspaceId: "w1", agent: "bob" }),
      rec("c", T + 9, { kind: OperationKind.EXTERNAL }),
      rec("d", T + 2, { kind: OperationKind.CREATE_WORKSPACE, workspaceId: "w2", agent: "ann" }),
    ]);
    expect(summary).toEqual({
      total: 4,
      kinds: [
        ["snapshots", 2],
        ["jj", 1],
        ["create", 1],
      ],
      agents: 2,
      workspaces: 2,
      oldest: T,
      newest: T + 9,
    });
  });

  it("is empty for the root alone", () => {
    expect(summarize([rec("root", 0, { parentIds: [] })])).toMatchObject({
      total: 0,
      kinds: [],
      oldest: undefined,
    });
  });
});

describe("buckets", () => {
  it("picks the smallest width on the ladder within the bucket limit", () => {
    expect(bucketWidth(T, T)).toBe(1_000);
    expect(bucketWidth(T, T + 119_000)).toBe(1_000);
    expect(bucketWidth(T, T + 120_000)).toBe(10_000);
    expect(bucketWidth(T, T + 3 * 3_600_000)).toBe(5 * 60_000);
    expect(bucketWidth(T, T + 1_000 * 86_400_000)).toBe(BUCKET_LADDER_MS.at(-1));
  });

  it("aligns buckets to multiples of the width and keeps within the limit", () => {
    const log = reefLog();
    const times = log.map((r) => r.endedAt);
    const width = bucketWidth(Math.min(...times), Math.max(...times));
    const bars = buckets(log, width);
    expect(bars.length).toBeLessThanOrEqual(MAX_BUCKETS);
    for (const bar of bars) expect(bar.start % width).toBe(0);
    expect(bars.reduce((n, b) => n + b.count, 0)).toBe(log.length);
  });

  it("changes only the newest bucket for one more operation in it", () => {
    const log = reefLog();
    const times = log.map((r) => r.endedAt);
    const newest = Math.max(...times);
    const width = bucketWidth(Math.min(...times), newest);
    const before = buckets(log, width);
    const after = buckets([...log, rec("new", newest)], width);
    expect(after.slice(0, -1)).toEqual(before.slice(0, -1));
    expect(after.at(-1)?.count).toBe((before.at(-1)?.count ?? 0) + 1);
  });
});
