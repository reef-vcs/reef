// The proposal page's actions (R3.31): Accept, Decline and Comment, as one
// state machine, so the page never shows two of them working at once or a
// button in two states. Each write is one API call through the web
// listener's allowlist (ADR 0086); Accept is R3.43's one-touch sign-off,
// then a `LandChange` for the same commit.
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError } from "../api/errors";
import { DEVICE_READS } from "../device";
import { LifecycleState, TransitionActor } from "../gen/reef/api/v1/lifecycle_pb";
import {
  LandingOutcome,
  LandingService,
  type LandChangeResponse,
} from "../gen/reef/api/v1/landing_pb";
import { NO_SUPPORT, type Failure } from "../sign/passkey";
import { signOff, type SignOffStep, type SignOffTarget } from "../sign/signoff";
import { lifecycleState } from "../state/states";
import { reverseHex } from "../ui/ids";
import { automaticWords } from "./proposal-words";

export type ActionName = "accept" | "decline" | "comment";

export type Step = SignOffStep | "landing" | "writing";

export type ActionState =
  | { kind: "idle" }
  | { kind: "working"; action: ActionName; step: Step }
  | { kind: "done"; action: ActionName; words: string }
  | { kind: "failed"; action: ActionName; failure: Failure };

export type ActionEvent =
  | { type: "start"; action: ActionName; step: Step }
  | { type: "step"; step: Step }
  | { type: "done"; words: string }
  | { type: "failed"; failure: Failure }
  | { type: "reset" };

/** One action at a time: a start while another works is ignored. */
export function actionReducer(state: ActionState, event: ActionEvent): ActionState {
  switch (event.type) {
    case "start":
      return state.kind === "working"
        ? state
        : { kind: "working", action: event.action, step: event.step };
    case "step":
      return state.kind === "working" ? { ...state, step: event.step } : state;
    case "done":
      return state.kind === "working"
        ? { kind: "done", action: state.action, words: event.words }
        : state;
    case "failed":
      return state.kind === "working"
        ? { kind: "failed", action: state.action, failure: event.failure }
        : state;
    case "reset":
      return state.kind === "working" ? state : { kind: "idle" };
  }
}

export const STEP_WORDS: Record<Step, string> = {
  requesting: "Asking the daemon for an approval.",
  touching: "Touch your passkey to approve this sign-off.",
  adding: "Adding the sign-off.",
  landing: "Asking to land it.",
  writing: "Sending it.",
};

/** What Accept needs that the page knows. */
export interface AcceptFacts {
  state: LifecycleState;
  /** The proposal moved to another commit since the person opened it. */
  amended: boolean;
  /** The tree on the page; empty when the change has no visible commit. */
  tree: string;
  /** The change moved since it was proposed (`ChangeLifecycle.amended`): what is on the page was never proposed. */
  unproposed: boolean;
  /** The signer's name and live passkeys; undefined while unread. */
  signer: { name: string; passkeys: number } | undefined;
  supported: boolean;
  /** The page is a paired phone's (R3.40). */
  device: boolean;
}

export const UNPROPOSED =
  "The agent changed this change since it proposed it: what is shown was never proposed, and it lands only once the agent proposes it again.";

/** Why Accept is off, or null when it is on. */
export function acceptBlocked(facts: AcceptFacts): string | null {
  if (facts.state !== LifecycleState.PROPOSED) {
    const label = lifecycleState(facts.state)?.label ?? "not proposed";
    return `Only a proposed change can be accepted; this one is ${label}.`;
  }
  if (facts.device) return DEVICE_READS;
  if (facts.unproposed) return UNPROPOSED;
  if (!facts.tree) return "The change has no visible commit to sign off.";
  if (facts.amended) {
    return "The agent changed this proposal since you opened it. Review the new version to accept it.";
  }
  if (!facts.supported) return NO_SUPPORT;
  if (!facts.signer) return "Reading who signs.";
  if (facts.signer.passkeys === 0) return `${facts.signer.name} has no passkey.`;
  return null;
}

/** Why Decline is off, or null. A decline never needs a passkey: it is not evidence. */
export function declineBlocked(
  state: LifecycleState,
  amended: boolean,
  paired: boolean,
): string | null {
  if (state !== LifecycleState.PROPOSED && state !== LifecycleState.QUEUED) {
    return "Only a change in review can be declined.";
  }
  if (paired) return DEVICE_READS;
  if (amended) {
    return "The agent changed this proposal since you opened it. Review the new version to decline it.";
  }
  return null;
}

/** What a landing request answered, as a sentence after "Signed off." */
export function landingWords(response: LandChangeResponse): string {
  if (response.waiting) {
    const words = automaticWords(response.waiting);
    return `Signed off. It lands once its checks are in${words ? `: ${words}` : "."}`;
  }
  const landing = response.landing;
  switch (landing?.outcome) {
    case LandingOutcome.QUEUED:
      return "Signed off and queued to land.";
    case LandingOutcome.LANDED:
      return "Signed off and landed.";
    case LandingOutcome.BLOCKED:
      return `Signed off, but the gate blocked the landing${landing.message ? `: ${landing.message}` : "."}`;
    default:
      return "Signed off.";
  }
}

/** What Accept signs and lands: the tree shown, of the change's commit shown. */
export interface AcceptTarget extends SignOffTarget {
  changeId: string;
}

const EXPECTED_COMMIT_MISMATCH = "EXPECTED_COMMIT_MISMATCH";
const VERIFIED_SIGNOFF_REQUIRED = "VERIFIED_SIGNOFF_REQUIRED";

export const MOVED_AFTER_SIGNING =
  "Signed off, but the agent changed the proposal before it could land: review the new version to accept it.";

/**
 * Why the web UI could not land a tree it signed off: the destination's
 * policy pins no passkey, or not the one that touched, or not where it did
 * (ADR 0086 decision 2, #346).
 * A destination with no policy, or one that does not load, lands nothing
 * from a terminal either, so its policy is committed by hand. Null for any
 * other refusal.
 */
export function unpinnedWords(metadata: Record<string, string>, commit: string): string | null {
  const destination = metadata.destination ?? "the destination";
  const line = "the [passkeys] line that reef identity passkey list prints";
  const terminal = `or land it from a terminal: reef land ${commit.slice(0, 12)}`;
  switch (metadata.missing) {
    case "pin":
      switch (metadata.policy) {
        case "no_policy":
          return `Signed off, but ${destination} has no reef-policy.toml, so nothing lands on it, from here or from a terminal. Commit a first one to ${destination} by hand (reef policy init writes a starter) with ${line}, then Accept again.`;
        case "invalid_policy":
          return `Signed off, but ${destination}'s reef-policy.toml does not load, so nothing lands on it, from here or from a terminal. Commit a fix to ${destination} by hand (reef policy check says what is wrong) with ${line}, then Accept again.`;
        case "no_pins":
          return `Signed off, but ${destination} pins no passkey, so the web UI cannot land on it. To land from here, land a change that adds ${line} to ${destination}'s reef-policy.toml from a terminal, then Accept again; ${terminal}.`;
        default:
          return null;
      }
    case "pinned_touch":
      return `Signed off, but ${destination} does not pin the passkey you touched, so the web UI cannot land it. Accept again with a passkey ${destination}'s reef-policy.toml pins (reef identity passkey list shows the line that pins each); ${terminal}.`;
    case "pinned_origin":
      return `Signed off, but your touch ran at ${metadata.origin ?? "this address"}, and ${destination}'s reef-policy.toml pins that passkey at another, so the web UI cannot land it. If you moved Reef's port, pin this address (reef identity passkey list prints the line); if you did not, this may not be Reef's page: open it with reef ui from your terminal, and ${terminal}.`;
    default:
      return null;
  }
}

/**
 * Accept: signs off the tree the person reviewed with one passkey touch,
 * then asks to land that commit, which lands the stack below it too (R3.10).
 * Through the web UI the daemon lands only a tree signed off with a passkey
 * the destination's policy pins (ADR 0086). The change is named by its id with the commit shown, so
 * an amend after the read refuses the landing as a mismatch. With automatic
 * landing the request returns the landing the service opened.
 */
export async function accept(
  connection: Connection,
  target: AcceptTarget,
  onStep: (step: Step) => void,
): Promise<string> {
  await signOff(connection, target, onStep);
  onStep("landing");
  try {
    const response = await client(connection, LandingService).landChange({
      repo: target.repo,
      revision: reverseHex(target.changeId),
      expectedCommitId: target.commitId,
      actor: TransitionActor.HUMAN,
      skipSnapshot: true,
    });
    return landingWords(response);
  } catch (thrown) {
    // The sign-off stands; only the request to land failed.
    const error = reefError(thrown);
    if (isSessionInvalid(error)) throw thrown;
    if (error.reason === EXPECTED_COMMIT_MISMATCH) return MOVED_AFTER_SIGNING;
    if (error.reason === VERIFIED_SIGNOFF_REQUIRED) {
      const words = unpinnedWords(error.metadata, target.commitId);
      if (words) return words;
    }
    return `Signed off, but the request to land failed: ${error.message}${error.hint ? ` ${error.hint}` : ""}`;
  }
}
