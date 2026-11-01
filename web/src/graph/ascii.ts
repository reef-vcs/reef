// A layout as text, for tests: the golden graphs are written in this format,
// inline in the test files, so a review shows the graph.
//
// Each column is two characters: the lane, then a gap. Each row is one line:
//   o      the node
//   | :    a direct or an indirect lane passing it
// followed by two spaces and the row's label. Around a row, a line is added
// only when there is something to show:
//   above, when other lanes join the node: `/` or `\` in the gap next to the
//     node, `-` across any farther gaps;
//   below, when an edge leaves the node's column, an edge is indirect, or a
//     parent is outside the revset: `|` or `:` for an edge in the node's own
//     column, `\` or `/` in the gap next to the node for an edge to another
//     column (`-` across farther gaps), and `~` for the stub.
// After the last row, lanes still open are shown once more, then cut: `.`.
import type { GraphRow, Layout, LaneKind } from "./layout";

const lane = (kind: LaneKind) => (kind === "direct" ? "|" : ":");

function blank(width: number): string[] {
  return Array.from({ length: width * 2 }, () => " ");
}

function put(line: string[], at: number, char: string, overwrite = true) {
  if (overwrite || line[at] === " ") line[at] = char;
}

/**
 * A line between the node's column and another: `char` in the gap next to the
 * node, `-` across the farther gaps. Lanes in between are kept.
 */
function connect(line: string[], node: number, other: number, char: string) {
  const step = other > node ? 1 : -1;
  for (let i = 2 * node + step; i !== 2 * other; i += step) {
    put(line, i, i === 2 * node + step ? char : "-", i % 2 === 1);
  }
}

function text(line: string[], label = ""): string {
  return (line.join("") + (label ? `  ${label}` : "")).trimEnd();
}

function pad(line: string[], width: number): string[] {
  while (line.length < width * 2) line.push(" ");
  return line;
}

export function ascii(graph: Layout, label: (row: GraphRow) => string = (r) => r.node.id): string {
  const width = graph.width;
  const lines: string[] = [];
  for (const row of graph.rows) {
    if (row.joins.length) {
      const above = blank(width);
      for (const p of row.passing) put(above, 2 * p.column, lane(p.kind));
      if (row.incoming) put(above, 2 * row.column, lane(row.incoming));
      for (const join of row.joins) {
        connect(above, row.column, join.from, join.from > row.column ? "/" : "\\");
      }
      lines.push(text(above));
    }
    const line = pad(blank(width), width);
    for (const p of row.passing) put(line, 2 * p.column, lane(p.kind));
    put(line, 2 * row.column, "o");
    lines.push(text(line, label(row)));

    const own = row.edges.find((e) => e.to === row.column);
    const away = row.edges.filter((e) => e.to !== row.column);
    if (row.stub || away.length || own?.kind === "indirect") {
      const below = blank(width);
      for (const p of row.passing) put(below, 2 * p.column, lane(p.kind));
      if (own) put(below, 2 * row.column, lane(own.kind));
      for (const edge of away) {
        connect(below, row.column, edge.to, edge.to > row.column ? "\\" : "/");
      }
      if (row.stub) put(below, own ? 2 * row.column + 1 : 2 * row.column, "~");
      lines.push(text(below));
    }
  }
  if (graph.open.length) {
    const cut = blank(width);
    for (const p of graph.open) put(cut, 2 * p.column, ".");
    lines.push(text(cut));
  }
  return lines.join("\n");
}
