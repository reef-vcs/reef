// Change ids as `reef log` prints them: jj's "reverse hex", where the digits
// `0` to `f` become the letters `z` to `k`. The API carries hex; URLs and
// labels use reverse hex, and either spelling is accepted back, since the two
// alphabets do not overlap.

const HEX = "0123456789abcdef";
const REVERSE = "zyxwvutsrqponmlk";

/** A hex id in reverse hex, as `reef_client::json::reverse_hex` spells it. */
export function reverseHex(hex: string): string {
  let out = "";
  for (const c of hex) {
    const i = HEX.indexOf(c);
    out += i < 0 ? c : (REVERSE[i] ?? c);
  }
  return out;
}

/** Hex from either spelling, or null for anything else (mixed, empty, other characters). */
export function hexFromEither(id: string): string | null {
  const lower = id.toLowerCase();
  if (/^[0-9a-f]+$/.test(lower)) return lower;
  if (!/^[k-z]+$/.test(lower)) return null;
  let out = "";
  for (const c of lower) out += HEX[REVERSE.indexOf(c)] ?? "";
  return out;
}

/** The first 8 characters, as the app shows ids. */
export function shortId(id: string): string {
  return id.slice(0, 8);
}
