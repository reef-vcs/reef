// Approving one sign-off (R3.43, ADR 0085): the page derives the challenge
// from what the approval names, so the touch approves what the page shows,
// then the passkey signs it and the daemon issues the key.
import { client, type Connection } from "../api/connection";
import { IdentityService, type Approval } from "../gen/reef/api/v1/identity_pb";
import { equal, hex, sha256 } from "./bytes";
import { approvalChallenge } from "./key";
import { assert, Refused } from "./passkey";

/** The challenge `approval`'s fields derive; the daemon's must be the same. */
export async function derivedChallenge(approval: Approval): Promise<Uint8Array> {
  const statement = hex(await sha256(approval.statement));
  if (approval.approves?.statementSha256 !== statement) {
    throw new Refused("The approval's statement is not the one it names. Nothing was approved.");
  }
  return approvalChallenge({
    authority: approval.authorityKeyId,
    key: approval.keyId,
    identityId: approval.identityId,
    session: approval.sessionId,
    statement,
    change: approval.changeId,
  });
}

/** Approves `approval` with a passkey touch; resolves to the approved approval. */
export async function approve(connection: Connection, approval: Approval): Promise<Approval> {
  const challenge = await derivedChallenge(approval);
  if (!equal(challenge, approval.challenge)) {
    throw new Refused("The daemon's challenge is not this approval's. Nothing was approved.");
  }
  const assertion = await assert({
    challenge,
    rpId: approval.rpId,
    credentialIds: approval.credentialIds,
  });
  const response = await client(connection, IdentityService).resolveApproval({
    id: approval.id,
    resolution: { case: "approve", value: assertion },
  });
  if (!response.approval) throw new Refused("The daemon returned no approval.");
  return response.approval;
}

export async function decline(connection: Connection, id: string): Promise<Approval> {
  const response = await client(connection, IdentityService).resolveApproval({
    id,
    resolution: { case: "decline", value: "" },
  });
  if (!response.approval) throw new Refused("The daemon returned no approval.");
  return response.approval;
}
