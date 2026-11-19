// Sign off in the page (R3.43 decision 3): a fresh key in this tab, an
// approval for exactly its statement and change, one passkey touch, then
// the same statement bytes signed and added. The key is dropped after.
import { client, type Connection } from "../api/connection";
import { EvidenceService, type Evidence } from "../gen/reef/api/v1/evidence_pb";
import { IdentityService } from "../gen/reef/api/v1/identity_pb";
import { equal } from "./bytes";
import { approve } from "./approval";
import { generateSessionKey, signProof, signStatement } from "./key";
import { Refused } from "./passkey";
import { statementPayload } from "./statement";

/** The change as the page shows it. */
export interface SignOffTarget {
  repo: string;
  commitId: string;
  treeId: string;
}

/** Where a sign-off is: each step waits on the daemon or on the person. */
export type SignOffStep = "requesting" | "touching" | "adding";

async function currentAuthority(connection: Connection): Promise<string> {
  const { authorities } = await client(connection, IdentityService).listAuthorities({});
  const current = authorities.find((authority) => authority.current);
  if (!current) throw new Refused("The daemon has no current authority to certify the key.");
  return current.keyId;
}

/**
 * Signs off `target` as the default identity: kind `signoff`, result `pass`,
 * on its tree. Resolves to the evidence as stored.
 */
export async function signOff(
  connection: Connection,
  target: SignOffTarget,
  onStep: (step: SignOffStep) => void,
): Promise<Evidence> {
  onStep("requesting");
  const [authority, key] = await Promise.all([currentAuthority(connection), generateSessionKey()]);
  const now = new Date();
  const statement = statementPayload(
    { kind: "signoff", result: "pass", startedOn: now, finishedOn: now },
    target.treeId,
  );
  const identity = "";
  const { approval } = await client(connection, IdentityService).requestApproval({
    identity,
    repo: target.repo,
    revision: target.commitId,
    statement,
    publicKey: key.spki,
    proof: await signProof(key, { authority, identity }),
  });
  if (!approval || approval.keyId !== key.id || !equal(approval.statement, statement)) {
    throw new Refused("The daemon's approval is not for this sign-off. Nothing was approved.");
  }
  onStep("touching");
  const approved = await approve(connection, approval);
  onStep("adding");
  const { evidence } = await client(connection, EvidenceService).addEvidence({
    repo: target.repo,
    envelope: await signStatement(key, statement),
    publicKey: key.spki,
    revision: approved.commitId,
  });
  if (!evidence) throw new Refused("The daemon stored no evidence.");
  return evidence;
}
