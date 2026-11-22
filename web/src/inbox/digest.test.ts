// #372, the user's decision of 2026-11-21: the digest follows the mark, and a
// page keeps what it showed once it marked it seen.
import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { LandedDigestSchema, type LandedDigest } from "../gen/reef/api/v1/inbox_pb";
import { holdDigest, shown } from "./digest";

const digest = (from: bigint, through: bigint, agent = "claude@a"): LandedDigest =>
  create(LandedDigestSchema, {
    fromSeq: from,
    throughSeq: through,
    count: through - from,
    byAgent: through > from ? [{ agent, count: through - from }] : [],
    newest: through > from ? [{ changeId: String(through), seq: through, agent }] : [],
  });

const none: ReadonlyMap<bigint, LandedDigest> = new Map();
/** The page marked each `[seq, what it showed]`. */
const marks = (...shown: [bigint, LandedDigest][]) => new Map(shown);

describe("a visit's digest", () => {
  it("follows the stream while the mark stands", () => {
    const visit = holdDigest(holdDigest(null, digest(0n, 2n), none), digest(0n, 3n), none);
    expect(shown(visit).count).toBe(3n);
  });

  it("banks what it showed when this page marked, and adds later landings", () => {
    let visit = holdDigest(null, digest(0n, 2n), none);
    visit = holdDigest(visit, digest(2n, 2n), marks([2n, digest(0n, 2n)]));
    expect(shown(visit).count, "the marked digest stays").toBe(2n);
    visit = holdDigest(visit, digest(2n, 3n), marks([2n, digest(0n, 2n)]));
    const now = shown(visit);
    expect(now.count).toBe(3n);
    expect(now.fromSeq, "the visit's base").toBe(0n);
    expect(now.throughSeq).toBe(3n);
    expect(now.newest.map((landing) => landing.seq)).toEqual([3n, 2n]);
    expect(now.byAgent).toEqual([expect.objectContaining({ agent: "claude@a", count: 3n })]);
    // A second mark in the same visit banks again.
    visit = holdDigest(visit, digest(3n, 3n), marks([2n, digest(0n, 2n)], [3n, shown(visit)]));
    expect(shown(visit).count).toBe(3n);
  });

  it("banks a mark the stream reports late, after the page marked again", () => {
    let visit = holdDigest(null, digest(0n, 2n), none);
    visit = holdDigest(visit, digest(0n, 3n), none);
    // The page marked 2, then 3, before the stream's digest from 2 came.
    const both = marks([2n, digest(0n, 2n)], [3n, digest(0n, 3n)]);
    visit = holdDigest(visit, digest(2n, 3n), both);
    expect(shown(visit).count).toBe(3n);
    visit = holdDigest(visit, digest(3n, 3n), both);
    expect(shown(visit).count).toBe(3n);
  });

  it("banks only what it showed when the stream moved on before the mark came back", () => {
    let visit = holdDigest(null, digest(0n, 2n), none);
    // The page marked 2; 18 more landed before the stream's digest from 2.
    visit = holdDigest(visit, digest(0n, 20n), none);
    visit = holdDigest(visit, digest(2n, 20n), marks([2n, digest(0n, 2n)]));
    expect(shown(visit).count, "each landing once, past the 10 listed").toBe(20n);
    expect(shown(visit).fromSeq).toBe(0n);
  });

  it("starts again from a mark set elsewhere", () => {
    let visit = holdDigest(null, digest(0n, 2n), none);
    visit = holdDigest(visit, digest(5n, 6n), marks([2n, digest(0n, 2n)]));
    expect(shown(visit).count).toBe(1n);
    expect(shown(visit).fromSeq).toBe(5n);
  });
});
