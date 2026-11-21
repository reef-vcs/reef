import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { PresenceKind, PresenceSchema } from "../gen/reef/api/v1/presence_pb";
import { AGENT_KINDS, STATE_KINDS, isAgentEntry, stateSignature } from "./kinds";

describe("presence kinds", () => {
  it("names every kind as an agent's or as state, once", () => {
    const kinds = Object.values(PresenceKind).filter(
      (kind): kind is PresenceKind => typeof kind === "number" && kind !== PresenceKind.UNSPECIFIED,
    );
    for (const kind of kinds) {
      expect(AGENT_KINDS.has(kind) !== STATE_KINDS.has(kind), PresenceKind[kind]).toBe(true);
    }
  });

  it("leaves the daemon's state entries out of agents, and signs them by repository", () => {
    const automatic = create(PresenceSchema, {
      id: "p1",
      kind: PresenceKind.AUTOMATIC,
      repoId: "r1",
    });
    expect(isAgentEntry(automatic)).toBe(false);
    expect(isAgentEntry(create(PresenceSchema, { kind: PresenceKind.EDITING }))).toBe(true);
    expect(stateSignature([automatic], PresenceKind.AUTOMATIC, "r1")).not.toBe("");
    expect(stateSignature([automatic], PresenceKind.AUTOMATIC, "r2")).toBe("");
    expect(stateSignature([automatic], PresenceKind.ATTENTION, "")).toBe("");
  });
});
