// A browser authenticator for the passkey tests: `PublicKeyCredential` and
// `navigator.credentials`, recording each ceremony's options and answering
// with fixed bytes, or refusing as a browser does.
import { vi } from "vitest";

class FakeCredential {
  constructor(
    readonly rawId: ArrayBuffer,
    readonly response: object,
  ) {}
}

const bytes = (...values: number[]) => new Uint8Array(values).buffer;

export const CREDENTIAL_ID = new Uint8Array([0xc1, 0xc2, 0xc3]);

export interface FakeAuthenticator {
  gets: PublicKeyCredentialRequestOptions[];
  creates: PublicKeyCredentialCreationOptions[];
  /** Each next ceremony throws `DOMException(name)` instead. */
  refuse: (name: string | null) => void;
}

export function installAuthenticator(): FakeAuthenticator {
  const fake: FakeAuthenticator & { refusal: string | null } = {
    gets: [],
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
      get: (options: CredentialRequestOptions) => {
        if (options.publicKey) fake.gets.push(options.publicKey);
        refused();
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
