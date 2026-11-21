import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { InboxItemSchema, type InboxItem } from "../gen/reef/api/v1/inbox_pb";
import { departedCount, holdRead, showNew } from "./held";

const item = (key: string, headline = key): InboxItem => create(InboxItemSchema, { key, headline });
const none = new Set<string>();
const keys = (rows: readonly { key: string }[]) => rows.map((row) => row.key);

describe("a held bucket", () => {
  it("shows the first read in order, then holds new items behind a count", () => {
    const first = holdRead(null, {
      items: [item("b"), item("a")],
      total: 2,
      elsewhere: none,
      answered: none,
    });
    expect(keys(first.rows)).toEqual(["b", "a"]);
    const second = holdRead(first, {
      items: [item("c"), item("b", "b again"), item("a")],
      total: 3,
      elsewhere: none,
      answered: none,
    });
    expect(keys(second.rows)).toEqual(["b", "a"]);
    expect(second.rows[0]?.item.headline).toBe("b again");
    expect(keys(second.fresh)).toEqual(["c"]);
    expect(keys(showNew(second).rows)).toEqual(["c", "b", "a"]);
  });

  it("keeps a row that left in place, saying why, until shown", () => {
    const first = holdRead(null, {
      items: [item("c"), item("b"), item("a")],
      total: 3,
      elsewhere: none,
      answered: none,
    });
    const after = holdRead(first, {
      items: [item("a")],
      total: 1,
      elsewhere: new Set(["b"]),
      answered: new Set(["c"]),
    });
    expect(after.rows.map((row) => [row.key, row.departed])).toEqual([
      ["c", "answered"],
      ["b", "moved"],
      ["a", null],
    ]);
    expect(departedCount(after)).toBe(2);
    expect(keys(showNew(after).rows)).toEqual(["a"]);
  });

  it("brings a departed row back in place when its item returns", () => {
    const first = holdRead(null, { items: [item("a")], total: 1, elsewhere: none, answered: none });
    const gone = holdRead(first, { items: [], total: 0, elsewhere: none, answered: none });
    const back = holdRead(gone, { items: [item("a")], total: 1, elsewhere: none, answered: none });
    expect(back.rows.map((row) => row.departed)).toEqual([null]);
    expect(back.fresh).toEqual([]);
  });
});
