import { afterEach, vi } from "vitest";

/** A phone's width, as `useNarrow` asks for it, until the test ends. */
export function narrow() {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query.includes("max-width: 599.98px"),
    addEventListener: () => undefined,
    removeEventListener: () => undefined,
  }));
}

afterEach(() => {
  vi.unstubAllGlobals();
});
