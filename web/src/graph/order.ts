// The smartlog's row order (R2.19). jj's order puts the newest commit first,
// so every snapshot would move its stack to the top; here each stack keeps a
// rank for the life of the view, and rows only move when the graph's shape
// changes.
//
// Ranks are lower first. Every placed commit gets its stack's rank and its
// position, kept for the life of the view. A commit seen before keeps both. A
// new commit of a change seen before (a rewrite) takes the rank and position
// of that change's commit that is gone, or else the change's rank: so a head
// keeps its place when it is rewritten, even one commit of a divergent
// change, and a stack keeps it when its head is abandoned (the new head held
// the rank). A new head whose first-parent chain, through nodes with one
// loaded child and the head's own mutability, reaches a ranked commit takes
// that rank: the stack grew. Any other new head is new work: above every rank
// in a live refresh, below every rank otherwise (a first load, or "Show
// more"). A stack pushed past the cut and back returns to its place.
//
// Heads that share a rank (a stack split in two, a merge's second parent)
// keep their previous positions. Only heads with none take the server's
// order, which follows the newest commit and would swap them on every
// snapshot.
//
// Emission is depth first from the heads in rank order: a node is placed once
// all its loaded children are, and then its parents are visited in edge
// order. Each stack is contiguous down to the first fork point it shares with
// a stack not yet placed, and the trunk spine comes last.
import type { GraphNode } from "./node";

/** Where a commit was placed: its stack's rank and its row. */
export interface Placement {
  changeId: string;
  rank: number;
  position: number;
}

/** What one order hands the next: every commit placed so far, by id. */
export interface OrderState {
  placed: ReadonlyMap<string, Placement>;
}

export interface Ordered {
  nodes: GraphNode[];
  state: OrderState;
}

const EMPTY: OrderState = { placed: new Map() };

/** Loaded parents through direct and indirect edges, without repeats. */
function loadedParents(node: GraphNode, byId: Map<string, GraphNode>): GraphNode[] {
  const parents: GraphNode[] = [];
  for (const edge of node.edges) {
    if (edge.kind === "missing") continue;
    const parent = byId.get(edge.parent);
    if (parent && !parents.includes(parent)) parents.push(parent);
  }
  return parents;
}

/**
 * `nodes` in the server's order (a topological order, children first, cut at
 * the limit), placed by the previous order's state. `live` says whether new
 * heads are new work (on top) or older work revealed (below).
 */
export function order(
  nodes: readonly GraphNode[],
  previous: OrderState = EMPTY,
  live = false,
): Ordered {
  const byId = new Map(nodes.map((node) => [node.id, node]));
  const children = new Map<string, number>();
  for (const node of nodes) {
    for (const parent of loadedParents(node, byId)) {
      children.set(parent.id, (children.get(parent.id) ?? 0) + 1);
    }
  }
  const heads = nodes.filter((node) => !children.has(node.id));

  // Previous placements of commits now gone, by change: what a rewrite replaced.
  const gone = new Map<string, Placement>();
  for (const [commit, placement] of previous.placed) {
    if (byId.has(commit)) continue;
    const held = gone.get(placement.changeId);
    if (!held || placement.position < held.position) gone.set(placement.changeId, placement);
  }
  const before = (node: GraphNode): Placement | undefined =>
    previous.placed.get(node.id) ?? gone.get(node.changeId);

  // Each head's rank: its own, an ancestor's through a plain chain, or new.
  const headRank = new Map<GraphNode, number>();
  const fresh: GraphNode[] = [];
  for (const head of heads) {
    let rank = before(head)?.rank;
    for (let node = head; rank === undefined;) {
      const parent = loadedParents(node, byId)[0];
      if (!parent || children.get(parent.id) !== 1 || parent.immutable !== head.immutable) break;
      rank = before(parent)?.rank;
      node = parent;
    }
    if (rank === undefined) fresh.push(head);
    else headRank.set(head, rank);
  }
  let top = 0;
  let bottom = -1;
  const ranks = [...headRank.values(), ...[...previous.placed.values()].map((p) => p.rank)];
  if (ranks.length) {
    top = Number.POSITIVE_INFINITY;
    bottom = Number.NEGATIVE_INFINITY;
    for (const rank of ranks) {
      top = Math.min(top, rank);
      bottom = Math.max(bottom, rank);
    }
  }
  fresh.forEach((head, i) => {
    headRank.set(head, live ? top - fresh.length + i : bottom + 1 + i);
  });

  // Ties keep previous positions; heads without one keep the server's order.
  const index = new Map(nodes.map((node, i) => [node.id, i]));
  const position = (node: GraphNode) => before(node)?.position ?? Number.POSITIVE_INFINITY;
  const sorted = [...heads].sort(
    (a, b) =>
      (headRank.get(a) ?? 0) - (headRank.get(b) ?? 0) ||
      position(a) - position(b) ||
      (index.get(a.id) ?? 0) - (index.get(b.id) ?? 0),
  );

  const remaining = new Map(children);
  const out: GraphNode[] = [];
  // Commits placed before and not now keep their placements, so a stack that
  // leaves the view and returns comes back to its place.
  const placed = new Map(previous.placed);
  const now = new Set<string>();
  // An explicit stack, so a 1,000-commit chain cannot overflow.
  const stack: { node: GraphNode; rank: number }[] = [];
  for (const head of sorted.toReversed()) stack.push({ node: head, rank: headRank.get(head) ?? 0 });
  while (stack.length) {
    const item = stack.pop();
    if (!item) break;
    const { node, rank } = item;
    if (now.has(node.id) || (remaining.get(node.id) ?? 0) > 0) continue;
    now.add(node.id);
    placed.set(node.id, { changeId: node.changeId, rank, position: out.length });
    out.push(node);
    const parents = loadedParents(node, byId);
    for (const parent of parents) remaining.set(parent.id, (remaining.get(parent.id) ?? 0) - 1);
    for (const parent of parents.toReversed()) stack.push({ node: parent, rank });
  }
  // A rewritten commit's old placement is replaced by the new commit's.
  const shown = new Set(out.map((node) => node.changeId));
  for (const [commit, placement] of placed) {
    if (!now.has(commit) && shown.has(placement.changeId)) placed.delete(commit);
  }
  return { nodes: out, state: { placed } };
}
