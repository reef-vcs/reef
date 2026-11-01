// Lanes for the smartlog (R2.19), in the style of jj's and Sapling's
// renderdag: a pure function over ordered rows, O(rows × lanes).
//
// A lane is a line waiting for its target row. At each row, the lanes that
// target the node arrive: the node takes the leftmost, and the others join it
// from above and are freed. Then each edge, in order: a missing edge draws a
// stub; an edge whose parent a lane of the same kind already awaits joins
// that lane; the first other edge continues in the node's own lane, and each
// later one takes the first free lane to its right. Every other lane passes
// straight through, and trailing free lanes are dropped. A lane keeps the
// kind of the edge that opened it, so an elided range is dashed over its
// whole height.
import type { EdgeKind, GraphNode } from "./node";

export type LaneKind = Exclude<EdgeKind, "missing">;

export interface GraphRow {
  node: GraphNode;
  /** The node's lane. */
  column: number;
  /** The kind of the lane arriving at the node from above in its own column, if any. */
  incoming: LaneKind | null;
  /** Other lanes that end at the node, from their columns above it. */
  joins: { from: number; kind: LaneKind }[];
  /** Lanes that pass the row without touching the node. */
  passing: { column: number; kind: LaneKind }[];
  /** Edges leaving the node downward, into these columns. */
  edges: { to: number; kind: LaneKind }[];
  /** A parent outside the revset: a short stub below the node. */
  stub: boolean;
  /** Columns the row needs. */
  width: number;
}

export interface Layout {
  rows: GraphRow[];
  /** Lanes still open after the last row: parents beyond the cut. */
  open: { column: number; kind: LaneKind; target: string }[];
  /** The widest row's columns. */
  width: number;
}

interface Lane {
  target: string;
  kind: LaneKind;
}

function firstFree(lanes: (Lane | null)[], from: number): number {
  for (let i = from; i < lanes.length; i++) if (lanes[i] === null) return i;
  return lanes.length;
}

export function layout(nodes: readonly GraphNode[]): Layout {
  const lanes: (Lane | null)[] = [];
  const rows: GraphRow[] = [];
  let widest = 0;
  for (const node of nodes) {
    const arriving: number[] = [];
    const passing: { column: number; kind: LaneKind }[] = [];
    for (const [i, lane] of lanes.entries()) {
      if (!lane) continue;
      if (lane.target === node.id) arriving.push(i);
      else passing.push({ column: i, kind: lane.kind });
    }
    const column = arriving[0] ?? firstFree(lanes, 0);
    const incoming = arriving.length ? (lanes[column]?.kind ?? null) : null;
    const joins = arriving.slice(1).map((from) => ({ from, kind: lanes[from]?.kind ?? "direct" }));
    const widthAbove = lanes.length;
    for (const i of arriving) lanes[i] = null;
    if (column === lanes.length) lanes.push(null);

    const edges: { to: number; kind: LaneKind }[] = [];
    let stub = false;
    let ownTaken = false;
    for (const edge of node.edges) {
      if (edge.kind === "missing") {
        stub = true;
        continue;
      }
      const kind = edge.kind;
      const waiting = lanes.findIndex(
        (lane, i) => i !== column && lane?.target === edge.parent && lane.kind === kind,
      );
      if (waiting >= 0) {
        edges.push({ to: waiting, kind });
        continue;
      }
      if (lanes[column] && lanes[column].target === edge.parent) continue;
      let to = column;
      if (ownTaken) {
        to = firstFree(lanes, column + 1);
        if (to === lanes.length) lanes.push(null);
      }
      ownTaken = true;
      lanes[to] = { target: edge.parent, kind };
      edges.push({ to, kind });
    }
    while (lanes.length && lanes.at(-1) === null) lanes.pop();
    const width = Math.max(widthAbove, column + 1, lanes.length);
    widest = Math.max(widest, width);
    rows.push({ node, column, incoming, joins, passing, edges, stub, width });
  }
  const open = lanes.flatMap((lane, column) =>
    lane ? [{ column, kind: lane.kind, target: lane.target }] : [],
  );
  return { rows, open, width: widest };
}
