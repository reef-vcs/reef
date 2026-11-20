// The signer against `reef-evidence`'s bytes (R3.43 test 3): the fixture
// `crates/reef-evidence/tests/web_signer.rs` writes and checks, for a key
// from a fixed seed, so every signature is deterministic.
import { describe, expect, it } from "vitest";
import stored from "../../../crates/reef-evidence/tests/fixtures/web-signer.json";
import { base64, base64url, concat, hex, sha256 } from "./bytes";
import { approvalChallenge, removalChallenge, sessionKeyOf, signProof, signStatement } from "./key";
import { statementPayload, type Outcome } from "./statement";

interface Fixture {
  inputs: {
    seed: string;
    authority: string;
    identity: string;
    identityId: string;
    session: string;
    tree: string;
    kind: string;
    result: Outcome;
    startedMs: number;
    finishedMs: number;
    change: string;
    passkey: string;
  };
  publicKey: string;
  keyId: string;
  proof: string;
  statements: { target: string | null; payload: string; sha256: string; envelope: string }[];
  approvalChallenge: string;
  removalChallenge: string;
}

const fixture = stored as Fixture;
const { inputs } = fixture;
const text = new TextDecoder();

/** The fixture's key: its seed as PKCS#8, and the public half from its JWK. */
async function fixtureKey() {
  const seed = Uint8Array.from(atob(inputs.seed), (c) => c.charCodeAt(0));
  const pkcs8 = concat(
    Uint8Array.of(
      0x30,
      0x2e,
      0x02,
      0x01,
      0x00,
      0x30,
      0x05,
      0x06,
      0x03,
      0x2b,
      0x65,
      0x70,
      0x04,
      0x22,
      0x04,
      0x20,
    ),
    seed,
  );
  const privateKey = await crypto.subtle.importKey("pkcs8", pkcs8, "Ed25519", true, ["sign"]);
  const { x } = await crypto.subtle.exportKey("jwk", privateKey);
  const publicKey = await crypto.subtle.importKey(
    "jwk",
    { kty: "OKP", crv: "Ed25519", x: x ?? "" },
    "Ed25519",
    true,
    ["verify"],
  );
  return sessionKeyOf(privateKey, publicKey);
}

describe("the signer", () => {
  it("names the key as reef-evidence does", async () => {
    const key = await fixtureKey();
    expect(base64(key.spki)).toBe(fixture.publicKey);
    expect(key.id).toBe(fixture.keyId);
  });

  it("signs the proof of possession", async () => {
    const proof = await signProof(await fixtureKey(), inputs);
    expect(base64(proof)).toBe(fixture.proof);
  });

  it("writes each statement and envelope byte for byte", async () => {
    const key = await fixtureKey();
    for (const statement of fixture.statements) {
      const payload = statementPayload(
        {
          kind: inputs.kind,
          result: inputs.result,
          target: statement.target ?? undefined,
          startedOn: new Date(inputs.startedMs),
          finishedOn: new Date(inputs.finishedMs),
        },
        inputs.tree,
      );
      expect(text.decode(payload)).toBe(statement.payload);
      expect(hex(await sha256(payload))).toBe(statement.sha256);
      expect(text.decode(await signStatement(key, payload))).toBe(statement.envelope);
    }
  });

  it("derives the approval and removal challenges", async () => {
    const key = await fixtureKey();
    const challenge = await approvalChallenge({
      authority: inputs.authority,
      key: key.id,
      identityId: inputs.identityId,
      session: inputs.session,
      statement: fixture.statements[0]?.sha256 ?? "",
      change: inputs.change,
    });
    expect(base64url(challenge)).toBe(fixture.approvalChallenge);
    expect(base64url(await removalChallenge(inputs.passkey))).toBe(fixture.removalChallenge);
  });
});
