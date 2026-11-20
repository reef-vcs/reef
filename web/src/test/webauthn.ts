// A browser authenticator for the passkey tests: `PublicKeyCredential` and
// `navigator.credentials`, recording each ceremony's options and answering
// with fixed bytes, or refusing as a browser does.
import { vi } from "vitest";
import { base64url, concat, sha256 } from "../sign/bytes";

class FakeCredential {
  constructor(
    readonly rawId: ArrayBuffer,
    readonly response: object,
  ) {}
}

const bytes = (...values: number[]) => new Uint8Array(values).buffer;

export const CREDENTIAL_ID = new Uint8Array([0xc1, 0xc2, 0xc3]);

/** A key the fake authenticator signs assertions with, as a real one does. */
export interface FakeKey {
  credentialId: Uint8Array;
  sign: (data: Uint8Array) => Promise<Uint8Array>;
}

export interface FakeAuthenticator {
  gets: PublicKeyCredentialRequestOptions[];
  /** When set, `get()` answers with this key's real assertion. */
  key: FakeKey | null;
  creates: PublicKeyCredentialCreationOptions[];
  /** Each next ceremony throws `DOMException(name)` instead. */
  refuse: (name: string | null) => void;
}

export function installAuthenticator(): FakeAuthenticator {
  const fake: FakeAuthenticator & { refusal: string | null } = {
    gets: [],
    key: null,
    creates: [],
    refusal: null,
    refuse: (name) => {
      fake.refusal = name;
    },
  };
  const refused = () => {
    if (fake.refusal) throw new DOMException("refused", fake.refusal);
  };
  vi.stubGlobal("PublicKeyCredential", FakeCredential);
  Object.defineProperty(navigator, "credentials", {
    configurable: true,
    value: {
      get: async (options: CredentialRequestOptions) => {
        if (options.publicKey) fake.gets.push(options.publicKey);
        refused();
        if (fake.key && options.publicKey) {
          const given = options.publicKey.challenge;
          const challenge = ArrayBuffer.isView(given)
            ? new Uint8Array(given.buffer, given.byteOffset, given.byteLength)
            : new Uint8Array(given);
          const clientDataJSON = new TextEncoder().encode(
            JSON.stringify({
              type: "webauthn.get",
              challenge: base64url(challenge),
              origin: window.location.origin,
            }),
          );
          const rpIdHash = await sha256(new TextEncoder().encode(options.publicKey.rpId ?? ""));
          const authenticatorData = concat(rpIdHash, new Uint8Array([0x05, 0, 0, 0, 1]));
          const signature = await fake.key.sign(
            concat(authenticatorData, await sha256(clientDataJSON)),
          );
          return new FakeCredential(fake.key.credentialId.slice().buffer, {
            authenticatorData: authenticatorData.buffer,
            clientDataJSON: clientDataJSON.buffer,
            signature: signature.slice().buffer,
          });
        }
        return Promise.resolve(
          new FakeCredential(CREDENTIAL_ID.slice().buffer, {
            authenticatorData: bytes(0xad),
            clientDataJSON: bytes(0xcd),
            signature: bytes(0x51),
          }),
        );
      },
      create: (options: CredentialCreationOptions) => {
        if (options.publicKey) fake.creates.push(options.publicKey);
        refused();
        return Promise.resolve(
          new FakeCredential(bytes(0xc9), {
            clientDataJSON: bytes(0xcc),
            getAuthenticatorData: () => bytes(0xa1),
            getPublicKey: () => bytes(0x30, 0x59),
            getPublicKeyAlgorithm: () => -7,
          }),
        );
      },
    },
  });
  return fake;
}

/** A browser without WebAuthn. */
export function removeAuthenticator() {
  vi.unstubAllGlobals();
  Reflect.deleteProperty(navigator, "credentials");
}
