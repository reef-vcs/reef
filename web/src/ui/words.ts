// The human vocabulary (the design pass), used outside developer mode. The
// API's words (`operationLabel`, lease health words) stay what developer mode
// shows, so a page can be checked against `reef` output.
import { OperationKind, SnapshotKind, type Operation } from "../gen/reef/api/v1/operations_pb";
import type { HealthWord } from "../fleet/lease";

const SENTENCES: Partial<Record<OperationKind, string>> = {
  [OperationKind.CREATE_WORKSPACE]: "created a workspace",
  [OperationKind.RELEASE_WORKSPACE]: "released a workspace",
  [OperationKind.MOVE_WORKSPACE]: "moved a workspace",
  [OperationKind.DESCRIBE]: "described a change",
  [OperationKind.ABANDON]: "abandoned a change",
  [OperationKind.UNDO]: "undid an operation",
  [OperationKind.EDIT]: "edited files",
  [OperationKind.IMPORT_REFS]: "imported git refs",
  [OperationKind.EXTERNAL]: "ran a jj command",
  [OperationKind.OTHER]: "did something else",
};

const SNAPSHOTS: Partial<Record<SnapshotKind, string>> = {
  [SnapshotKind.WATCHER]: "saved a snapshot",
  [SnapshotKind.EXPLICIT]: "took a snapshot",
  [SnapshotKind.FINAL]: "took a final snapshot",
  [SnapshotKind.MOVE]: "snapshotted before a move",
  [SnapshotKind.SHUTDOWN]: "snapshotted at shutdown",
};

/** "described a change": past tense, lower case. */
export function operationSentence(operation: Pick<Operation, "kind" | "snapshotKind">): string {
  if (operation.kind === OperationKind.SNAPSHOT) {
    return SNAPSHOTS[operation.snapshotKind] ?? "saved a snapshot";
  }
  return SENTENCES[operation.kind] ?? "did something else";
}

export function capitalize(text: string): string {
  return text.charAt(0).toUpperCase() + text.slice(1);
}

/** Lease health in people's words; developer mode shows the API's. */
export const HEALTH_WORDS: Record<HealthWord | "-", string> = {
  heartbeating: "healthy",
  "not heartbeating": "idle",
  "waiting for a heartbeat": "waiting",
  "missed heartbeats": "unresponsive",
  "-": "-",
};

/** "1 active workspace", "2 active workspaces". */
export function plural(count: number, one: string, many = `${one}s`): string {
  return `${count.toLocaleString("en-US")} ${count === 1 ? one : many}`;
}
