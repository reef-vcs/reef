import { describe, expect, it } from "vitest";
import { contrast } from "./contrast";

describe("contrast", () => {
  it("is 21:1 for black on white, either way round", () => {
    expect(contrast("#000000", "#ffffff")).toBeCloseTo(21, 5);
    expect(contrast("#ffffff", "#000000")).toBeCloseTo(21, 5);
  });

  it("reads the short form the production CSS minifier writes", () => {
    expect(contrast("#4b5a6e", "#fff")).toBeCloseTo(contrast("#4b5a6e", "#ffffff"), 10);
  });

  it("refuses other syntax", () => {
    expect(() => contrast("rgb(0 0 0)", "#fff")).toThrow(/not a #rrggbb or #rgb color/);
  });
});
