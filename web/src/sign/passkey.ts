// The browser's passkey ceremonies, through the native `PublicKeyCredential`
// API only (ADR 0047: no runtime dependency runs beside the token). The
// daemon checks every byte; the page only carries them.
import type { MessageInitShape } from "@bufbuild/protobuf";
import { reefError, type ReefError } from "../api/errors";
import type {
  PasskeyAssertionSchema,
  RegisterPasskeyRequestSchema,
  StartPasskeyRegistrationResponse,
} from "../gen/reef/api/v1/identity_pb";
import { buffer, utf8 } from "./bytes";

export type Assertion = MessageInitShape<typeof PasskeyAssertionSchema>;
export type Registration = Pick<
  MessageInitShape<typeof RegisterPasskeyRequestSchema>,
  "clientDataJson" | "authenticatorData" | "publicKey" | "algorithm"
>;

/** Why a step did not happen: the browser's refusal in words, or the daemon's error. */
export type Failure = { kind: "browser"; words: string } | { kind: "daemon"; error: ReefError };

/** A refusal the page itself makes, in words. */
export class Refused extends Error {}

export const NO_SUPPORT = "This browser has no passkey support.";

const BROWSER_WORDS: Record<string, string> = {
  // Browsers give the same error for a dismissed prompt and for an
  // authenticator that could not verify the person, so the words name both.
  NotAllowedError:
    "Cancelled: the passkey prompt was dismissed or timed out, or the authenticator could not verify you.",
  AbortError: "Cancelled.",
  InvalidStateError: "This authenticator already holds a passkey for this identity.",
  NotSupportedError: "This authenticator supports none of the passkey algorithms Reef accepts.",
  SecurityError: "This address cannot use passkeys. Open the web UI at its localhost URL.",
};

export function failure(thrown: unknown): Failure {
  if (thrown instanceof Refused) return { kind: "browser", words: thrown.message };
  if (thrown instanceof DOMException) {
    return { kind: "browser", words: BROWSER_WORDS[thrown.name] ?? thrown.message };
  }
  return { kind: "daemon", error: reefError(thrown) };
}

export function hasPasskeySupport(): boolean {
  return typeof PublicKeyCredential !== "undefined" && "credentials" in navigator;
}

function credentials(): CredentialsContainer {
  if (!hasPasskeySupport()) throw new Refused(NO_SUPPORT);
  return navigator.credentials;
}

function allow(ids: readonly Uint8Array[]): PublicKeyCredentialDescriptor[] {
  return ids.map((id) => ({ type: "public-key", id: buffer(id) }));
}

/** A user-verified assertion over `challenge` by one of `credentialIds`. */
export async function assert(request: {
  challenge: Uint8Array;
  rpId: string;
  credentialIds: readonly Uint8Array[];
}): Promise<Assertion> {
  const credential = await credentials().get({
    publicKey: {
      challenge: buffer(request.challenge),
      rpId: request.rpId,
      allowCredentials: allow(request.credentialIds),
      userVerification: "required",
    },
  });
  if (!(credential instanceof PublicKeyCredential)) throw new Refused("No passkey was chosen.");
  const response = credential.response as AuthenticatorAssertionResponse;
  return {
    credentialId: new Uint8Array(credential.rawId),
    authenticatorData: new Uint8Array(response.authenticatorData),
    clientDataJson: new Uint8Array(response.clientDataJSON),
    signature: new Uint8Array(response.signature),
  };
}

/** A new passkey for the registration `start` began. */
export async function create(start: StartPasskeyRegistrationResponse): Promise<Registration> {
  const credential = await credentials().create({
    publicKey: {
      challenge: buffer(start.challenge),
      rp: { id: start.rpId, name: "Reef" },
      user: { id: utf8(start.identityId), name: start.userName, displayName: start.userName },
      pubKeyCredParams: start.algorithms.map((alg) => ({ type: "public-key", alg })),
      excludeCredentials: allow(start.credentialIds),
      authenticatorSelection: { residentKey: "preferred", userVerification: "required" },
      attestation: "none",
    },
  });
  if (!(credential instanceof PublicKeyCredential)) throw new Refused("No passkey was made.");
  const response = credential.response as AuthenticatorAttestationResponse;
  const publicKey = response.getPublicKey();
  if (!publicKey) {
    throw new Refused("The authenticator made a passkey whose key this browser cannot read.");
  }
  return {
    clientDataJson: new Uint8Array(response.clientDataJSON),
    authenticatorData: new Uint8Array(response.getAuthenticatorData()),
    publicKey: new Uint8Array(publicKey),
    algorithm: response.getPublicKeyAlgorithm(),
  };
}
