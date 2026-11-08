// R3.44: the SDK's provenance declarations, hashed here and never holding the text.

import { createHash } from "node:crypto";

import { describe, expect, it } from "vitest";

import { instructionSha256 } from "../../src/index.js";
import { SessionActor } from "../../src/gen/reef/api/v1/identity_pb.js";
import { createPlan, declaration } from "../../src/provenance.js";

const sha = (text: string | Uint8Array): string => createHash("sha256").update(text).digest("hex");

describe("provenance", () => {
  it("hashes the instruction's exact bytes and sends only the digest", () => {
    const text = "fix the parser\n";
    const message = declaration({ model: "m/1", instruction: text }, true);
    expect(message.instructionSha256).toBe(sha(text));
    expect(JSON.stringify(message)).not.toContain("fix the parser");
    expect(instructionSha256("café")).toBe(sha(Buffer.from("café", "utf8")));
    expect(instructionSha256(new Uint8Array([1, 2]))).toBe(sha(new Uint8Array([1, 2])));
  });

  it("clears with an empty field on declare, and declares nothing with one at creation", () => {
    const created = declaration(
      { task: "", model: " m ", actor: "human", identity: "alice" },
      true,
    );
    expect(created).toEqual({ model: "m", actor: SessionActor.HUMAN, identity: "alice" });
    const cleared = declaration({ task: "", instructionSha256: "" }, false);
    expect(cleared).toEqual({ task: "", instructionSha256: "" });
  });

  it("ignores an environment value the daemon would refuse", () => {
    expect(
      createPlan(undefined, { REEF_MODEL: "two words", REEF_TASK: "tab\there" }),
    ).toBeUndefined();
    expect(createPlan({ task: "T-1" }, { REEF_MODEL: "two words" })).toEqual({
      declaration: { task: "T-1" },
      explicit: true,
    });
  });

  it("clears the instruction with an empty one, and counts no empty field as given", () => {
    expect(declaration({ instruction: "" }, false)).toEqual({ instructionSha256: "" });
    expect(createPlan({ model: "" }, {})).toBeUndefined();
  });

  it("refuses what it cannot send", () => {
    expect(() =>
      declaration({ instruction: "x", instructionSha256: "0".repeat(64) }, true),
    ).toThrow(TypeError);
    expect(() => declaration({ identity: "alice" }, false)).toThrow(TypeError);
    expect(() => declaration({ actor: "toString" as "agent" }, true)).toThrow(TypeError);
  });

  it("takes REEF_MODEL and REEF_TASK as defaults, and nothing else from the environment", () => {
    expect(createPlan(undefined, {})).toBeUndefined();
    const env = {
      REEF_MODEL: "env-model",
      REEF_TASK: " T-1 ",
      CLAUDECODE: "1",
      CLAUDE_CODE_SESSION_ID: "s",
      AI_AGENT: "claude-code_2-1-316_agent",
    };
    expect(createPlan(undefined, env)).toEqual({
      declaration: { model: "env-model", task: "T-1" },
      explicit: false,
    });
    expect(createPlan({ model: "mine" }, env)).toEqual({
      declaration: { model: "mine", task: "T-1" },
      explicit: true,
    });
  });
});
