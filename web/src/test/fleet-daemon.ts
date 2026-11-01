// An in-memory daemon for the workspaces page's tests (Connect's router
// transport): the workspaces and working copies the test sets, the reads the
// page makes, and a record of every request.
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { durationFromMs, timestampFromMs } from "@bufbuild/protobuf/wkt";
import { createRouterTransport } from "@connectrpc/connect";
import { connectionOver, type Connection } from "../api/connection";
import {
  ChangeSchema,
  ChangeService,
  ListChangesResponseSchema,
  LogEntrySchema,
  type Change,
  type ListChangesRequest,
} from "../gen/reef/api/v1/changes_pb";
import {
  DaemonService,
  GetDaemonStatusResponseSchema,
  type GetDaemonStatusRequest,
} from "../gen/reef/api/v1/daemon_pb";
import {
  GetOperationResponseSchema,
  OperationService,
  type GetOperationRequest,
} from "../gen/reef/api/v1/operations_pb";
import {
  GetRepositoryResponseSchema,
  RepositorySchema,
  RepositoryService,
  type GetRepositoryRequest,
} from "../gen/reef/api/v1/repositories_pb";
import {
  DiskUsageSchema,
  GetWorkspaceResponseSchema,
  LeaseSchema,
  LeaseState,
  ListWorkspacesResponseSchema,
  WorkspaceSchema,
  WorkspaceService,
  WorkspaceState,
  type GetWorkspaceRequest,
  type ListWorkspacesRequest,
  type Workspace,
} from "../gen/reef/api/v1/workspaces_pb";
import { operation } from "./fakes";

export const REPO = "a3dda866c7e8f343";
export const T = Date.UTC(2026, 10, 1, 12, 0, 0);

/** A workspace on change `change` (hex), active, with a lease issued at `T` and never renewed. */
export function workspace(
  id: string,
  change: string,
  init: MessageInitShape<typeof WorkspaceSchema> = {},
): Workspace {
  return create(WorkspaceSchema, {
    id,
    repoId: REPO,
    owner: "ann@host",
    purpose: "fix the parser",
    changeId: change,
    commitId: `${change.slice(0, 8)}c0ffee`,
    state: WorkspaceState.ACTIVE,
    path: `/state/ws/${id}`,
    createdAt: timestampFromMs(T),
    lease: create(LeaseSchema, {
      id: `lease-${id}`,
      owner: "ann@host",
      issuedAt: timestampFromMs(T),
      lastHeartbeatAt: timestampFromMs(T),
      expiresAt: timestampFromMs(T + 3_600_000),
      state: LeaseState.ACTIVE,
      heartbeatEvery: durationFromMs(60_000),
      grace: durationFromMs(300_000),
    }),
    changedFileCount: 2,
    ...init,
  });
}

export function change(id: string, init: MessageInitShape<typeof ChangeSchema> = {}): Change {
  return create(ChangeSchema, {
    changeId: id,
    commitId: `${id.slice(0, 8)}c0ffee`,
    description: `${id.slice(0, 4)}: a change\n\nwith a body`,
    ...init,
  });
}

export class FakeFleetDaemon {
  workspaces: Workspace[] = [];
  changes: Change[] = [];
  /** What `GetWorkspace` answers for workspaces no longer listed. */
  finals = new Map<string, Workspace>();
  uptimeMs = 3_600_000;
  /** `GetWorkspace` answers without `disk_usage`. */
  noDisk = false;
  /** Each page holds at most this many workspaces. */
  pageCap = Number.POSITIVE_INFINITY;
  failList: Error | undefined;
  failRepository: Error | undefined;
  failGetWorkspace: Error | undefined;
  /** Holds each `ListWorkspaces` answer until it resolves. */
  hold: Promise<void> | undefined;
  /** Holds each `GetWorkspace` answer until it resolves. */
  holdGet: Promise<void> | undefined;
  readonly listCalls: ListWorkspacesRequest[] = [];
  readonly changeCalls: ListChangesRequest[] = [];
  readonly getCalls: GetWorkspaceRequest[] = [];
  readonly repositoryCalls: GetRepositoryRequest[] = [];
  readonly statusCalls: GetDaemonStatusRequest[] = [];
  readonly operationCalls: GetOperationRequest[] = [];

  connection(): Connection {
    const transport = createRouterTransport(({ service }) => {
      service(RepositoryService, {
        getRepository: (request) => {
          this.repositoryCalls.push(request);
          if (this.failRepository) throw this.failRepository;
          return create(GetRepositoryResponseSchema, {
            repository: create(RepositorySchema, { id: REPO, root: "/src/reef" }),
          });
        },
      });
      service(DaemonService, {
        getDaemonStatus: (request) => {
          this.statusCalls.push(request);
          return create(GetDaemonStatusResponseSchema, {
            version: "0.2.0",
            uptimeMs: BigInt(this.uptimeMs),
          });
        },
      });
      service(WorkspaceService, {
        listWorkspaces: async (request) => {
          this.listCalls.push(request);
          await this.hold;
          if (this.failList) throw this.failList;
          const skip = request.pageToken ? Number(request.pageToken) : 0;
          const size = Math.min(request.pageSize || 100, this.pageCap);
          const page = this.workspaces.slice(skip, skip + size);
          const next = skip + page.length;
          return create(ListWorkspacesResponseSchema, {
            workspaces: page,
            nextPageToken: next < this.workspaces.length ? String(next) : "",
            atOperation: request.atOperation,
          });
        },
        getWorkspace: async (request) => {
          this.getCalls.push(request);
          await this.holdGet;
          if (this.failGetWorkspace) throw this.failGetWorkspace;
          const found =
            this.workspaces.find((w) => w.id === request.workspace) ??
            this.finals.get(request.workspace);
          // The daemon's GetWorkspace never fills `changed_file_count`.
          const record =
            found && create(WorkspaceSchema, { ...found, changedFileCount: undefined });
          return create(GetWorkspaceResponseSchema, {
            workspace: record,
            ...(request.skipDiskUsage || this.noDisk || request.atOperation
              ? {}
              : {
                  diskUsage: create(DiskUsageSchema, {
                    allocatedBytes: 18n * 1024n * 1024n,
                    filesChanged: 3n,
                  }),
                }),
            snapshotCount: 4n,
            atOperation: request.atOperation,
          });
        },
      });
      service(ChangeService, {
        listChanges: (request) => {
          this.changeCalls.push(request);
          return create(ListChangesResponseSchema, {
            entries: this.changes.map((c) => create(LogEntrySchema, { change: c })),
            atOperation: request.atOperation || "op-head",
          });
        },
      });
      service(OperationService, {
        getOperation: (request) => {
          this.operationCalls.push(request);
          return create(GetOperationResponseSchema, {
            operation: operation(`${request.operationId}-full`, REPO, T, { agent: "ann" }),
          });
        },
      });
    });
    return connectionOver(transport, "test");
  }
}
