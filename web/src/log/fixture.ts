// Records for tests: a builder, and 600 consecutive operations of Reef's own
// log (2026-10-26/27) with agents, users and hosts replaced by placeholders
// and ids cut to 16 characters.
import { OperationKind, SnapshotKind } from "../gen/reef/api/v1/operations_pb";
import fixture from "./fixtures/reef-oplog.json";
import type { OpRecord } from "./record";

const KINDS: Record<string, OperationKind> = {
  unspecified: OperationKind.UNSPECIFIED,
  snapshot: OperationKind.SNAPSHOT,
  "create-workspace": OperationKind.CREATE_WORKSPACE,
  "release-workspace": OperationKind.RELEASE_WORKSPACE,
  "move-workspace": OperationKind.MOVE_WORKSPACE,
  describe: OperationKind.DESCRIBE,
  abandon: OperationKind.ABANDON,
  undo: OperationKind.UNDO,
  edit: OperationKind.EDIT,
  "import-refs": OperationKind.IMPORT_REFS,
  external: OperationKind.EXTERNAL,
  other: OperationKind.OTHER,
};

const SNAPSHOTS: Record<string, SnapshotKind> = {
  "": SnapshotKind.UNSPECIFIED,
  watcher: SnapshotKind.WATCHER,
  explicit: SnapshotKind.EXPLICIT,
  final: SnapshotKind.FINAL,
  move: SnapshotKind.MOVE,
  shutdown: SnapshotKind.SHUTDOWN,
};

/** Reef's log as records, newest first as `ListOperations` lists it. */
export function reefLog(): OpRecord[] {
  return fixture.map((entry) => ({
    ...entry,
    kind: KINDS[entry.kind] ?? OperationKind.UNSPECIFIED,
    snapshotKind: SNAPSHOTS[entry.snapshotKind] ?? SnapshotKind.UNSPECIFIED,
  }));
}

/** A record with one parent, `p-<id>` unless given. */
export function rec(id: string, endedAt: number, init: Partial<OpRecord> = {}): OpRecord {
  return {
    id,
    parentIds: [`p-${id}`],
    endedAt,
    kind: OperationKind.SNAPSHOT,
    snapshotKind: SnapshotKind.WATCHER,
    workspaceId: "",
    jjWorkspace: "",
    agent: "",
    username: "",
    hostname: "",
    description: "",
    ...init,
  };
}
