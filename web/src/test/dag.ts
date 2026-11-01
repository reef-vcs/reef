// Small graphs written as text, for the graph's tests. One node per line, in
// the server's order (children first): the commit id, then its parents.
//   a2 a1        a2 with a direct edge to a1
//   t4! :t2      t4 is immutable, with an indirect edge to t2
//   t2! ~t1      a parent outside the revset
//   a1/x t4      commit a1 of change x (a divergent change, or a rewrite)
import type { EdgeKind, GraphNode } from "../graph/node";

export function dag(text: string): GraphNode[] {
  return text
    .trim()
    .split("\n")
    .map((line) => line.trim())
    .filter((line) => line && !line.startsWith("#"))
    .map((line) => {
      const [head = "", ...parents] = line.split(/\s+/);
      const immutable = head.endsWith("!");
      const [id = "", changeId = id] = head.replace(/!$/, "").split("/");
      return {
        id,
        changeId,
        immutable,
        edges: parents.map((p) => {
          const kind: EdgeKind = p.startsWith(":")
            ? "indirect"
            : p.startsWith("~")
              ? "missing"
              : "direct";
          return { parent: p.replace(/^[:~]/, ""), kind };
        }),
      };
    });
}
