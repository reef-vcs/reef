// @vitest-environment node
// R2.19 test 5: the smartlog's order keeps stacks in place.
import { describe, expect, it } from "vitest";
import { logEntries, type CliLogEntry } from "../test/log-json";
import { dag } from "../test/dag";
import fixture from "./fixtures/reef-smartlog.json";
import { graphNode, type GraphNode } from "./node";
import { order, type OrderState } from "./order";

const ids = (nodes: GraphNode[]) => nodes.map((n) => n.id);

function place(text: string, previous?: OrderState, live = false) {
  return order(dag(text), previous, live);
}

/** Children before parents, every node once. */
function topological(nodes: GraphNode[]) {
  const at = new Map(nodes.map((n, i) => [n.id, i]));
  expect(at.size).toBe(nodes.length);
  for (const [i, node] of nodes.entries()) {
    for (const edge of node.edges) {
      const parent = at.get(edge.parent);
      if (edge.kind !== "missing" && parent !== undefined) expect(parent).toBeGreaterThan(i);
    }
  }
}

const TWO_STACKS = `
a2 a1
a1 t4
b1 t4
t4! :t2
t2! ~t1
`;

describe("order", () => {
  it("places children before parents, every node once", () => {
    const { nodes } = place(TWO_STACKS);
    topological(nodes);
    expect(ids(nodes)).toEqual(["a2", "a1", "b1", "t4", "t2"]);
  });

  it("keeps each stack contiguous down to its fork point, the spine last", () => {
    const { nodes } = place(`
      b2 b1
      a2 a1
      b1 t2
      a1 t4
      t4! :t2
      t2! ~t1
    `);
    expect(ids(nodes)).toEqual(["b2", "b1", "a2", "a1", "t4", "t2"]);
  });

  it("places heads in rank order", () => {
    const state = {
      placed: new Map([
        ["b1", { changeId: "b1", rank: 0, position: 0 }],
        ["a2", { changeId: "a2", rank: 1, position: 1 }],
      ]),
    };
    expect(ids(place(TWO_STACKS, state).nodes)).toEqual(["b1", "a2", "a1", "t4", "t2"]);
  });

  it("keeps a rewritten head's place when the server lists it first", () => {
    const first = place(`
      a2 a1
      b1 t4
      a1 t4
      t4!
    `);
    expect(ids(first.nodes)).toEqual(["a2", "a1", "b1", "t4"]);
    // b1 was snapshotted: a new commit of the same change, now newest.
    const next = place(
      `
      b1x/b1 t4
      a2 a1
      a1 t4
      t4!
    `,
      first.state,
      true,
    );
    expect(ids(next.nodes)).toEqual(["a2", "a1", "b1x", "t4"]);
  });

  it("keeps a grown stack's place", () => {
    const first = place(`
      a2 a1
      a1 t4
      b1 t4
      t4!
    `);
    const next = place(
      `
      b2 b1
      a2 a1
      b1 t4
      a1 t4
      t4!
    `,
      first.state,
      true,
    );
    expect(ids(next.nodes)).toEqual(["a2", "a1", "b2", "b1", "t4"]);
  });

  it("keeps a stack's place when its head is abandoned", () => {
    const first = place(`
      a2 a1
      a1 t4
      b2 b1
      b1 t4
      t4!
    `);
    const next = place(
      `
      b1 t4
      a2 a1
      a1 t4
      t4!
    `,
      first.state,
      true,
    );
    expect(ids(next.nodes)).toEqual(["a2", "a1", "b1", "t4"]);
  });

  it("puts a new stack on top in a live refresh, and below for a larger limit", () => {
    const first = place(TWO_STACKS);
    const text = `
      c1 t4
      ${TWO_STACKS}
    `;
    expect(ids(place(text, first.state, true).nodes)).toEqual(["c1", "a2", "a1", "b1", "t4", "t2"]);
    expect(ids(place(text, first.state, false).nodes)).toEqual([
      "a2",
      "a1",
      "b1",
      "c1",
      "t4",
      "t2",
    ]);
  });

  it("does not give a new stack on a stack's commit that stack's rank", () => {
    const first = place(`
      a2 a1
      a1 t4
      b1 t4
      t4!
    `);
    const next = place(
      `
      c1 a1
      a2 a1
      a1 t4
      b1 t4
      t4!
    `,
      first.state,
      true,
    );
    expect(ids(next.nodes)).toEqual(["c1", "a2", "a1", "b1", "t4"]);
  });

  it("follows a merge's first parent, then its second", () => {
    const { nodes } = place(`
      m a b
      b r
      a r
      r!
    `);
    expect(ids(nodes)).toEqual(["m", "a", "b", "r"]);
  });

  it("places both commits of a divergent change", () => {
    const { nodes, state } = place(`
      x1/x t
      x2/x t
      t!
    `);
    expect(ids(nodes)).toEqual(["x1", "x2", "t"]);
    // Each commit is a head of its own, with a rank of its own.
    expect(state.placed.get("x1")?.rank).toBeLessThan(state.placed.get("x2")?.rank ?? 0);
  });

  it("leaves no node waiting for a child after a cut", () => {
    // Cut at the limit: a1's parent t4 is not loaded.
    const { nodes } = place(`
      a2 a1
      b1 a1
      a1 t4
    `);
    expect(ids(nodes)).toEqual(["a2", "b1", "a1"]);
  });

  it("keeps a divergent change's commits in place as either is snapshotted", () => {
    let state = place(`
      x1/x t
      x2/x t
      b1 t
      t!
    `).state;
    // Each snapshot makes the rewritten commit the newest, so the server lists it first.
    const steps = [
      ["x2b/x t", "x1/x t"],
      ["x1b/x t", "x2b/x t"],
      ["x2c/x t", "x1b/x t"],
    ] as const;
    const seen: string[][] = [];
    for (const [newest, other] of steps) {
      const next = place(`${newest}\n${other}\nb1 t\nt!`, state, true);
      state = next.state;
      seen.push(ids(next.nodes));
    }
    expect(seen).toEqual([
      ["x1", "x2b", "b1", "t"],
      ["x1b", "x2b", "b1", "t"],
      ["x1b", "x2c", "b1", "t"],
    ]);
  });

  it("keeps a split stack's heads in place as they are snapshotted", () => {
    const first = place(`
      a3 a2
      a2 a1
      a1 t
      t!
    `);
    // a3 rebased onto a1: two heads sharing the stack's rank.
    let state = place(`a3x/a3 a1\na2 a1\na1 t\nt!`, first.state, true).state;
    for (const text of [`a2y/a2 a1\na3x/a3 a1\na1 t\nt!`, `a3z/a3 a1\na2y/a2 a1\na1 t\nt!`]) {
      const next = place(text, state, true);
      expect(
        ids(next.nodes)
          .slice(0, 2)
          .map((id) => id.slice(0, 2)),
      ).toEqual(["a3", "a2"]);
      state = next.state;
    }
  });

  it("keeps stacks in place after a merge of two of them is abandoned", () => {
    const first = place(`
      a1 t
      c1 t
      b1 t
      t!
    `);
    const merged = place(`m a1 b1\na1 t\nc1 t\nb1 t\nt!`, first.state, true);
    const abandoned = place(`a1 t\nc1 t\nb1 t\nt!`, merged.state, true);
    const order1 = ids(abandoned.nodes);
    // b1 snapshotted, then a1: the order holds.
    const snap1 = place(`b1x/b1 t\na1 t\nc1 t\nt!`, abandoned.state, true);
    expect(ids(snap1.nodes)).toEqual(order1.map((id) => (id === "b1" ? "b1x" : id)));
    const snap2 = place(`a1x/a1 t\nb1x/b1 t\nc1 t\nt!`, snap1.state, true);
    expect(ids(snap2.nodes)).toEqual(ids(snap1.nodes).map((id) => (id === "a1" ? "a1x" : id)));
  });

  it("keeps a stack's rank after it leaves the view and returns", () => {
    const first = place(`
      a1 t
      b1 t
      t!
    `);
    const cut = place(`a1 t\nt!`, first.state, true);
    const back = place(`b1 t\na1 t\nt!`, cut.state, true);
    expect(ids(back.nodes)).toEqual(["a1", "b1", "t"]);
  });

  it("gives a first load of Reef's own smartlog the CLI's order", () => {
    const nodes = logEntries(fixture as CliLogEntry[]).map(graphNode);
    const placed = order(nodes).nodes;
    topological(placed);
    expect(ids(placed)).toEqual(ids(nodes));
  });
});
