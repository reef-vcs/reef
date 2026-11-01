// The activity feed's rows (R2.21): a pure function of the window's ordered
// records, so the stream and a list, a live session and a reload, give the
// same rows for the same operations.
//
// Each operation is one of three classes. Work: a snapshot or an edit
// recorded for a Reef workspace, keyed by the workspace and the agent.
// Outside jj: an `EXTERNAL` operation, keyed by the jj workspace and
// `username@hostname`. Events: every other kind, and any merge of concurrent
// operations. Walking oldest first, an event is a row of its own and closes
// the open runs in its place (its workspace, or its jj workspace). Work and
// outside operations join the open run with their key while its newest
// operation ended no more than `RUN_GAP_MS` before; operations of other keys
// in between do not break a run. Effects are never consulted.
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { isRoot, newerFirst, type OpRecord } from "./record";

/** The longest pause inside a run. */
export const RUN_GAP_MS = 5 * 60_000;

export type RowClass = "work" | "outside" | "event";

export interface Row {
  /** Its oldest operation's id: stable as a run grows at the top. */
  key: string;
  class: RowClass;
  /** Newest first. */
  operations: OpRecord[];
}

export function classify(record: OpRecord): RowClass {
  if (record.parentIds.length > 1) return "event";
  if (
    (record.kind === OperationKind.SNAPSHOT || record.kind === OperationKind.EDIT) &&
    record.workspaceId
  ) {
    return "work";
  }
  if (record.kind === OperationKind.EXTERNAL) return "outside";
  return "event";
}

/** Where an operation happened: its Reef workspace, else its jj workspace. */
export function placeOf(record: OpRecord): string {
  return record.workspaceId ? `ws\n${record.workspaceId}` : `jj\n${record.jjWorkspace}`;
}

export function userOf(record: OpRecord): string {
  if (!record.username && !record.hostname) return "";
  return `${record.username}@${record.hostname}`;
}

function runKey(record: OpRecord, cls: RowClass): string {
  return cls === "work"
    ? `w\n${record.workspaceId}\n${record.agent}`
    : `o\n${record.jjWorkspace}\n${userOf(record)}`;
}

/** Rows from records in `newerFirst` order, newest row first. */
export function coalesce(records: readonly OpRecord[]): Row[] {
  const rows: Row[] = [];
  /** Open runs by key; each row's operations are oldest first until the end. */
  const open = new Map<string, { row: Row; place: string; newestAt: number }>();
  for (let i = records.length - 1; i >= 0; i--) {
    const record = records[i];
    if (!record || isRoot(record)) continue;
    const cls = classify(record);
    const place = placeOf(record);
    if (cls === "event") {
      for (const [key, run] of open) if (run.place === place) open.delete(key);
      rows.push({ key: record.id, class: cls, operations: [record] });
      continue;
    }
    const key = runKey(record, cls);
    const run = open.get(key);
    if (run && record.endedAt - run.newestAt <= RUN_GAP_MS) {
      run.row.operations.push(record);
      run.newestAt = record.endedAt;
      continue;
    }
    const row: Row = { key: record.id, class: cls, operations: [record] };
    rows.push(row);
    open.set(key, { row, place, newestAt: record.endedAt });
  }
  for (const row of rows) row.operations.reverse();
  return rows.sort((a, b) => newerFirst(a.operations[0] as OpRecord, b.operations[0] as OpRecord));
}
