// `reef log -o json` as `LogEntry` messages, so fixtures are captured with
// the CLI: what the CLI prints is what the view is tested on.
import { create } from "@bufbuild/protobuf";
import { timestampFromDate } from "@bufbuild/protobuf/wkt";
import {
  ChangeSchema,
  GraphEdgeKind,
  LogEntrySchema,
  SignatureSchema,
  type LogEntry,
} from "../gen/reef/api/v1/changes_pb";

interface CliSignature {
  name: string;
  email: string;
  at?: string;
}

/** One entry of `reef log -o json`'s array. */
export interface CliLogEntry {
  change_id: string;
  commit_id: string;
  parent_commit_ids: string[];
  parent_change_ids: string[];
  description: string;
  author?: CliSignature;
  committer?: CliSignature;
  tree_id?: string;
  empty: boolean;
  conflict: boolean;
  divergent: boolean;
  immutable: boolean;
  hidden: boolean;
  bookmarks: string[];
  tags: string[];
  workspace_ids: string[];
  jj_workspaces: string[];
  edges: { commit_id: string; kind: string }[];
}

const KINDS: Record<string, GraphEdgeKind> = {
  direct: GraphEdgeKind.DIRECT,
  indirect: GraphEdgeKind.INDIRECT,
  missing: GraphEdgeKind.MISSING,
};

function signature(cli: CliSignature | undefined) {
  if (!cli) return undefined;
  return create(SignatureSchema, {
    name: cli.name,
    email: cli.email,
    ...(cli.at ? { at: timestampFromDate(new Date(cli.at)) } : {}),
  });
}

export function logEntries(cli: CliLogEntry[]): LogEntry[] {
  return cli.map((entry) =>
    create(LogEntrySchema, {
      change: create(ChangeSchema, {
        changeId: entry.change_id,
        commitId: entry.commit_id,
        parentCommitIds: entry.parent_commit_ids,
        parentChangeIds: entry.parent_change_ids,
        description: entry.description,
        author: signature(entry.author),
        committer: signature(entry.committer),
        treeId: entry.tree_id ?? "",
        empty: entry.empty,
        conflict: entry.conflict,
        divergent: entry.divergent,
        immutable: entry.immutable,
        hidden: entry.hidden,
        bookmarks: entry.bookmarks,
        tags: entry.tags,
        workspaceIds: entry.workspace_ids,
        jjWorkspaces: entry.jj_workspaces,
      }),
      edges: entry.edges.map((edge) => ({
        commitId: edge.commit_id,
        kind: KINDS[edge.kind] ?? GraphEdgeKind.UNSPECIFIED,
      })),
    }),
  );
}
