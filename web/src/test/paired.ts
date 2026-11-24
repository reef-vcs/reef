import { afterEach } from "vitest";
import { keepDevice } from "../device";
import { storeSession } from "../session";

export const PAIRED = { id: "00112233aabbccdd", name: "iphone", expiresAt: 1_800_000_000_000 };
export const PAIRED_TOKEN = `reef_${PAIRED.id}_${"x".repeat(43)}`;

/** Makes the page a paired phone's (R3.40), until the test ends. */
export function paired() {
  storeSession(PAIRED_TOKEN);
  keepDevice(PAIRED);
}

afterEach(() => {
  localStorage.clear();
});
