import { describe, expect, it } from "vitest";
import { inboxTitle } from "./needs";

describe("the inbox title", () => {
  it("titles the page with the count", () => {
    expect(inboxTitle(3)).toBe("(3) Inbox — Reef");
    expect(inboxTitle(0)).toBe("Inbox — Reef");
    expect(inboxTitle(1, "reef")).toBe("(1) Inbox · reef — Reef");
  });
});
