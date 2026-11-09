// W2: the mapping from Phase 2's change fields and outcomes to states.
import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { ChangeSchema } from "../gen/reef/api/v1/changes_pb";
import { ChangeOutcome } from "../gen/reef/api/v1/history_pb";
import { ChangeLifecycleSchema, LifecycleState } from "../gen/reef/api/v1/lifecycle_pb";
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

describe("changeState with a lifecycle (R3.09)", () => {
  it.each([
    [LifecycleState.PROPOSED, true, "proposed", "proposed"],
    [LifecycleState.QUEUED, true, "queued", "queued"],
    [LifecycleState.LANDED, true, "landed", "landed"],
    [LifecycleState.DECLINED, true, "ended", "declined"],
    [LifecycleState.WITHDRAWN, true, "ended", "withdrawn"],
    [LifecycleState.SUPERSEDED, true, "ended", "superseded"],
    // Derived (not recorded): the flags decide, as before.
    [LifecycleState.LANDED, false, "draft", "draft"],
    [LifecycleState.DRAFT, false, "draft", "draft"],
  ])("%s recorded=%s", (state, recorded, expected, label) => {
    const change = create(ChangeSchema, {
      lifecycle: create(ChangeLifecycleSchema, { state, recorded }),
    });
    expect(changeState(change)).toEqual({ state: expected, label });
  });

  it("an abandoned proposal reads abandoned", () => {
    const change = create(ChangeSchema, {
      hidden: true,
      lifecycle: create(ChangeLifecycleSchema, { state: LifecycleState.PROPOSED, recorded: true }),
    });
    expect(changeState(change)).toEqual({ state: "ended", label: "abandoned" });
  });

  it("conflicts still come first", () => {
    const change = create(ChangeSchema, {
      conflict: true,
      lifecycle: create(ChangeLifecycleSchema, { state: LifecycleState.PROPOSED, recorded: true }),
    });
    expect(changeState(change).state).toBe("conflicted");
  });
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
