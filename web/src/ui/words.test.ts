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

// R3.12: a comment operation's sentence names its first event.
describe("operationSentence for comments", () => {
  const base = { kind: OperationKind.COMMENT, snapshotKind: SnapshotKind.UNSPECIFIED };
  it.each([
    ["comment", "commented on a change"],
    ["instruct", "wrote an instruction"],
    ["confirm", "confirmed an instruction"],
    ["claim", "took an instruction"],
    ["finish", "finished an instruction"],
    ["cancel", "cancelled an instruction"],
  ])("%s", (event, sentence) => {
    expect(operationSentence({ ...base, comments: [{ event }] })).toBe(sentence);
  });

  it("falls back without an event", () => {
    expect(operationSentence(base)).toBe("commented on a change");
  });
});

// #258: a publish names its remote and branch when its effects say.
describe("operationSentence for a publish", () => {
  const base = { kind: OperationKind.PUBLISH, snapshotKind: SnapshotKind.UNSPECIFIED };
  it("names the remote branch", () => {
    const effects = {
      bookmarks: [
        { name: "reef/kxyzwqrsmtlo", remote: "" },
        { name: "reef/kxyzwqrsmtlo", remote: "origin" },
      ],
    };
    expect(operationSentence({ ...base, effects })).toBe(
      "published a change to origin/reef/kxyzwqrsmtlo",
    );
  });
  it("falls back without effects", () => {
    expect(operationSentence(base)).toBe("published a change");
  });
});
