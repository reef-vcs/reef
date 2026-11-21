// #372: the inbox stream folded by key, a new stream's first inbox replacing
// the held one once complete, and the held inbox as one frame for a tab that
// joins.
import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import {
  InboxBucket,
  InboxItemSchema,
  WatchInboxResponseSchema,
  type InboxItem,
} from "../gen/reef/api/v1/inbox_pb";
import { immediateFrames } from "../test/fakes";
import { InboxStore } from "./inbox";

const item = (key: string, bucket = InboxBucket.NEEDS_YOU_NOW, headline = "asks"): InboxItem =>
  create(InboxItemSchema, { key, bucket, headline });

const frame = (
  sections: { repoId: string; items?: InboxItem[]; left?: string[]; removed?: boolean }[],
  synced = true,
) => create(WatchInboxResponseSchema, { sections, synced });

const keys = (store: InboxStore, repo: string) => [
  ...(store.sections.get(repo)?.items.keys() ?? []),
];

describe("the inbox store", () => {
  it("folds items in, changes them by key, and drops what left or was removed", () => {
    const store = new InboxStore(immediateFrames);
    store.beginResync();
    store.applyFrame(frame([{ repoId: "r", items: [item("a"), item("b")] }, { repoId: "s" }]));
    expect(store.synced).toBe(true);
    expect(keys(store, "r")).toEqual(["a", "b"]);
    store.applyFrame(
      frame([{ repoId: "r", items: [item("b", InboxBucket.WATCHING, "moved")], left: ["a"] }]),
    );
    expect(keys(store, "r")).toEqual(["b"]);
    expect(store.sections.get("r")?.items.get("b")?.bucket).toBe(InboxBucket.WATCHING);
    store.applyFrame(frame([{ repoId: "s", removed: true }]));
    expect([...store.sections.keys()]).toEqual(["r"]);
  });

  it("keeps the held inbox until a new stream's first inbox is complete", () => {
    const store = new InboxStore(immediateFrames);
    store.beginResync();
    store.applyFrame(frame([{ repoId: "r", items: [item("a")] }]));
    store.beginResync();
    store.applyFrame(frame([{ repoId: "r", items: [item("b")] }], false));
    expect(keys(store, "r"), "the old inbox stands").toEqual(["a"]);
    store.applyFrame(frame([{ repoId: "s", items: [item("c")] }]));
    expect(keys(store, "r"), "the new one replaces it").toEqual(["b"]);
    expect(keys(store, "s")).toEqual(["c"]);
  });

  it("round-trips the held inbox as one frame", () => {
    const store = new InboxStore(immediateFrames);
    store.beginResync();
    store.applyFrame(frame([{ repoId: "r", items: [item("a"), item("b")] }]));
    const joiner = new InboxStore(immediateFrames);
    joiner.beginResync();
    joiner.applyFrame(store.snapshotFrame());
    expect(joiner.synced).toBe(true);
    expect(keys(joiner, "r")).toEqual(["a", "b"]);
    store.beginResync();
    expect(store.snapshotFrame().synced, "mid-resync, a joiner waits for the rest").toBe(false);
  });
});
