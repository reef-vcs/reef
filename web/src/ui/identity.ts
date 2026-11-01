// Identities as people read them (the design pass): an avatar color that is
// a pure function of the name, the letter in it, and `name@host` split so the
// host can be quieter than the name.

/** FNV-1a over the trimmed, lower-cased name, modulo the 8 avatar colors. */
export function avatarIndex(name: string): number {
  let hash = 0x811c9dc5;
  const key = name.trim().toLowerCase();
  for (let i = 0; i < key.length; i++) {
    hash ^= key.charCodeAt(i);
    hash = Math.imul(hash, 0x01000193) >>> 0;
  }
  return hash % 8;
}

const LETTER = /[\p{L}\p{N}]/u;

/** The first letter or digit of the part before `@`, else of the name. */
export function initialOf(name: string): string {
  const local = name.split("@")[0] ?? "";
  const match = LETTER.exec(local) ?? LETTER.exec(name);
  return match ? match[0].toUpperCase() : "?";
}

/** `claude@Aarons-MacBook-Pro` is `{ name: "claude", host: "Aarons-MacBook-Pro" }`. */
export function splitIdentity(identity: string): { name: string; host: string } {
  const at = identity.indexOf("@");
  if (at <= 0) return { name: identity, host: "" };
  return { name: identity.slice(0, at), host: identity.slice(at + 1) };
}
