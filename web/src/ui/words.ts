// The human vocabulary (the design pass), used outside developer mode. The
// API's words (`operationLabel`, lease health words) stay what developer mode
// shows, so a page can be checked against `reef` output.
import { LifecycleState } from "../gen/reef/api/v1/lifecycle_pb";
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
  [OperationKind.TRANSITION]: "moved a change through its lifecycle",
  [OperationKind.LAND]: "landed a change",
  [OperationKind.BATCH]: "tested a batch of changes",
  [OperationKind.COMMENT]: "commented on a change",
  [OperationKind.PUBLISH]: "published a change",
  [OperationKind.REBASE]: "rebased a change",
  [OperationKind.FETCH]: "fetched from a git remote",
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

const TRANSITIONS: Partial<Record<LifecycleState, string>> = {
  [LifecycleState.DRAFT]: "returned a change to draft",
  [LifecycleState.PROPOSED]: "proposed a change",
  [LifecycleState.QUEUED]: "queued a proposal",
  [LifecycleState.LANDED]: "recorded a landing",
  [LifecycleState.DECLINED]: "declined a proposal",
  [LifecycleState.WITHDRAWN]: "withdrew a proposal",
  [LifecycleState.SUPERSEDED]: "superseded a change",
};

/** A comment operation's first event (R3.12), by the event's own name. */
const COMMENT_EVENTS: Record<string, string> = {
  comment: "commented on a change",
  instruct: "wrote an instruction",
  confirm: "confirmed an instruction",
  claim: "took an instruction",
  start: "started an instruction",
  finish: "finished an instruction",
  cancel: "cancelled an instruction",
  release: "handed an instruction back",
};

/** What an operation's first lifecycle transition moved a change to, if any (R3.09). */
type WithTransition = Pick<Operation, "kind" | "snapshotKind"> & {
  transitionTo?: LifecycleState;
  transitions?: Operation["transitions"];
  comments?: Pick<Operation["comments"][number], "event">[];
  effects?: { bookmarks: { name: string; remote: string }[] } | undefined;
  description?: string;
};

/** A landing push's description (R3.17), before the destination. */
const PUSH_LANDED_PREFIX = "reef: push landed ";

/** "described a change": past tense, lower case. */
export function operationSentence(operation: WithTransition): string {
  if (operation.kind === OperationKind.SNAPSHOT) {
    return SNAPSHOTS[operation.snapshotKind] ?? "saved a snapshot";
  }
  if (operation.kind === OperationKind.TRANSITION) {
    const first = operation.transitions?.[0];
    const to = operation.transitionTo ?? first?.to;
    // The landing service's dequeue (R3.13): back to proposed, from queued.
    if (to === LifecycleState.PROPOSED && first?.from === LifecycleState.QUEUED) {
      return "returned a proposal from the queue";
    }
    if (to !== undefined && TRANSITIONS[to]) return TRANSITIONS[to];
  }
  // A batch lands several changes in one operation (R3.14).
  if (operation.kind === OperationKind.LAND && (operation.transitions?.length ?? 0) > 1) {
    return `landed ${plural(operation.transitions?.length ?? 0, "change")}`;
  }
  // A publish (#258) names where it went: its remote bookmark's effect. A
  // landing push (R3.17) is a publish of the destination's landed history.
  if (operation.kind === OperationKind.PUBLISH) {
    const remote = operation.effects?.bookmarks.find((bookmark) => bookmark.remote !== "");
    const landed = operation.description?.startsWith(PUSH_LANDED_PREFIX) ?? false;
    if (landed) {
      return remote
        ? `pushed landed history to ${remote.remote}/${remote.name}`
        : "pushed landed history";
    }
    if (remote) return `published a change to ${remote.remote}/${remote.name}`;
  }
  if (operation.kind === OperationKind.COMMENT) {
    const event = operation.comments?.[0]?.event;
    if (event !== undefined && COMMENT_EVENTS[event]) return COMMENT_EVENTS[event];
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
