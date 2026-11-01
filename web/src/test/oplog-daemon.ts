// An in-memory operation log for the activity feed's tests (Connect's router
// transport): `ListOperations` with page tokens that pin the heads and skip
// what earlier pages took, `at_operation`, `GetOperation` by id, prefix or
// time, concurrent heads, and a garbage collection that renames every id.
// Pre-GC ids still answer `GetOperation`; a walk from one, or a token pinned
// before the GC, fails with `OPERATION_LOG_REWRITTEN`, as the daemon's do.
import { clone, create } from "@bufbuild/protobuf";
import { timestampFromMs, timestampMs } from "@bufbuild/protobuf/wkt";
import { Code, createRouterTransport } from "@connectrpc/connect";
import { connectionOver, type Connection } from "../api/connection";
import {
  GetOperationResponseSchema,
  ListOperationsResponseSchema,
  OperationKind,
  OperationSchema,
  OperationService,
  SnapshotKind,
  type GetOperationRequest,
  type ListOperationsRequest,
  type Operation,
} from "../gen/reef/api/v1/operations_pb";
import {
  GetRepositoryResponseSchema,
  RepositorySchema,
  RepositoryService,
} from "../gen/reef/api/v1/repositories_pb";
import { daemonError } from "./fakes";

export const REPO = "a3dda866c7e8f343";
export const T = Date.UTC(2026, 10, 1, 12, 0, 0);

const ms = (operation: Operation) => (operation.endedAt ? timestampMs(operation.endedAt) : 0);

function newerFirst(a: Operation, b: Operation): number {
  const d = ms(b) - ms(a);
  if (d !== 0) return d;
  return a.id < b.id ? 1 : a.id > b.id ? -1 : 0;
}

const rewritten = () =>
  daemonError(Code.FailedPrecondition, {
    reason: "OPERATION_LOG_REWRITTEN",
    hint: "garbage collection rewrote the log",
  });

const notFound = (id: string) =>
  daemonError(Code.NotFound, { reason: "OPERATION_NOT_FOUND", metadata: { operation: id } });

export interface OpInit {
  endedAt: number;
  id?: string;
  kind?: OperationKind;
  snapshotKind?: SnapshotKind;
  workspaceId?: string;
  jjWorkspace?: string;
  agent?: string;
  username?: string;
  hostname?: string;
  description?: string;
}

export class FakeOplog {
  /** Current operations by id. */
  readonly ops = new Map<string, Operation>();
  /** Operations from before the last GC, by their old ids. */
  readonly old = new Map<string, Operation>();
  heads: string[];
  readonly rootId: string;
  /** Bumped by each GC; tokens carry it. */
  private generation = 0;
  private next = 0;
  readonly listCalls: ListOperationsRequest[] = [];
  readonly getCalls: GetOperationRequest[] = [];
  /** Holds each list answer, read when the request arrived, until it resolves. */
  hold: Promise<void> | undefined;
  /** Each page holds at most this many operations (the daemon may answer short). */
  pageCap = 1000;
  failList: Error | undefined;

  constructor(readonly repo: string = REPO) {
    this.rootId = "0".repeat(16);
    this.ops.set(
      this.rootId,
      create(OperationSchema, { id: this.rootId, repoId: repo, endedAt: timestampFromMs(0) }),
    );
    this.heads = [this.rootId];
  }

  /**
   * Records an operation on the heads (a merge when there are several), or
   * on `parents`, which makes concurrent heads when they are not all heads.
   */
  add(init: OpInit, parents: string[] = [...this.heads]): Operation {
    this.next += 1;
    const id = init.id ?? `op${String(this.next).padStart(5, "0")}`.padEnd(16, "0");
    const operation = create(OperationSchema, {
      kind: OperationKind.SNAPSHOT,
      snapshotKind: SnapshotKind.WATCHER,
      ...init,
      id,
      repoId: this.repo,
      parentIds: parents,
      endedAt: timestampFromMs(init.endedAt),
    });
    this.ops.set(id, operation);
    this.heads = [...this.heads.filter((h) => !parents.includes(h)), id];
    return operation;
  }

  /** Renames every operation; the old ids keep answering `GetOperation`. */
  gc() {
    this.generation += 1;
    const suffix = `g${String(this.generation)}`;
    const rename = (id: string) => (id === this.rootId ? id : `${id.slice(0, 14)}${suffix}`);
    const renamed = new Map<string, Operation>();
    for (const [id, operation] of this.ops) {
      if (id !== this.rootId) this.old.set(id, operation);
      const fresh = clone(OperationSchema, operation);
      fresh.id = rename(id);
      fresh.parentIds = operation.parentIds.map(rename);
      renamed.set(fresh.id, fresh);
    }
    this.ops.clear();
    for (const [id, operation] of renamed) this.ops.set(id, operation);
    this.heads = this.heads.map(rename);
  }

  /** What `ListOperations` lists from `from` (the heads by default), all pages. */
  walk(from: string[] = this.heads): Operation[] {
    const out = new Map<string, Operation>();
    const stack = [...from];
    while (stack.length > 0) {
      const id = stack.pop() as string;
      if (out.has(id)) continue;
      const operation = this.ops.get(id);
      if (!operation) continue;
      out.set(id, operation);
      stack.push(...operation.parentIds);
    }
    return [...out.values()].sort(newerFirst);
  }

  private find(id: string): Operation | undefined {
    const exact = this.ops.get(id) ?? this.old.get(id);
    if (exact) return exact;
    const matches = [...this.ops.values()].filter((o) => o.id.startsWith(id));
    return matches.length === 1 ? matches[0] : undefined;
  }

  connection(): Connection {
    const transport = createRouterTransport(({ service }) => {
      service(RepositoryService, {
        getRepository: () =>
          create(GetRepositoryResponseSchema, {
            repository: create(RepositorySchema, { id: this.repo, root: "/src/reef" }),
          }),
      });
      service(OperationService, {
        listOperations: async (request) => {
          this.listCalls.push(request);
          if (this.failList) throw this.failList;
          let heads = this.heads;
          let skip = 0;
          let at = "";
          if (request.pageToken) {
            const token = JSON.parse(request.pageToken) as {
              g: number;
              heads: string[];
              skip: number;
              at: string;
            };
            if (token.g !== this.generation) throw rewritten();
            ({ heads, skip, at } = token);
          } else if (request.atOperation) {
            const operation = this.find(request.atOperation);
            if (!operation) throw notFound(request.atOperation);
            if (!this.ops.has(operation.id)) throw rewritten();
            at = operation.id;
            heads = [operation.id];
          }
          const size = Math.min(request.pageSize || 100, 1000, this.pageCap);
          const all = this.walk(heads);
          const page = all.slice(skip, skip + size);
          const taken = skip + page.length;
          // The page is read when the request arrives, and answered after the hold.
          const response = create(ListOperationsResponseSchema, {
            operations: page,
            nextPageToken:
              taken < all.length
                ? JSON.stringify({ g: this.generation, heads, skip: taken, at })
                : "",
            atOperation: at,
          });
          await this.hold;
          return response;
        },
        getOperation: (request) => {
          this.getCalls.push(request);
          if (request.endedAtOrBefore) {
            const limit = timestampMs(request.endedAtOrBefore);
            const found = this.walk().find((o) => ms(o) <= limit && o.id !== this.rootId);
            if (!found)
              throw daemonError(Code.OutOfRange, {
                reason: "OPERATION_NOT_RETAINED",
                hint: "older than the oldest kept operation",
              });
            return create(GetOperationResponseSchema, { operation: found });
          }
          const found = this.find(request.operationId);
          if (!found) throw notFound(request.operationId);
          return create(GetOperationResponseSchema, { operation: found });
        },
      });
    });
    return connectionOver(transport, "test");
  }
}
