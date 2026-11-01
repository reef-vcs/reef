// The graph's input, independent of the API's messages so the ordering and
// the layout are pure and testable without protobuf: one node per commit,
// with its edges as `ListChanges` gives them.
import { GraphEdgeKind, type LogEntry } from "../gen/reef/api/v1/changes_pb";

export type EdgeKind = "direct" | "indirect" | "missing";

export interface GraphEdge {
  /** The parent commit id. */
  parent: string;
  kind: EdgeKind;
}

export interface GraphNode {
  /** The commit id: one row per commit. */
  id: string;
  /** The change id: a rewrite keeps it, so a stack's place follows it. */
  changeId: string;
  edges: GraphEdge[];
  immutable: boolean;
}

const KINDS: Record<GraphEdgeKind, EdgeKind | undefined> = {
  [GraphEdgeKind.UNSPECIFIED]: undefined,
  [GraphEdgeKind.DIRECT]: "direct",
  [GraphEdgeKind.INDIRECT]: "indirect",
  [GraphEdgeKind.MISSING]: "missing",
};

/** A `ListChanges` entry as a node. An edge of a kind this build does not know counts as missing. */
export function graphNode(entry: LogEntry): GraphNode {
  const change = entry.change;
  return {
    id: change?.commitId ?? "",
    changeId: change?.changeId ?? "",
    immutable: change?.immutable ?? false,
    edges: entry.edges.map((edge) => ({
      parent: edge.commitId,
      kind: KINDS[edge.kind] ?? "missing",
    })),
  };
}
