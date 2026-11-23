// The selected change's sign-offs and Sign off (R3.43 decision 3): kind
// `signoff`, result `pass`, on the change's tree as the panel shows it, as
// the default identity, approved by one passkey touch in this tab.
import { useEffect, useState } from "react";
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import type { Change } from "../gen/reef/api/v1/changes_pb";
import { EvidenceResult, EvidenceService, type Evidence } from "../gen/reef/api/v1/evidence_pb";
import { PolicyService, type PolicyVerifiedSigner } from "../gen/reef/api/v1/policy_pb";
import {
  IdentityService,
  IdentityStatus,
  SessionPresence,
  type Identity,
} from "../gen/reef/api/v1/identity_pb";
import { failure, hasPasskeySupport, type Failure } from "../sign/passkey";
import { signOff, type SignOffStep } from "../sign/signoff";
import { ErrorView } from "../ui/ErrorView";
import { FailureView } from "../ui/FailureView";
import { reverseHex, shortId } from "../ui/ids";
import { Link } from "../ui/router";
import { Time } from "../ui/TimeLabel";
import { NoPasskeysHere } from "./Handoff";

export type Signer =
  | { kind: "loading" }
  | { kind: "failed"; error: ReefError }
  | { kind: "loaded"; identity: Identity; passkeys: number };

type Action =
  | { kind: "idle" }
  | { kind: "busy"; step: SignOffStep }
  | { kind: "done"; evidence: Evidence }
  | { kind: "failed"; failure: Failure };

const STEP_WORDS: Record<SignOffStep, string> = {
  requesting: "Asking the daemon for an approval.",
  touching: "Touch your passkey to approve this sign-off.",
  adding: "Adding the sign-off.",
};

const RESULT_WORDS: Partial<Record<EvidenceResult, string>> = {
  [EvidenceResult.PASS]: "pass",
  [EvidenceResult.FAIL]: "fail",
  [EvidenceResult.ERROR]: "error",
};

const STATUS_WORDS: Partial<Record<IdentityStatus, string>> = {
  [IdentityStatus.EXPIRED]: "key expired",
  [IdentityStatus.REVOKED]: "key revoked",
  [IdentityStatus.INVALID]: "certificate invalid",
};

/** The default identity, who Sign off signs as, and how many live passkeys it has. */
export function useSigner(connection: Connection, onSessionInvalid: () => void): Signer {
  const [signer, setSigner] = useState<Signer>({ kind: "loading" });
  useEffect(() => {
    let current = true;
    const identities = client(connection, IdentityService);
    identities
      .getIdentity({ name: "" })
      .then(async ({ identity }) => {
        if (!identity) throw new Error("The daemon returned no identity.");
        const { passkeys } = await identities.listPasskeys({ identity: identity.id });
        if (current) setSigner({ kind: "loaded", identity, passkeys: passkeys.length });
      })
      .catch((thrown: unknown) => {
        if (!current) return;
        const error = reefError(thrown);
        if (isSessionInvalid(error)) onSessionInvalid();
        else setSigner({ kind: "failed", error });
      });
    return () => {
      current = false;
    };
  }, [connection, onSessionInvalid]);
  return signer;
}

interface SignOffs {
  evidence: Evidence[];
  /** Whether the trunk's policy pins each `user_verified` sign-off's passkey, by evidence id. */
  pins: ReadonlyMap<string, PolicyVerifiedSigner>;
}

/** The change's sign-offs, read again after each of this panel's. */
function useSignOffs(
  connection: Connection,
  repo: string,
  change: Change,
  version: number,
  onSessionInvalid: () => void,
) {
  const [state, setState] = useState<SignOffs | { error: ReefError } | null>(null);
  const { changeId, commitId } = change;
  useEffect(() => {
    let current = true;
    // Pinning is the policy's word on each passkey; without a policy, or
    // when it cannot be read, the rows say nothing about it.
    const pins = client(connection, PolicyService)
      .evaluatePolicy({ repo, revision: commitId, skipSnapshot: true })
      .then(
        ({ evaluation }) =>
          new Map((evaluation?.verified ?? []).map((signer) => [signer.evidenceId, signer])),
        () => new Map<string, PolicyVerifiedSigner>(),
      );
    client(connection, EvidenceService)
      .listEvidence({ repo, changeId, kinds: ["signoff"] })
      .then(
        async ({ evidence }) => {
          const pinned = await pins;
          if (current) setState({ evidence, pins: pinned });
        },
        (thrown: unknown) => {
          if (!current) return;
          const error = reefError(thrown);
          if (isSessionInvalid(error)) onSessionInvalid();
          else setState({ error });
        },
      );
    return () => {
      current = false;
    };
  }, [connection, repo, changeId, commitId, version, onSessionInvalid]);
  return state;
}

export function presenceWords(evidence: Evidence): string {
  const key = evidence.identity?.key;
  if (key?.presence !== SessionPresence.USER_VERIFIED) return "not verified";
  const passkey = key.passkey;
  const name = passkey?.label ? `“${passkey.label}”` : shortId(passkey?.fingerprint ?? "");
  const at = key.passkeyOrigin ? ` at ${key.passkeyOrigin}` : "";
  return `user verified, passkey ${name}${at}`;
}

/** Whether the repository's policy pins a verified sign-off's touch, or why not. */
export function pinWords(signer: PolicyVerifiedSigner): string {
  if (signer.pinned) return "pinned by this repository's policy";
  return signer.unpinned === "origin"
    ? "touched at an address this repository's policy does not pin"
    : "not pinned by this repository's policy";
}

function SignOffRow({
  evidence,
  signer,
}: {
  evidence: Evidence;
  signer: PolicyVerifiedSigner | undefined;
}) {
  const status = evidence.identity?.status;
  const who = evidence.identity?.key?.identity || `key ${shortId(evidence.keyId)}`;
  return (
    <li>
      {who} · {RESULT_WORDS[evidence.result] ?? "unknown result"} · {presenceWords(evidence)}
      {status !== undefined && STATUS_WORDS[status] && ` · ${STATUS_WORDS[status]}`}
      {!evidence.current && " · on an earlier tree"}{" "}
      <span className="secondary">
        · <Time at={evidence.addedAt} />
      </span>
      {signer && (
        <span className="pinning">
          {" · "}
          {pinWords(signer)}
        </span>
      )}
    </li>
  );
}

export function SignOff({
  connection,
  repo,
  change,
  onSessionInvalid,
}: {
  connection: Connection;
  repo: string;
  change: Change;
  onSessionInvalid: () => void;
}) {
  const signer = useSigner(connection, onSessionInvalid);
  const [version, setVersion] = useState(0);
  const signOffs = useSignOffs(connection, repo, change, version, onSessionInvalid);
  const [action, setAction] = useState<Action>({ kind: "idle" });
  const busy = action.kind === "busy";
  const supported = hasPasskeySupport();
  const ready = signer.kind === "loaded" && signer.passkeys > 0 && supported;

  async function start() {
    try {
      const evidence = await signOff(
        connection,
        { repo, commitId: change.commitId, treeId: change.treeId },
        (step) => {
          setAction({ kind: "busy", step });
        },
      );
      setAction({ kind: "done", evidence });
    } catch (thrown) {
      const failed = failure(thrown);
      if (failed.kind === "daemon" && isSessionInvalid(failed.error)) onSessionInvalid();
      else setAction({ kind: "failed", failure: failed });
    } finally {
      setVersion((v) => v + 1);
    }
  }

  return (
    <section className="signoff">
      <h4>Sign-offs</h4>
      {signOffs === null && <p className="secondary">Reading the sign-offs.</p>}
      {signOffs && "error" in signOffs && <ErrorView error={signOffs.error} />}
      {signOffs && "evidence" in signOffs && signOffs.evidence.length === 0 && (
        <p className="secondary">No sign-offs yet.</p>
      )}
      {signOffs && "evidence" in signOffs && signOffs.evidence.length > 0 && (
        <ul className="activity">
          {signOffs.evidence.map((evidence) => (
            <SignOffRow
              key={evidence.evidenceId}
              evidence={evidence}
              signer={signOffs.pins.get(evidence.evidenceId)}
            />
          ))}
        </ul>
      )}
      {signer.kind === "failed" && <ErrorView error={signer.error} />}
      {signer.kind === "loaded" && (
        <>
          <p className="actions">
            <button
              type="button"
              className="primary"
              disabled={!ready || busy}
              onClick={() => void start()}
            >
              Sign off
            </button>{" "}
            <span className="secondary">
              as {signer.identity.name}, at {window.location.origin}, on tree{" "}
              <span className="mono">{shortId(change.treeId)}</span> of change{" "}
              <span className="mono">{shortId(reverseHex(change.changeId))}</span>, with one passkey
              touch
            </span>
          </p>
          {!supported && <NoPasskeysHere />}
          {supported && signer.passkeys === 0 && (
            <p className="secondary">
              {signer.identity.name} has no passkey. <Link to="/passkeys">Add one</Link> to sign
              off.
            </p>
          )}
        </>
      )}
      {action.kind === "busy" && <p className="secondary">{STEP_WORDS[action.step]}</p>}
      {action.kind === "done" && <p role="status">Signed off: {presenceWords(action.evidence)}.</p>}
      {action.kind === "failed" && <FailureView failure={action.failure} />}
    </section>
  );
}
