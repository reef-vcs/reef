// "Landed since you last looked" over one visit (#372, ADR 0091; the user's
// decision of 2026-11-21). The stream's digest follows the person's mark
// live. Once this page marks a digest seen, what it showed stays on screen
// for the rest of the visit and later landings add to it. A mark that moves
// past where this page marked (another device, another tab) is the person
// having looked elsewhere: the digest starts again from it. A landing undone
// after this page banked it stays counted until the next visit.
import { create } from "@bufbuild/protobuf";
import {
  InboxAgentCountSchema,
  LandedDigestSchema,
  type LandedDigest,
} from "../gen/reef/api/v1/inbox_pb";

/** Landings and agents a digest lists, as the daemon cuts them. */
const LISTED = 10;

export interface VisitDigest {
  /** What the page showed when it marked seen, merged across marks. */
  banked: LandedDigest | null;
  /** The stream's digest, from the mark. */
  tail: LandedDigest;
}

/**
 * The visit after the stream sent `incoming`; `marked` holds, by seq, the
 * digest this page showed when it marked that seq seen.
 */
export function holdDigest(
  held: VisitDigest | null,
  incoming: LandedDigest,
  marked: ReadonlyMap<bigint, LandedDigest>,
): VisitDigest {
  if (held === null || incoming.fromSeq <= held.tail.fromSeq) {
    return { banked: held?.banked ?? null, tail: incoming };
  }
  // The mark moved. This page's own mark banks what it showed, which ends
  // at the mark, so nothing is counted twice; a mark past it was set
  // elsewhere, and the digest starts again from it.
  return { banked: marked.get(incoming.fromSeq) ?? null, tail: incoming };
}

/** What the page shows. */
export function shown(visit: VisitDigest): LandedDigest {
  return visit.banked ? merge(visit.banked, visit.tail) : visit.tail;
}

/** `older` then `newer`, which starts where `older` ends, as one digest from `older`'s base. */
function merge(older: LandedDigest, newer: LandedDigest): LandedDigest {
  const agents = new Map<string, bigint>();
  for (const count of [...older.byAgent, ...newer.byAgent]) {
    agents.set(count.agent, (agents.get(count.agent) ?? 0n) + count.count);
  }
  const byAgent = [...agents]
    .sort(([a, x], [b, y]) => (x === y ? a.localeCompare(b) : x > y ? -1 : 1))
    .slice(0, LISTED)
    .map(([agent, count]) => create(InboxAgentCountSchema, { agent, count }));
  return create(LandedDigestSchema, {
    count: older.count + newer.count,
    byAgent,
    newest: [...newer.newest, ...older.newest].slice(0, LISTED),
    fromSeq: older.fromSeq,
    throughSeq: newer.throughSeq > older.throughSeq ? newer.throughSeq : older.throughSeq,
    since: older.since,
    fromMark: older.fromMark,
  });
}
