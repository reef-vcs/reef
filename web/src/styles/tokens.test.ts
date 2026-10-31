// @vitest-environment node
// W1: every state has a foreground and a tint in both themes, each
// foreground is at least 4.5:1 on the surface and on its own tint, and the
// two copies of the dark theme agree.
import { readFileSync } from "node:fs";
import { fileURLToPath } from "node:url";
import { describe, expect, it } from "vitest";
import { contrast } from "../state/contrast";
import { STATES } from "../state/states";

// Read from disk: Vitest stubs CSS imports.
const css = readFileSync(fileURLToPath(new URL("./tokens.css", import.meta.url)), "utf8");

function block(selector: RegExp): Map<string, string> {
  const match = selector.exec(css);
  if (!match) throw new Error(`no block for ${String(selector)}`);
  const body = css.slice(match.index + match[0].length, css.indexOf("}", match.index));
  return new Map(
    [...body.matchAll(/--([\w-]+):\s*(#[0-9a-f]{6});/gi)].map((m) => [m[1]!, m[2]!.toLowerCase()]),
  );
}

const themes = {
  light: block(/\[data-theme="light"\]\s*\{/),
  dark: block(/\n\[data-theme="dark"\]\s*\{/),
};
const darkByPreference = block(/:root:not\(\[data-theme="light"\]\)\s*\{/);

describe("state tokens", () => {
  it("defines the dark theme the same way for the preference and the attribute", () => {
    expect([...darkByPreference]).toEqual([...themes.dark]);
  });

  for (const [theme, tokens] of Object.entries(themes)) {
    for (const state of STATES) {
      it(`${state} meets 4.5:1 in ${theme}`, () => {
        const surface = tokens.get("surface");
        const fg = tokens.get(`state-${state}`);
        const tint = tokens.get(`state-${state}-tint`);
        expect(surface).toBeDefined();
        expect(fg).toBeDefined();
        expect(tint).toBeDefined();
        expect(contrast(fg!, surface!)).toBeGreaterThanOrEqual(4.5);
        expect(contrast(fg!, tint!)).toBeGreaterThanOrEqual(4.5);
      });
    }
  }
});
