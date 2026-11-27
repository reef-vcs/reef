// A bucket of the inbox held in place (R3.33, the design's live rules):
// nothing reorders under the cursor. The first read shows every item; later
// reads replace a shown row's item in place, keep a row whose item left the
// bucket (dimmed, with what happened), and put items not shown yet behind a
// count until the person shows them. Keys are the daemon's
// (`InboxItem.key`), stable across reads.
import type { InboxItem } from "../gen/reef/api/v1/inbox_pb";

/** What happened to a shown row whose item left its bucket. */
export type Departure = "moved" | "answered" | "done";

export interface HeldRow {
  key: string;
  item: InboxItem;
  /** Null while the item is still in the bucket. */
  departed: Departure | null;
}

export interface HeldBucket {
  /** In display order; fixed until `showNew`. */
  readonly rows: readonly HeldRow[];
  /** Items not shown yet, newest first: the header's count. */
  readonly fresh: readonly InboxItem[];
  /** The daemon's totals over every repository read. */
  readonly total: number;
}

export interface ReadFacts {
  /** This bucket's items now, newest first. */
  items: readonly InboxItem[];
  total: number;
  /** Keys now in the other bucket. */
  elsewhere: ReadonlySet<string>;
  /** Keys this page answered. */
  answered: ReadonlySet<string>;
}

/** The bucket after a read; `held` is null for the first. */
export function holdRead(held: HeldBucket | null, facts: ReadFacts): HeldBucket {
  if (held === null) {
    return {
      rows: facts.items.map((item) => ({ key: item.key, item, departed: null })),
      fresh: [],
      total: facts.total,
    };
  }
  const now = new Map(facts.items.map((item) => [item.key, item]));
  const shown = new Set(held.rows.map((row) => row.key));
  const rows = held.rows.map((row): HeldRow => {
    const item = now.get(row.key);
    if (item) return { key: row.key, item, departed: null };
    const departed: Departure = facts.answered.has(row.key)
      ? "answered"
      : facts.elsewhere.has(row.key)
        ? "moved"
        : "done";
    return { ...row, departed };
  });
  const fresh = facts.items.filter((item) => !shown.has(item.key));
  return { rows, fresh, total: facts.total };
}

/**
 * The page answered `key`: a row that left before the answer's response came
 * back says "Answered", not why it left. A row still in the bucket keeps its
 * item, and says "Answered" when it leaves.
 */
export function sayAnswered(held: HeldBucket, key: string): HeldBucket {
  const rows = held.rows.map((row) =>
    row.key === key && row.departed !== null && row.departed !== "answered"
      ? { ...row, departed: "answered" as const }
      : row,
  );
  const changed = rows.some((row, index) => row !== held.rows[index]);
  return changed ? { ...held, rows } : held;
}

/** "Show": new items go on top, newest first, and rows that left go. */
export function showNew(held: HeldBucket): HeldBucket {
  return {
    rows: [
      ...held.fresh.map((item) => ({ key: item.key, item, departed: null })),
      ...held.rows.filter((row) => row.departed === null),
    ],
    fresh: [],
    total: held.total,
  };
}

/** Rows that left, which "Show" clears. */
export function departedCount(held: HeldBucket): number {
  return held.rows.filter((row) => row.departed !== null).length;
}
