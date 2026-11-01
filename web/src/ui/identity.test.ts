// The design pass: avatar colors are a pure function of the name.
import { describe, expect, it } from "vitest";
import { avatarIndex, initialOf, splitIdentity } from "./identity";

describe("identity", () => {
  it("colors by the trimmed, lower-cased name", () => {
    expect(avatarIndex("Ann")).toBe(avatarIndex(" ann "));
    for (const name of ["claude@Aarons-MacBook-Pro", "e2e-fleet", "ann@host:42"]) {
      expect(avatarIndex(name)).toBe(avatarIndex(name.toUpperCase()));
      expect(avatarIndex(name)).toBeGreaterThanOrEqual(0);
      expect(avatarIndex(name)).toBeLessThan(8);
    }
    // Golden values: a change here recolors every identity.
    expect(["claude@Aarons-MacBook-Pro", "e2e-fleet", "ann@host:42"].map(avatarIndex))
      .toMatchInlineSnapshot(`
      [
        6,
        0,
        2,
      ]
    `);
  });

  it("spreads names over all eight colors", () => {
    const buckets = new Array<number>(8).fill(0);
    for (let i = 0; i < 200; i++) {
      const index = avatarIndex(`agent-${String(i)}`);
      buckets[index] = (buckets[index] ?? 0) + 1;
    }
    for (const n of buckets) expect(n).toBeGreaterThanOrEqual(10);
  });

  it("takes the first letter or digit before the @", () => {
    expect(initialOf("claude@Aarons-MacBook-Pro")).toBe("C");
    expect(initialOf("e2e-fleet")).toBe("E");
    expect(initialOf("_42bot")).toBe("4");
    expect(initialOf("élodie")).toBe("É");
    expect(initialOf("@host")).toBe("H");
    expect(initialOf("--")).toBe("?");
  });

  it("splits name and host", () => {
    expect(splitIdentity("claude@Aarons-MacBook-Pro")).toEqual({
      name: "claude",
      host: "Aarons-MacBook-Pro",
    });
    expect(splitIdentity("reviewer")).toEqual({ name: "reviewer", host: "" });
    expect(splitIdentity("a@b@c")).toEqual({ name: "a", host: "b@c" });
  });
});
