// R3.09: a transition's sentence names what it did to the change.
import { describe, expect, it } from "vitest";
import { LifecycleState } from "../gen/reef/api/v1/lifecycle_pb";
import { OperationKind, SnapshotKind } from "../gen/reef/api/v1/operations_pb";
import { operationSentence } from "./words";

describe("operationSentence for transitions", () => {
  const base = { kind: OperationKind.TRANSITION, snapshotKind: SnapshotKind.UNSPECIFIED };
  it.each([
    [LifecycleState.PROPOSED, "proposed a change"],
    [LifecycleState.WITHDRAWN, "withdrew a proposal"],
    [LifecycleState.DECLINED, "declined a proposal"],
    [LifecycleState.SUPERSEDED, "superseded a change"],
    [LifecycleState.LANDED, "recorded a landing"],
  ])("%s", (transitionTo, sentence) => {
    expect(operationSentence({ ...base, transitionTo })).toBe(sentence);
  });

  it("falls back without a transition", () => {
    expect(operationSentence(base)).toBe("moved a change through its lifecycle");
  });
});
