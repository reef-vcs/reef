// R2.19 test 2: change ids in both spellings.
import { describe, expect, it } from "vitest";
import { hexFromEither, reverseHex, shortId } from "./ids";

describe("ids", () => {
  it("spells hex in reverse hex as reef log does", () => {
    // The pair `reef ws create` printed for R2.19's brief workspace.
    expect(reverseHex("1a8feb92e605c86ea040897355642577")).toBe("yprkloqxltzunrtlpzvzrqswuutvxuss");
  });

  it("reads either spelling back as hex", () => {
    expect(hexFromEither("yprkloqxltzunrtlpzvzrqswuutvxuss")).toBe(
      "1a8feb92e605c86ea040897355642577",
    );
    expect(hexFromEither("1A8FEB92")).toBe("1a8feb92");
    expect(hexFromEither("1a8fkz")).toBeNull();
    expect(hexFromEither("ghij")).toBeNull();
    expect(hexFromEither("")).toBeNull();
    expect(hexFromEither("abc/")).toBeNull();
  });

  it("shortens to 8 characters", () => {
    expect(shortId("yprkloqxltzunrtl")).toBe("yprkloqx");
  });
});
