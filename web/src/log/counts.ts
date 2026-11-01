// The activity feed's counts (R2.21): the summary of the period shown and
// the scrubber's bars. Each is a count over records the window holds once
// each, so they are right by completeness; nothing here reads a clock.
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { isRoot, type OpRecord } from "./record";

export interface Summary {
  total: number;
  /** Counts by kind, in the order the summary shows them; zeros left out. */
  kinds: [label: string, count: number][];
  agents: number;
  workspaces: number;
  /** Milliseconds; undefined when there are no operations. */
  oldest: number | undefined;
  newest: number | undefined;
}

const KIND_WORDS: [OperationKind, string, string][] = [
  [OperationKind.SNAPSHOT, "snapshot", "snapshots"],
  [OperationKind.EDIT, "edit", "edits"],
  [OperationKind.EXTERNAL, "jj", "jj"],
  [OperationKind.CREATE_WORKSPACE, "create", "creates"],
  [OperationKind.RELEASE_WORKSPACE, "release", "releases"],
  [OperationKind.MOVE_WORKSPACE, "move", "moves"],
  [OperationKind.DESCRIBE, "describe", "describes"],
  [OperationKind.ABANDON, "abandon", "abandons"],
  [OperationKind.UNDO, "undo", "undos"],
  [OperationKind.IMPORT_REFS, "ref import", "ref imports"],
  [OperationKind.OTHER, "other", "other"],
  [OperationKind.UNSPECIFIED, "unclassified", "unclassified"],
];

export function summarize(records: Iterable<OpRecord>): Summary {
  const byKind = new Map<OperationKind, number>();
  const agents = new Set<string>();
  const workspaces = new Set<string>();
  let total = 0;
  let oldest: number | undefined;
  let newest: number | undefined;
  for (const record of records) {
    if (isRoot(record)) continue;
    total += 1;
    byKind.set(record.kind, (byKind.get(record.kind) ?? 0) + 1);
    if (record.agent) agents.add(record.agent);
    if (record.workspaceId) workspaces.add(record.workspaceId);
    if (oldest === undefined || record.endedAt < oldest) oldest = record.endedAt;
    if (newest === undefined || record.endedAt > newest) newest = record.endedAt;
  }
  const kinds: [string, number][] = [];
  for (const [kind, one, many] of KIND_WORDS) {
    const count = byKind.get(kind) ?? 0;
    if (count > 0) kinds.push([count === 1 ? one : many, count]);
  }
  // A kind newer than this build's schema.
  const known = new Set(KIND_WORDS.map(([kind]) => kind));
  const unknown = [...byKind].filter(([kind]) => !known.has(kind)).reduce((n, [, c]) => n + c, 0);
  if (unknown > 0) kinds.push(["unknown", unknown]);
  return { total, kinds, agents: agents.size, workspaces: workspaces.size, oldest, newest };
}

/** The bucket widths the scrubber's track may use, smallest first. */
export const BUCKET_LADDER_MS = [
  1_000,
  10_000,
  60_000,
  5 * 60_000,
  15 * 60_000,
  3_600_000,
  6 * 3_600_000,
  86_400_000,
];

/** Bars on the track at most. */
export const MAX_BUCKETS = 120;

/**
 * The smallest width on the ladder that keeps `[oldest, newest]` within
 * `MAX_BUCKETS` buckets aligned to multiples of the width; the largest
 * otherwise.
 */
export function bucketWidth(oldest: number, newest: number): number {
  for (const width of BUCKET_LADDER_MS) {
    if (Math.floor(newest / width) - Math.floor(oldest / width) + 1 <= MAX_BUCKETS) return width;
  }
  return BUCKET_LADDER_MS[BUCKET_LADDER_MS.length - 1] ?? 86_400_000;
}

export interface Bucket {
  /** Start, in milliseconds, a multiple of the width. */
  start: number;
  count: number;
}

/**
 * Operations per bucket from the oldest record's bucket to the newest's,
 * empty buckets included. Aligned to multiples of `width`, so one more
 * operation in the newest bucket changes only that bucket.
 */
export function buckets(records: Iterable<OpRecord>, width: number): Bucket[] {
  const counts = new Map<number, number>();
  let first: number | undefined;
  let last: number | undefined;
  for (const record of records) {
    if (isRoot(record)) continue;
    const index = Math.floor(record.endedAt / width);
    counts.set(index, (counts.get(index) ?? 0) + 1);
    if (first === undefined || index < first) first = index;
    if (last === undefined || index > last) last = index;
  }
  if (first === undefined || last === undefined) return [];
  const out: Bucket[] = [];
  for (let index = first; index <= last; index++) {
    out.push({ start: index * width, count: counts.get(index) ?? 0 });
  }
  return out;
}
