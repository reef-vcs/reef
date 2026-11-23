// A paired phone (R3.40): what the daemon answered when a code was typed
// here. The token is the page's session (`session.ts`); this record is for
// the page's words and for what it offers, and the daemon decides.
import { localStorageOrNull, storedSession, stripSession, takeSession } from "./session";

export const DEVICE_KEY = "reef.device";

/** Why a write is off on a paired phone. */
export const DEVICE_READS =
  "A paired phone only reads. Do this in the web UI on the computer Reef runs on.";

export interface Device {
  /** Its token's id, which the token holds. */
  id: string;
  name: string;
  /** Milliseconds since the epoch; null when the daemon named no end. */
  expiresAt: number | null;
}

function parse(text: string): Device | null {
  const { id, name, expiresAt } = JSON.parse(text) as Partial<Record<keyof Device, unknown>>;
  if (typeof id !== "string" || typeof name !== "string") return null;
  return { id, name, expiresAt: typeof expiresAt === "number" ? expiresAt : null };
}

/** This device, if the session here is a paired one's. */
export function device(storage: Storage | null = localStorageOrNull()): Device | null {
  try {
    const text = storage?.getItem(DEVICE_KEY);
    const paired = text ? parse(text) : null;
    // The daemon refuses it from then on.
    if (paired?.expiresAt != null && paired.expiresAt <= Date.now()) return null;
    // A link that brought another token took the pairing's place.
    return paired && storedSession(storage)?.startsWith(`reef_${paired.id}_`) ? paired : null;
  } catch {
    return null;
  }
}

export function keepDevice(paired: Device, storage: Storage | null = localStorageOrNull()) {
  try {
    storage?.setItem(DEVICE_KEY, JSON.stringify(paired));
  } catch {
    // Storage refused: the daemon still refuses what the page then offers.
  }
}

/** Forgets the pairing that `token` is of, and no later one. */
export function endDevice(token: string, storage: Storage | null = localStorageOrNull()) {
  try {
    const text = storage?.getItem(DEVICE_KEY);
    if (text && token.startsWith(`reef_${parse(text)?.id ?? ""}_`)) storage?.removeItem(DEVICE_KEY);
  } catch {
    // Storage refused, or not a pairing: nothing is forgotten.
  }
}

/**
 * The page's token (`takeSession`), but a paired phone keeps its own: any
 * site can open this page with a fragment, and a pairing cannot be opened
 * again from a link.
 */
export function openSession(): string | null {
  if (!device()) return takeSession();
  stripSession();
  return storedSession();
}
