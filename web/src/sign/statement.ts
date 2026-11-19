// The evidence statement and its DSSE envelope, byte for byte as
// `reef-evidence` writes them (`Claim::payload`, `Envelope::to_bytes`):
// compact JSON with keys in the format's order. `JSON.stringify` escapes as
// serde_json does for every string a Rust `String` can hold.
import { base64, concat, utf8 } from "./bytes";

export const PAYLOAD_TYPE = "application/vnd.in-toto+json";

export type Outcome = "pass" | "fail" | "error";

/** A claim about a tree, as its producer builds it. */
export interface Claim {
  kind: string;
  result: Outcome;
  /** Absent for the whole check of `kind`. */
  target?: string | undefined;
  startedOn: Date;
  finishedOn: Date;
}

/** DSSE v1's pre-authentication encoding: what is signed. */
export function pae(payloadType: string, payload: Uint8Array): Uint8Array<ArrayBuffer> {
  const type = utf8(payloadType);
  return concat(
    utf8(`DSSEv1 ${String(type.length)} `),
    type,
    utf8(` ${String(payload.length)} `),
    payload,
  );
}

/** The in-toto statement of `claim` about the git tree `tree`. */
export function statementPayload(claim: Claim, tree: string): Uint8Array<ArrayBuffer> {
  return utf8(
    JSON.stringify({
      _type: "https://in-toto.io/Statement/v1",
      subject: [{ name: "tree", digest: { gitTree: tree } }],
      predicateType: "urn:reef-vcs:evidence:v1",
      predicate: {
        kind: claim.kind,
        result: claim.result,
        ...(claim.target ? { target: claim.target } : {}),
        startedOn: claim.startedOn.toISOString(),
        finishedOn: claim.finishedOn.toISOString(),
      },
    }),
  );
}

/** The canonical envelope of `payload` with one signature. */
export function envelopeBytes(
  payloadType: string,
  payload: Uint8Array,
  keyid: string,
  sig: Uint8Array,
): Uint8Array<ArrayBuffer> {
  return utf8(
    JSON.stringify({
      payload: base64(payload),
      payloadType,
      signatures: [{ keyid, sig: base64(sig) }],
    }),
  );
}
