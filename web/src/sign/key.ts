// A session key held by this tab (R3.43): WebCrypto Ed25519 whose private
// half never leaves the browser, generated for one sign-off and dropped
// after it. Its proof of possession and the passkey challenges are derived
// as `reef-evidence` derives them (`session::proof_payload`,
// `webauthn::approval_payload`, `webauthn::passkey_change_payload`).
import { buffer, hex, sha256, utf8 } from "./bytes";
import { envelopeBytes, pae, PAYLOAD_TYPE } from "./statement";

export interface SessionKey {
  readonly privateKey: CryptoKey;
  /** DER SubjectPublicKeyInfo, 44 bytes. */
  readonly spki: Uint8Array<ArrayBuffer>;
  /** SHA-256 of `spki`, lowercase hex. */
  readonly id: string;
}

export async function sessionKeyOf(
  privateKey: CryptoKey,
  publicKey: CryptoKey,
): Promise<SessionKey> {
  const spki = new Uint8Array(await crypto.subtle.exportKey("spki", publicKey));
  return { privateKey, spki, id: hex(await sha256(spki)) };
}

export async function generateSessionKey(): Promise<SessionKey> {
  const pair = await crypto.subtle.generateKey({ name: "Ed25519" }, false, ["sign", "verify"]);
  return sessionKeyOf(pair.privateKey, pair.publicKey);
}

async function sign(key: SessionKey, message: Uint8Array): Promise<Uint8Array<ArrayBuffer>> {
  return new Uint8Array(await crypto.subtle.sign("Ed25519", key.privateKey, buffer(message)));
}

/**
 * The key's proof of possession for a sign-off approval: the actor `human`
 * and no session, `identity` exactly as the request names it.
 */
export function signProof(
  key: SessionKey,
  { authority, identity }: { authority: string; identity: string },
): Promise<Uint8Array<ArrayBuffer>> {
  const payload = `reef-session-key-proof/v1\n${authority}\n${key.id}\n${identity}\nhuman\n\n`;
  return sign(key, pae("application/vnd.reef-vcs.session-key-proof", utf8(payload)));
}

/** `statement` signed in a canonical evidence envelope. */
export async function signStatement(
  key: SessionKey,
  statement: Uint8Array,
): Promise<Uint8Array<ArrayBuffer>> {
  const sig = await sign(key, pae(PAYLOAD_TYPE, statement));
  return envelopeBytes(PAYLOAD_TYPE, statement, key.id, sig);
}

/** What one sign-off's passkey touch approves. */
export interface ApprovalFor {
  authority: string;
  key: string;
  identityId: string;
  session: string;
  /** SHA-256 of the statement, lowercase hex. */
  statement: string;
  change: string;
}

/** The WebAuthn challenge of a sign-off's assertion. */
export function approvalChallenge(approval: ApprovalFor): Promise<Uint8Array<ArrayBuffer>> {
  const payload =
    "reef-presence-approval/v1\n" +
    [
      approval.authority,
      approval.key,
      approval.identityId,
      "human",
      approval.session,
      approval.statement,
      approval.change,
    ].join("\n") +
    "\n";
  return sha256(pae("application/vnd.reef-vcs.presence-approval", utf8(payload)));
}

/** The WebAuthn challenge of an assertion that approves removing `passkey`. */
export function removalChallenge(passkey: string): Promise<Uint8Array<ArrayBuffer>> {
  return sha256(pae("application/vnd.reef-vcs.passkey-change", utf8(`remove\n${passkey}\n`)));
}
