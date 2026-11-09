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

  // R3.13: the landing service's dequeue and land.
  it("names a dequeue", () => {
    const transitions = [
      { from: LifecycleState.QUEUED, to: LifecycleState.PROPOSED },
    ] as unknown as NonNullable<Parameters<typeof operationSentence>[0]["transitions"]>;
    expect(operationSentence({ ...base, transitions })).toBe("returned a proposal from the queue");
  });

  it("names a landing", () => {
    expect(
      operationSentence({ kind: OperationKind.LAND, snapshotKind: SnapshotKind.UNSPECIFIED }),
    ).toBe("landed a change");
  });

  it("names a batch's landing and its trials", () => {
    const transitions = [
      { from: LifecycleState.QUEUED, to: LifecycleState.LANDED },
      { from: LifecycleState.QUEUED, to: LifecycleState.LANDED },
      { from: LifecycleState.QUEUED, to: LifecycleState.LANDED },
    ] as unknown as NonNullable<Parameters<typeof operationSentence>[0]["transitions"]>;
    expect(
      operationSentence({
        kind: OperationKind.LAND,
        snapshotKind: SnapshotKind.UNSPECIFIED,
        transitions,
      }),
    ).toBe("landed 3 changes");
    expect(
      operationSentence({ kind: OperationKind.BATCH, snapshotKind: SnapshotKind.UNSPECIFIED }),
    ).toBe("tested a batch of changes");
  });
});
