// An in-memory daemon for the smartlog's tests (Connect's router transport):
// a log the test sets as text (test/dag.ts), the reads the view makes, and a
// record of every request.
import { create } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { createRouterTransport } from "@connectrpc/connect";
import { connectionOver, type Connection } from "../api/connection";
import {
  ChangeSchema,
  ChangeService,
  GraphEdgeKind,
  ListChangesResponseSchema,
  LogEntrySchema,
  type Change,
  type ListChangesRequest,
  type LogEntry,
} from "../gen/reef/api/v1/changes_pb";
import {
  DiffService,
  FileDiffSchema,
  GetDiffResponseSchema,
  type GetDiffRequest,
} from "../gen/reef/api/v1/diff_pb";
import { FileChangeKind, FileChangeSchema } from "../gen/reef/api/v1/files_pb";
import {
  ChangeActivitySchema,
  ChangeOutcome,
  HistoryChangeSchema,
  HistoryService,
  QueryChangesResponseSchema,
  type QueryChangesRequest,
} from "../gen/reef/api/v1/history_pb";
import {
  GetOperationResponseSchema,
  OperationService,
  type GetOperationRequest,
} from "../gen/reef/api/v1/operations_pb";
import { RepositorySchema } from "../gen/reef/api/v1/repositories_pb";
import {
  GetStatusResponseSchema,
  RepositoryStatusSchema,
  StatusService,
  type GetStatusRequest,
} from "../gen/reef/api/v1/status_pb";
import { WorkspaceSchema } from "../gen/reef/api/v1/workspaces_pb";
import { dag } from "./dag";
import { operation } from "./fakes";

export const REPO = "a3dda866c7e8f343";

const KINDS = {
  direct: GraphEdgeKind.DIRECT,
  indirect: GraphEdgeKind.INDIRECT,
  missing: GraphEdgeKind.MISSING,
};

/** Hex ids for test names: `a2` is commit `a2` of change `a2`, padded with zeros. */
export function hexId(name: string): string {
  let hex = "";
  for (let i = 0; i < name.length; i++) hex += name.charCodeAt(i).toString(16);
  return hex.padEnd(32, "0").slice(0, 32);
}

/** What a test may set on a change beyond its ids and edges. */
export type ChangeFields = Partial<
  Pick<
    Change,
    | "description"
    | "bookmarks"
    | "tags"
    | "workspaceIds"
    | "jjWorkspaces"
    | "empty"
    | "conflict"
    | "divergent"
    | "hidden"
  >
>;

/**
 * Log entries from test/dag.ts's text. Change ids are hex (`hexId`), commit
 * ids are the names; `fields` adds to a change by commit name.
 */
export function entries(text: string, fields: Record<string, ChangeFields> = {}): LogEntry[] {
  return dag(text).map((node) =>
    create(LogEntrySchema, {
      change: create(ChangeSchema, {
        changeId: hexId(node.changeId),
        commitId: node.id,
        immutable: node.immutable,
        description: `${node.id}: a change\n\nwith a body`,
        committer: {
          name: "Ann",
          email: "ann@example.com",
          at: timestampFromMs(1_761_919_380_000),
        },
        ...fields[node.id],
      }),
      edges: node.edges.map((edge) => ({ commitId: edge.parent, kind: KINDS[edge.kind] })),
    }),
  );
}

export class FakeRepoDaemon {
  log: LogEntry[] = [];
  /** The operation every read is at, unless asked for another. */
  head = "op-1";
  /** Each page holds at most this many entries, as if cut by the 3 MiB limit. */
  pageCap = Number.POSITIVE_INFINITY;
  readonly listCalls: ListChangesRequest[] = [];
  readonly statusCalls: GetStatusRequest[] = [];
  readonly diffCalls: GetDiffRequest[] = [];
  readonly historyCalls: QueryChangesRequest[] = [];
  readonly operationCalls: GetOperationRequest[] = [];
  /** Holds each `ListChanges` answer until it resolves. */
  hold: Promise<void> | undefined;
  failList: Error | undefined;
  /** Thrown by the next `GetStatus` only. */
  failStatusOnce: Error | undefined;
  failHistory: Error | undefined;
  /** `GetOperation` answers with no operation. */
  noOperation = false;
  readonly workspaces = [create(WorkspaceSchema, { id: "ws00000000000001", owner: "agent-a" })];

  connection(): Connection {
    const transport = createRouterTransport(({ service }) => {
      service(ChangeService, {
        listChanges: async (request) => {
          this.listCalls.push(request);
          await this.hold;
          if (this.failList) throw this.failList;
          const skip = request.pageToken ? Number(request.pageToken) : 0;
          const size = Math.min(request.pageSize || 100, this.pageCap);
          const page = this.log.slice(skip, skip + size);
          const next = skip + page.length;
          return create(ListChangesResponseSchema, {
            entries: page,
            nextPageToken: next < this.log.length ? String(next) : "",
            atOperation: request.atOperation || this.head,
          });
        },
      });
      service(StatusService, {
        getStatus: (request) => {
          this.statusCalls.push(request);
          const fail = this.failStatusOnce;
          this.failStatusOnce = undefined;
          if (fail) throw fail;
          return create(GetStatusResponseSchema, {
            repository: create(RepositorySchema, { id: REPO, root: "/src/reef" }),
            atOperation: request.atOperation,
            repositoryStatus: create(RepositoryStatusSchema, {
              trunkBookmark: "main",
              activeWorkspaces: this.workspaces,
              activeWorkspaceCount: this.workspaces.length,
            }),
          });
        },
      });
      service(DiffService, {
        getDiff: (request) => {
          this.diffCalls.push(request);
          return create(GetDiffResponseSchema, {
            files: [
              create(FileDiffSchema, {
                change: create(FileChangeSchema, {
                  path: "src/a.rs",
                  kind: FileChangeKind.MODIFIED,
                }),
                linesAdded: 3,
                linesRemoved: 1,
              }),
            ],
            totalFiles: 120,
            atOperation: request.atOperation || this.head,
          });
        },
      });
      service(HistoryService, {
        queryChanges: (request) => {
          this.historyCalls.push(request);
          if (this.failHistory) throw this.failHistory;
          return create(QueryChangesResponseSchema, {
            changes: [
              create(HistoryChangeSchema, {
                changeId: request.changes[0] ?? "",
                outcome: ChangeOutcome.OPEN,
                activity: [
                  create(ChangeActivitySchema, {
                    agent: "agent-a",
                    workspaceId: "ws00000000000001",
                    operations: 4,
                  }),
                ],
                activityCount: 1,
              }),
            ],
          });
        },
      });
      service(OperationService, {
        getOperation: (request) => {
          this.operationCalls.push(request);
          if (this.noOperation) return create(GetOperationResponseSchema, {});
          return create(GetOperationResponseSchema, {
            operation: operation(`${request.operationId}-full`, REPO, 1_761_919_380_000, {
              agent: "agent-a",
            }),
          });
        },
      });
    });
    return connectionOver(transport, "test");
  }
}
