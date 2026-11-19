// Checking a passkey's assertion in the page (R3.43), as `reef-evidence`
// checks one: the client data's type, challenge and origin, and the
// signature over `authenticatorData ‖ SHA-256(clientDataJSON)` with the
// public key the daemon holds for the passkey. The Passkeys page uses it
// before it pins a passkey, so a row whose key is not the one the person's
// authenticator holds (an agent's registration that reuses the person's
// credential id) is never pinned.
import { base64url, buffer, concat, equal, sha256, utf8 } from "./bytes";

/** The passkey as the daemon holds it. */
export interface HeldKey {
  publicKey: Uint8Array;
  algorithm: number;
}

/** What the authenticator returned. */
export interface Asserted {
  authenticatorData: Uint8Array;
  clientDataJson: Uint8Array;
  signature: Uint8Array;
}

const ES256 = -7;
const EDDSA = -8;
const RS256 = -257;
/** The authenticator data's UP and UV flags. */
const USER_PRESENT_AND_VERIFIED = 0x05;

/** A DER `ECDSA-Sig-Value` as WebCrypto's 64 bytes, `r ‖ s`; `null` when malformed. */
export function ecdsaRaw(der: Uint8Array): Uint8Array | null {
  if (der[0] !== 0x30 || der[1] !== der.length - 2) return null;
  const parts: Uint8Array[] = [];
  let at = 2;
  for (let n = 0; n < 2; n++) {
    const length = der[at + 1];
    if (der[at] !== 0x02 || length === undefined || at + 2 + length > der.length) return null;
    let value = der.subarray(at + 2, at + 2 + length);
    while (value.length > 32 && value[0] === 0) value = value.subarray(1);
    if (value.length > 32) return null;
    const padded = new Uint8Array(32);
    padded.set(value, 32 - value.length);
    parts.push(padded);
    at += 2 + length;
  }
  return at === der.length ? concat(...parts) : null;
}

async function signatureVerifies(key: HeldKey, signed: Uint8Array, signature: Uint8Array) {
  const spki = buffer(key.publicKey);
  switch (key.algorithm) {
    case EDDSA: {
      const imported = await crypto.subtle.importKey("spki", spki, { name: "Ed25519" }, false, [
        "verify",
      ]);
      return crypto.subtle.verify("Ed25519", imported, buffer(signature), buffer(signed));
    }
    case ES256: {
      const raw = ecdsaRaw(signature);
      if (!raw) return false;
      const imported = await crypto.subtle.importKey(
        "spki",
        spki,
        { name: "ECDSA", namedCurve: "P-256" },
        false,
        ["verify"],
      );
      return crypto.subtle.verify(
        { name: "ECDSA", hash: "SHA-256" },
        imported,
        buffer(raw),
        buffer(signed),
      );
    }
    case RS256: {
      const imported = await crypto.subtle.importKey(
        "spki",
        spki,
        { name: "RSASSA-PKCS1-v1_5", hash: "SHA-256" },
        false,
        ["verify"],
      );
      return crypto.subtle.verify("RSASSA-PKCS1-v1_5", imported, buffer(signature), buffer(signed));
    }
    default:
      return false;
  }
}

/**
 * Whether `asserted` is `key`'s user-verified assertion over `challenge`
 * for `rpId`, made at `origin` and not in a cross-origin frame. Any
 * malformed input is `false`.
 */
export async function assertionVerifies(
  key: HeldKey,
  asserted: Asserted,
  challenge: Uint8Array,
  origin: string,
  rpId: string,
): Promise<boolean> {
  try {
    const client = JSON.parse(new TextDecoder().decode(asserted.clientDataJson)) as {
      type?: unknown;
      challenge?: unknown;
      origin?: unknown;
      crossOrigin?: unknown;
    };
    const flags = asserted.authenticatorData[32] ?? 0;
    const rpIdHash = asserted.authenticatorData.subarray(0, 32);
    if (
      !equal(rpIdHash, await sha256(utf8(rpId))) ||
      client.crossOrigin === true ||
      (flags & USER_PRESENT_AND_VERIFIED) !== USER_PRESENT_AND_VERIFIED ||
      client.type !== "webauthn.get" ||
      client.challenge !== base64url(challenge) ||
      client.origin !== origin
    ) {
      return false;
    }
    const signed = concat(asserted.authenticatorData, await sha256(asserted.clientDataJson));
    return await signatureVerifies(key, signed, asserted.signature);
  } catch {
    return false;
  }
}
