// The proposal view's sentences (R3.31): what the automatic service is doing
// about a proposal, what the policy still needs, why a transition happened,
// and what holds a proposal back. Plain text: the page renders every string
// as text, never as HTML.
import {
  AutomaticState,
  type AutomaticStatus,
  type ProposalDependency,
} from "../gen/reef/api/v1/changes_pb";
import {
  DependencyEvent,
  LifecycleState,
  TransitionActor,
  TransitionCause,
  type ChangeTransition,
} from "../gen/reef/api/v1/lifecycle_pb";
import {
  PolicyDecision,
  PolicyRuleStatus,
  type PolicyEvidenceNeed,
} from "../gen/reef/api/v1/policy_pb";
import { reverseHex, shortId } from "../ui/ids";
import { plural } from "../ui/words";

/** A change id as people read it: jj's reverse hex, short. */
export function changeLabel(changeId: string): string {
  return shortId(reverseHex(changeId));
}

/** "test", "test of src/lib.rs", "a sign-off". */
export function needWords(need: Pick<PolicyEvidenceNeed, "kind" | "target">): string {
  const kind = need.kind === "signoff" ? "a sign-off" : need.kind;
  return need.target ? `${kind} of ${need.target}` : kind;
}

function needsList(needs: Pick<PolicyEvidenceNeed, "kind" | "target">[], total: number): string {
  const words = needs.map(needWords);
  const more = total > needs.length ? `, and ${total - needs.length} more` : "";
  if (words.length <= 1) return `${words.join("")}${more}`;
  return `${words.slice(0, -1).join(", ")} and ${words.at(-1) ?? ""}${more}`;
}

/** One sentence for the automatic service's status (R3.45); empty when it has none. */
export function automaticWords(status: AutomaticStatus | undefined): string {
  if (!status) return "";
  const missing = needsList(status.missing, status.missingTotal);
  switch (status.state) {
    case AutomaticState.OFF:
      return "Not landed automatically: Accept asks for the landing.";
    case AutomaticState.PENDING:
      return "Not judged yet.";
    case AutomaticState.WAITING:
      return missing ? `Waiting for ${missing} on this tree.` : "Waiting for its checks.";
    case AutomaticState.READY:
      return "Ready to land.";
    case AutomaticState.QUEUED:
      return "Landing.";
    case AutomaticState.NEEDS:
      if (status.message) return status.message;
      return missing ? `Needs ${missing}.` : "Blocked by its policy.";
    case AutomaticState.NO_EXECUTOR:
      return "Waiting for an executor: none has claimed its checks.";
    case AutomaticState.RETURNED:
      return (
        status.message || `Its landing came back${status.reason ? ` (${status.reason})` : ""}.`
      );
    default:
      return "";
  }
}

/** What the policy decided, as a sentence. */
export const DECISION_WORDS: Record<PolicyDecision, string> = {
  [PolicyDecision.UNSPECIFIED]: "The policy gave no decision.",
  [PolicyDecision.ALLOW]: "The policy allows it to land.",
  [PolicyDecision.BLOCK]: "The policy blocks it.",
  [PolicyDecision.NO_POLICY]: "Its destination has no policy: nothing is required.",
  [PolicyDecision.INVALID_POLICY]: "Its destination's policy is invalid: it cannot land.",
};

export const RULE_WORDS: Record<PolicyRuleStatus, string> = {
  [PolicyRuleStatus.UNSPECIFIED]: "unknown",
  [PolicyRuleStatus.SATISFIED]: "met",
  [PolicyRuleStatus.UNSATISFIED]: "not met",
  [PolicyRuleStatus.NOT_APPLICABLE]: "does not apply",
  [PolicyRuleStatus.ERROR]: "failed to evaluate",
};

const DEPENDENCY_EVENTS: Record<DependencyEvent, string> = {
  [DependencyEvent.UNSPECIFIED]: "moved",
  [DependencyEvent.AMENDED]: "was amended",
  [DependencyEvent.LANDED]: "landed",
  [DependencyEvent.SUPERSEDED]: "was superseded",
};

const TO_WORDS: Partial<Record<LifecycleState, string>> = {
  [LifecycleState.DRAFT]: "Returned to draft",
  [LifecycleState.PROPOSED]: "Proposed",
  [LifecycleState.QUEUED]: "Queued to land",
  [LifecycleState.LANDED]: "Landed",
  [LifecycleState.DECLINED]: "Declined",
  [LifecycleState.WITHDRAWN]: "Withdrawn",
  [LifecycleState.SUPERSEDED]: "Superseded",
};

/** Who performed a transition, in words. */
function byWhom(transition: ChangeTransition): string {
  if (transition.actor === TransitionActor.DAEMON) return "by Reef";
  const who = transition.identity || transition.agent;
  const via = transition.web ? " from the web UI" : transition.remote ? " remotely" : "";
  return who ? `by ${who}${via}` : via.trim();
}

/** One history row: "Declined by arsood from the web UI: needs a test." */
export function transitionWords(transition: ChangeTransition): string {
  if (transition.cause === TransitionCause.DEPENDENCY && transition.dependency) {
    const below = changeLabel(transition.dependency.changeId);
    const event = DEPENDENCY_EVENTS[transition.dependency.event];
    if (transition.dependency.event === DependencyEvent.SUPERSEDED) {
      return `Moved onto ${below}, which superseded the change below it.`;
    }
    return `Carried along: ${below}, below it, ${event}.`;
  }
  const to = TO_WORDS[transition.to] ?? "Moved";
  const repeat =
    transition.to === LifecycleState.PROPOSED && transition.from === LifecycleState.PROPOSED
      ? "Proposed again"
      : to;
  const cause =
    transition.cause === TransitionCause.UNDO
      ? " (an undo)"
      : transition.cause === TransitionCause.OBSERVED
        ? " (seen on its destination)"
        : "";
  const who = byWhom(transition);
  const reason = transition.reason ? `: ${transition.reason}` : "";
  return `${repeat}${who ? ` ${who}` : ""}${cause}${reason}.`;
}

/** Why a proposal waits on the changes below it (R3.11); empty when nothing holds it. */
export function heldWords(
  dependencies: ProposalDependency[],
  reasonOf: (changeId: string) => string = () => "",
): string {
  const left = dependencies.filter(
    (d) =>
      d.lifecycle?.state === LifecycleState.DECLINED ||
      d.lifecycle?.state === LifecycleState.WITHDRAWN,
  );
  const first = left[0];
  if (!first) return "";
  const word = first.lifecycle?.state === LifecycleState.DECLINED ? "declined" : "withdrawn";
  const others = left.length > 1 ? ` (and ${plural(left.length - 1, "other")})` : "";
  const reason = reasonOf(first.changeId);
  const why = reason ? ` (“${reason}”)` : "";
  return `Held: ${changeLabel(first.changeId)}${others}, below it, was ${word}${why}. It cannot land until the change below is proposed again or it moves off it.`;
}
