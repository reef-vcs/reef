// @vitest-environment node
// R2.19 test 7: invariants of the order and the layout over seeded random
// graphs, and the time to lay out 1,000 rows.
import { describe, expect, it } from "vitest";
import { layout, type GraphRow } from "./layout";
import type { EdgeKind, GraphNode } from "./node";
import { order } from "./order";

/** mulberry32: small, seeded, and the same on every machine. */
function random(seed: number): () => number {
  let a = seed;
  return () => {
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/**
 * A DAG of up to `max` nodes in a topological order (children first), with
 * merges, indirect and missing edges, cut at a random prefix.
 */
function randomGraph(seed: number, max: number): GraphNode[] {
  const rand = random(seed);
  const count = 1 + Math.floor(rand() * max);
  // Built oldest first; node i's parents are older nodes.
  const nodes: GraphNode[] = [];
  for (let i = 0; i < count; i++) {
    const edges: { parent: string; kind: EdgeKind }[] = [];
    if (i > 0) {
      const parents = rand() < 0.15 ? 2 + Math.floor(rand() * 2) : 1;
      for (let p = 0; p < parents; p++) {
        const parent = `n${String(Math.floor(rand() * i))}`;
        if (edges.some((e) => e.parent === parent)) continue;
        const r = rand();
        edges.push({ parent, kind: r < 0.2 ? "indirect" : r < 0.27 ? "missing" : "direct" });
      }
    } else if (rand() < 0.5) {
      edges.push({ parent: "outside", kind: "missing" });
    }
    // Some commits are another commit's change again: a divergent change.
    const changeId =
      i > 2 && rand() < 0.08 ? `c${String(Math.floor(rand() * i))}` : `c${String(i)}`;
    nodes.push({ id: `n${String(i)}`, changeId, edges, immutable: rand() < 0.2 });
  }
  const server = nodes.toReversed();
  // The limit cuts a prefix of the server's order.
  return rand() < 0.3 ? server.slice(0, 1 + Math.floor(rand() * server.length)) : server;
}

function check(nodes: GraphNode[]) {
  const ordered = order(nodes).nodes;
  expect(new Set(ordered.map((n) => n.id))).toEqual(new Set(nodes.map((n) => n.id)));
  const graph = layout(ordered);
  const rowOf = new Map(graph.rows.map((row, i) => [row.node.id, i]));

  for (const row of graph.rows) {
    // The node and the lanes passing it each have a column of their own.
    const columns = [row.column, ...row.passing.map((p) => p.column)];
    expect(new Set(columns).size).toBe(columns.length);
    // Edges may join a passing lane that waits for the same parent;
    // followsTo below checks each one reaches its parent.
    const opened = row.edges.map((e) => e.to);
    // The row is exactly as wide as the columns it draws.
    const used = [
      row.column,
      ...row.passing.map((p) => p.column),
      ...opened,
      ...row.joins.map((j) => j.from),
    ];
    expect(row.width).toBe(Math.max(...used) + 1);
    expect(row.width).toBeLessThanOrEqual(graph.width);
  }

  for (const [i, row] of graph.rows.entries()) {
    for (const edge of row.node.edges) {
      if (edge.kind === "missing") {
        expect(row.stub).toBe(true);
        continue;
      }
      const target = rowOf.get(edge.parent);
      const leaving = row.edges.find((e) => followsTo(graph.rows, i, e, edge.parent));
      if (target === undefined) {
        // Beyond the cut: the lane stays open past the last row.
        expect(leaving).toBeDefined();
        continue;
      }
      expect(target).toBeGreaterThan(i);
      expect(leaving?.kind).toBe(edge.kind);
    }
  }

  for (const lane of graph.open) expect(rowOf.has(lane.target)).toBe(false);
}

/**
 * Snapshots of random heads, each a new commit of the same change listed
 * first by the server (as jj's order does), keep every change where it was.
 */
function checkStable(seed: number, nodes: GraphNode[]) {
  const rand = random(seed * 7919);
  let server = nodes;
  let ordered = order(server);
  for (let step = 0; step < 6; step++) {
    const parents = new Set(server.flatMap((n) => n.edges.map((e) => e.parent)));
    const heads = server.filter((n) => !parents.has(n.id) && !n.immutable);
    const head = heads[Math.floor(rand() * heads.length)];
    if (!head) return;
    const rewritten = { ...head, id: `${head.id}'` };
    server = [rewritten, ...server.filter((n) => n !== head)];
    const before = ordered.nodes.map((n) => n.changeId);
    ordered = order(server, ordered.state, true);
    const after = ordered.nodes.map((n) => n.changeId);
    if (after.join() !== before.join()) {
      const diffAt = after.findIndex((c, i) => c !== before[i]);
      throw new Error(
        `step ${String(step)} rewrote ${head.id} (${head.changeId}); first difference at ${String(diffAt)}: before ${before.slice(diffAt, diffAt + 6).join(",")} after ${after.slice(diffAt, diffAt + 6).join(",")}; dup change ids: ${String(nodes.length - new Set(nodes.map((n) => n.changeId)).size)}`,
      );
    }
  }
}

/**
 * Whether the edge leaving row `i` into column `edge.to` reaches the row of
 * `parent` through lanes of its own kind: passing each row between, then
 * arriving in the node's column or joining it.
 */
function followsTo(
  rows: GraphRow[],
  i: number,
  edge: { to: number; kind: string },
  parent: string,
): boolean {
  for (let r = i + 1; r < rows.length; r++) {
    const row = rows[r];
    if (!row) return false;
    if (row.node.id === parent) {
      if (row.column === edge.to) return row.incoming === edge.kind;
      return row.joins.some((j) => j.from === edge.to && j.kind === edge.kind);
    }
    if (!row.passing.some((p) => p.column === edge.to && p.kind === edge.kind)) return false;
  }
  return true;
}

describe("layout invariants", () => {
  it("hold over 500 seeded random graphs, and survive snapshots", () => {
    for (let seed = 1; seed <= 500; seed++) {
      const nodes = randomGraph(seed, 60);
      try {
        check(nodes);
        checkStable(seed, nodes);
      } catch (error) {
        throw new Error(`seed ${String(seed)}: ${String(error)}`, { cause: error });
      }
    }
  });

  it("orders and lays out 1,000 nodes in 50 stacks within 200 ms", () => {
    const nodes: GraphNode[] = [];
    // 50 stacks of 19 on a trunk of 50; server order: stacks, then trunk.
    for (let s = 49; s >= 0; s--) {
      for (let d = 18; d >= 0; d--) {
        const parent = d === 0 ? `t${String(s)}` : `s${String(s)}-${String(d - 1)}`;
        nodes.push({
          id: `s${String(s)}-${String(d)}`,
          changeId: `s${String(s)}-${String(d)}`,
          edges: [{ parent, kind: "direct" }],
          immutable: false,
        });
      }
    }
    for (let t = 49; t >= 0; t--) {
      nodes.push({
        id: `t${String(t)}`,
        changeId: `t${String(t)}`,
        edges: t === 0 ? [] : [{ parent: `t${String(t - 1)}`, kind: "indirect" }],
        immutable: true,
      });
    }
    expect(nodes).toHaveLength(1000);
    const start = performance.now();
    const graph = layout(order(nodes).nodes);
    const ms = performance.now() - start;
    console.log(`order and layout, 1,000 nodes: ${ms.toFixed(1)} ms`);
    expect(graph.rows).toHaveLength(1000);
    expect(ms).toBeLessThan(200);
  });
});
