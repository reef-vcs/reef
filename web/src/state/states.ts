// The state-color system (R2.18, ADR 0047): the design's seven object states,
// `ended` and `none`, and the mapping from what Phase 2's API reports.
import type { Change } from "../gen/reef/api/v1/changes_pb";
import { ChangeOutcome } from "../gen/reef/api/v1/history_pb";
import { LifecycleState } from "../gen/reef/api/v1/lifecycle_pb";

export const STATES = [
  "draft",
  "proposed",
  "queued",
  "testing",
  "landed",
  "failed",
  "conflicted",
  "ended",
  "none",
] as const;

export type State = (typeof STATES)[number];

export const MEANINGS: Record<State, string> = {
  draft: "Mutable work not proposed",
  proposed: "Put forward for main",
  queued: "In the landing queue",
  testing: "Checks running",
  landed: "On trunk()",
  failed: "Checks or a landing failed",
  conflicted: "Needs a resolution: a conflict, or a divergent change",
  ended: "Ended without landing: abandoned, undone, declined, withdrawn, superseded",
  none: "No state, or one the app does not know",
};

export interface Labelled {
  state: State;
  label: string;
}

/**
 * A change's state. `immutable` is `immutable_heads()` and its ancestors, and
 * Reef's `immutable_heads()` is `trunk() | tags()` (unlike jj's, it leaves out
 * untracked remote bookmarks, so a remote-only branch is a draft). The one
 * immutable commit not on trunk is an ancestor of a tag outside trunk's
 * ancestry, so R2.19 kept immutable as "landed".
 */
export function changeState(change: Change): Labelled {
  if (change.conflict) return { state: "conflicted", label: "conflicted" };
  if (change.divergent) return { state: "conflicted", label: "divergent" };
  if (change.hidden) return { state: "ended", label: "abandoned" };
  // A recorded lifecycle (R3.09) before what the flags say.
  const recorded = change.lifecycle?.recorded ? lifecycleState(change.lifecycle.state) : undefined;
  if (recorded) return recorded;
  if (change.immutable) return { state: "landed", label: "landed" };
  return { state: "draft", label: "draft" };
}

/** A recorded lifecycle state (R3.09); undefined for draft and unknown values. */
export function lifecycleState(state: LifecycleState): Labelled | undefined {
  switch (state) {
    case LifecycleState.PROPOSED:
      return { state: "proposed", label: "proposed" };
    case LifecycleState.QUEUED:
      return { state: "queued", label: "queued" };
    case LifecycleState.LANDED:
      return { state: "landed", label: "landed" };
    case LifecycleState.DECLINED:
      return { state: "ended", label: "declined" };
    case LifecycleState.WITHDRAWN:
      return { state: "ended", label: "withdrawn" };
    case LifecycleState.SUPERSEDED:
      return { state: "ended", label: "superseded" };
    default:
      return undefined;
  }
}

/** A change's state from R2.16's history outcome. */
export function outcomeState(outcome: ChangeOutcome): Labelled {
  switch (outcome) {
    case ChangeOutcome.OPEN:
      return { state: "draft", label: "draft" };
    case ChangeOutcome.LANDED:
      return { state: "landed", label: "landed" };
    case ChangeOutcome.ABANDONED:
      return { state: "ended", label: "abandoned" };
    case ChangeOutcome.UNDONE:
      return { state: "ended", label: "undone" };
    default:
      return { state: "none", label: "unknown" };
  }
}
