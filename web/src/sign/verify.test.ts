// The page's check of a passkey's assertion (R3.43): it accepts the held
// key's user-verified assertion over the challenge at the origin, for each
// algorithm WebCrypto signs here, and refuses any other key, challenge,
// origin, type or missing verification.
import { describe, expect, it } from "vitest";
import { buffer, concat, sha256, utf8 } from "./bytes";
import { assertionVerifies, ecdsaRaw, type HeldKey } from "./verify";

const ORIGIN = "http://localhost:4123";
const CHALLENGE = new Uint8Array(32).fill(7);
const RP = "localhost";
const RP_HASH = await sha256(utf8(RP));
const VERIFIED = concat(RP_HASH, new Uint8Array([0x05, 0, 0, 0, 1]));

function clientData(init: { type?: string; challenge?: string; origin?: string } = {}) {
  return utf8(
    JSON.stringify({
      type: init.type ?? "webauthn.get",
      challenge: init.challenge ?? "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc",
      origin: init.origin ?? ORIGIN,
    }),
  );
}

/** A DER `ECDSA-Sig-Value` from WebCrypto's `r ‖ s`. */
function der(raw: Uint8Array): Uint8Array {
  const integer = (half: Uint8Array) => {
    let value = half;
    while (value.length > 1 && value[0] === 0 && (value[1] ?? 0) < 0x80) value = value.subarray(1);
    if ((value[0] ?? 0) >= 0x80) value = concat(new Uint8Array([0]), value);
    return concat(new Uint8Array([0x02, value.length]), value);
  };
  const body = concat(integer(raw.subarray(0, 32)), integer(raw.subarray(32)));
  return concat(new Uint8Array([0x30, body.length]), body);
}

type Signer = (data: Uint8Array) => Promise<Uint8Array>;

async function ed25519(): Promise<{ key: HeldKey; sign: Signer }> {
  const pair = await crypto.subtle.generateKey("Ed25519", false, ["sign", "verify"]);
  const spki = new Uint8Array(await crypto.subtle.exportKey("spki", pair.publicKey));
  return {
    key: { publicKey: spki, algorithm: -8 },
    sign: async (data) =>
      new Uint8Array(await crypto.subtle.sign("Ed25519", pair.privateKey, buffer(data))),
  };
}

async function es256(): Promise<{ key: HeldKey; sign: Signer }> {
  const pair = await crypto.subtle.generateKey({ name: "ECDSA", namedCurve: "P-256" }, false, [
    "sign",
    "verify",
  ]);
  const spki = new Uint8Array(await crypto.subtle.exportKey("spki", pair.publicKey));
  return {
    key: { publicKey: spki, algorithm: -7 },
    sign: async (data) =>
      der(
        new Uint8Array(
          await crypto.subtle.sign(
            { name: "ECDSA", hash: "SHA-256" },
            pair.privateKey,
            buffer(data),
          ),
        ),
      ),
  };
}

async function asserted(sign: Signer, client = clientData(), authenticatorData = VERIFIED) {
  const signature = await sign(concat(authenticatorData, await sha256(client)));
  return { authenticatorData, clientDataJson: client, signature };
}

describe("assertionVerifies", () => {
  it.each([
    ["EdDSA", ed25519],
    ["ES256", es256],
  ])("accepts the held %s key's assertion and refuses everything else", async (_, make) => {
    const { key, sign } = await make();
    const other = await make();
    expect(await assertionVerifies(key, await asserted(sign), CHALLENGE, ORIGIN, RP)).toBe(true);
    expect(
      await assertionVerifies(other.key, await asserted(sign), CHALLENGE, ORIGIN, RP),
      "a row whose key is not the authenticator's",
    ).toBe(false);
    expect(
      await assertionVerifies(key, await asserted(sign), new Uint8Array(32), ORIGIN, RP),
      "another challenge",
    ).toBe(false);
    expect(
      await assertionVerifies(key, await asserted(sign), CHALLENGE, "http://localhost:9", RP),
      "another origin",
    ).toBe(false);
    expect(
      await assertionVerifies(
        key,
        await asserted(sign, clientData({ type: "webauthn.create" })),
        CHALLENGE,
        ORIGIN,
        RP,
      ),
      "a registration's client data",
    ).toBe(false);
    expect(
      await assertionVerifies(key, await asserted(sign), CHALLENGE, ORIGIN, "example.com"),
      "another RP ID",
    ).toBe(false);
    const framed = utf8(
      JSON.stringify({
        type: "webauthn.get",
        challenge: "BwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwcHBwc",
        origin: ORIGIN,
        crossOrigin: true,
      }),
    );
    expect(
      await assertionVerifies(key, await asserted(sign, framed), CHALLENGE, ORIGIN, RP),
      "a cross-origin frame",
    ).toBe(false);
    const unverified = concat(VERIFIED.subarray(0, 32), new Uint8Array([0x01, 0, 0, 0, 1]));
    expect(
      await assertionVerifies(
        key,
        await asserted(sign, clientData(), unverified),
        CHALLENGE,
        ORIGIN,
        RP,
      ),
      "no user verification",
    ).toBe(false);
  });

  it("reads DER ECDSA signatures with and without leading zeros", () => {
    const raw = concat(new Uint8Array(32).fill(0x80), new Uint8Array(32).fill(0x01));
    expect(ecdsaRaw(der(raw))).toEqual(raw);
    expect(ecdsaRaw(new Uint8Array([0x30, 0x01, 0x02]))).toBeNull();
  });
});
