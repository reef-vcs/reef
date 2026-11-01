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

// The design pass: every custom property, not only hex ones (shadows).
function allProperties(selector: RegExp): Map<string, string> {
  const match = selector.exec(css);
  if (!match) throw new Error(`no block for ${String(selector)}`);
  const body = css.slice(match.index + match[0].length, css.indexOf("}", match.index));
  return new Map([...body.matchAll(/--([\w-]+):\s*([^;]+);/g)].map((m) => [m[1]!, m[2]!.trim()]));
}

/** Hue in degrees of a `#rrggbb` color. */
function hue(hex: string): number {
  const [r, g, b] = [1, 3, 5].map((i) => parseInt(hex.slice(i, i + 2), 16) / 255) as [
    number,
    number,
    number,
  ];
  const max = Math.max(r, g, b);
  const min = Math.min(r, g, b);
  const d = max - min;
  if (d === 0) return 0;
  const h = max === r ? ((g - b) / d) % 6 : max === g ? (b - r) / d + 2 : (r - g) / d + 4;
  return (h * 60 + 360) % 360;
}

const BRAND_TOKENS = [
  "canvas",
  "control-border",
  "brand",
  "brand-strong",
  "brand-tint",
  "on-brand",
  "coral",
  "ocean",
  "on-ocean",
  "on-ocean-muted",
  "ocean-accent",
  ...Array.from({ length: 8 }, (_, n) => [
    `avatar-${String(n)}-bg`,
    `avatar-${String(n)}-fg`,
  ]).flat(),
];

describe("brand and neutral tokens", () => {
  it("defines the dark theme the same way for every property, shadows included", () => {
    expect([...allProperties(/:root:not\(\[data-theme="light"\]\)\s*\{/)]).toEqual([
      ...allProperties(/\n\[data-theme="dark"\]\s*\{/),
    ]);
  });

  for (const [theme, tokens] of Object.entries(themes)) {
    const get = (name: string) => {
      const value = tokens.get(name);
      if (!value) throw new Error(`--${name} is missing in ${theme}`);
      return value;
    };

    it(`defines every brand token in ${theme}`, () => {
      for (const name of BRAND_TOKENS) expect(tokens.get(name), name).toBeDefined();
    });

    it(`holds text pairs to 4.5:1 in ${theme}`, () => {
      const pairs: [string, string][] = [
        ["text", "canvas"],
        ["text-secondary", "canvas"],
        ["link", "surface"],
        ["link", "canvas"],
        ["link", "surface-raised"],
        ["link", "brand-tint"],
        ["brand-strong", "brand-tint"],
        ["on-brand", "brand"],
        ["on-ocean", "ocean"],
        ["on-ocean-muted", "ocean"],
        ["ocean-accent", "ocean"],
        ...STATES.flatMap((s): [string, string][] => [
          [`state-${s}`, "canvas"],
          [`state-${s}`, "brand-tint"],
        ]),
        ...Array.from({ length: 8 }, (_, n): [string, string] => [
          `avatar-${String(n)}-fg`,
          `avatar-${String(n)}-bg`,
        ]),
      ];
      for (const [fg, bg] of pairs) {
        expect(contrast(get(fg), get(bg)), `--${fg} on --${bg}`).toBeGreaterThanOrEqual(4.5);
      }
    });

    it(`holds edges and the coral to 3:1 in ${theme}`, () => {
      const pairs: [string, string][] = [
        ["control-border", "surface"],
        ["control-border", "canvas"],
        ["focus", "surface"],
        ["focus", "canvas"],
        ["coral", "ocean"],
      ];
      for (const [fg, bg] of pairs) {
        expect(contrast(get(fg), get(bg)), `--${fg} on --${bg}`).toBeGreaterThanOrEqual(3);
      }
    });

    it(`keeps brand teal at least 30° of hue from landed green, and off every state, in ${theme}`, () => {
      const distance = Math.abs(hue(get("brand")) - hue(get("state-landed")));
      expect(Math.min(distance, 360 - distance)).toBeGreaterThanOrEqual(30);
      for (const s of STATES) expect(get("brand")).not.toBe(get(`state-${s}`));
    });
  }
});
