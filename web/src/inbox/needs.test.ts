import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { InboxItemSchema } from "../gen/reef/api/v1/inbox_pb";
import { inboxTitle, NeedsWatcher } from "./needs";

const item = (key: string) => create(InboxItemSchema, { key });

describe("what needs the person", () => {
  it("reports nothing on opening, then each new key once", () => {
    const watcher = new NeedsWatcher();
    expect(watcher.next([item("a")])).toEqual([]);
    expect(watcher.next([item("a"), item("b")]).map((i) => i.key)).toEqual(["b"]);
    expect(watcher.next([item("a")])).toEqual([]);
    expect(watcher.next([item("a"), item("b")])).toEqual([]);
  });

  it("titles the page with the count", () => {
    expect(inboxTitle(3)).toBe("(3) Inbox — Reef");
    expect(inboxTitle(0)).toBe("Inbox — Reef");
    expect(inboxTitle(1, "reef")).toBe("(1) Inbox · reef — Reef");
  });
});
