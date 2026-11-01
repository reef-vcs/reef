// An operation as the activity feed keeps it (R2.21): the fields a row, a
// count and the scrubber need, and never effects or attributes, so an
// operation from the stream and the same one from a list make the same
// record. The operation pane reads the rest with `GetOperation`.
import { timestampMs } from "@bufbuild/protobuf/wkt";
import type { OperationKind, SnapshotKind, Operation } from "../gen/reef/api/v1/operations_pb";

/** Characters of the description a record keeps. */
export const DESCRIPTION_KEPT = 200;

export interface OpRecord {
  id: string;
  parentIds: string[];
  /** Milliseconds since the epoch. */
  endedAt: number;
  kind: OperationKind;
  snapshotKind: SnapshotKind;
  workspaceId: string;
  jjWorkspace: string;
  agent: string;
  username: string;
  hostname: string;
  description: string;
}

export function recordOf(operation: Operation): OpRecord {
  return {
    id: operation.id,
    parentIds: [...operation.parentIds],
    endedAt: operation.endedAt ? timestampMs(operation.endedAt) : 0,
    kind: operation.kind,
    snapshotKind: operation.snapshotKind,
    workspaceId: operation.workspaceId,
    jjWorkspace: operation.jjWorkspace,
    agent: operation.agent,
    username: operation.username,
    hostname: operation.hostname,
    description: operation.description.slice(0, DESCRIPTION_KEPT),
  };
}

/**
 * jj's root operation: no parents. It ends the log and is never a row or a
 * count; its time is the epoch.
 */
export function isRoot(record: OpRecord): boolean {
  return record.parentIds.length === 0;
}

/** `ListOperations`' order: newest end first, ties by id descending. */
export function newerFirst(a: OpRecord, b: OpRecord): number {
  if (a.endedAt !== b.endedAt) return b.endedAt - a.endedAt;
  return a.id < b.id ? 1 : a.id > b.id ? -1 : 0;
}
