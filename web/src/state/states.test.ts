// W2: the mapping from Phase 2's change fields and outcomes to states.
import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { ChangeSchema } from "../gen/reef/api/v1/changes_pb";
import { ChangeOutcome } from "../gen/reef/api/v1/history_pb";
import { changeState, outcomeState } from "./states";

describe("changeState", () => {
  const flags = [false, true];
  for (const conflict of flags)
    for (const divergent of flags)
      for (const hidden of flags)
        for (const immutable of flags) {
          const expected = conflict
            ? { state: "conflicted", label: "conflicted" }
            : divergent
              ? { state: "conflicted", label: "divergent" }
              : hidden
                ? { state: "ended", label: "abandoned" }
                : immutable
                  ? { state: "landed", label: "landed" }
                  : { state: "draft", label: "draft" };
          it(`conflict=${conflict} divergent=${divergent} hidden=${hidden} immutable=${immutable}`, () => {
            const change = create(ChangeSchema, { conflict, divergent, hidden, immutable });
            expect(changeState(change)).toEqual(expected);
          });
        }
});

describe("outcomeState", () => {
  it.each([
    [ChangeOutcome.OPEN, "draft", "draft"],
    [ChangeOutcome.LANDED, "landed", "landed"],
    [ChangeOutcome.ABANDONED, "ended", "abandoned"],
    [ChangeOutcome.UNDONE, "ended", "undone"],
    [ChangeOutcome.UNSPECIFIED, "none", "unknown"],
    // A value newer than this build's schema.
    // eslint-disable-next-line @typescript-eslint/no-unsafe-enum-assignment
    [99 as ChangeOutcome, "none", "unknown"],
  ])("%s", (outcome, state, label) => {
    expect(outcomeState(outcome)).toEqual({ state, label });
  });
});
