// An operation's kind as words, as the overview and the smartlog show it.
import { OperationKind, SnapshotKind, type Operation } from "../gen/reef/api/v1/operations_pb";

const KINDS: Partial<Record<OperationKind, string>> = {
  [OperationKind.SNAPSHOT]: "snapshot",
  [OperationKind.CREATE_WORKSPACE]: "create workspace",
  [OperationKind.RELEASE_WORKSPACE]: "release workspace",
  [OperationKind.MOVE_WORKSPACE]: "move workspace",
  [OperationKind.DESCRIBE]: "describe",
  [OperationKind.ABANDON]: "abandon",
  [OperationKind.UNDO]: "undo",
  [OperationKind.EDIT]: "edit",
  [OperationKind.IMPORT_REFS]: "import refs",
  [OperationKind.EXTERNAL]: "external",
  [OperationKind.OTHER]: "other",
};

const SNAPSHOTS: Partial<Record<SnapshotKind, string>> = {
  [SnapshotKind.WATCHER]: "watcher",
  [SnapshotKind.EXPLICIT]: "explicit",
  [SnapshotKind.FINAL]: "final",
  [SnapshotKind.MOVE]: "move",
  [SnapshotKind.SHUTDOWN]: "shutdown",
};

export function operationLabel(operation: Operation): string {
  const kind = KINDS[operation.kind] ?? "unknown";
  const snapshot = SNAPSHOTS[operation.snapshotKind];
  return operation.kind === OperationKind.SNAPSHOT && snapshot ? `${kind} (${snapshot})` : kind;
}
