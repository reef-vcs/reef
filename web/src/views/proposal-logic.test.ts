// The proposal view's pure parts (R3.31): the symbol list's nesting and
// order, a hunk's line numbers, the sentences, and the action machine.
import { create } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { DEVICE_READS } from "../device";
import { AutomaticStatusSchema, AutomaticState } from "../gen/reef/api/v1/changes_pb";
import {
  FileDiffSchema,
  HunkSchema,
  SymbolChangeKind,
  SymbolChangeSchema,
  SymbolCoverage,
} from "../gen/reef/api/v1/diff_pb";
import { LandChangeResponseSchema, LandingOutcome } from "../gen/reef/api/v1/landing_pb";
import {
  ChangeTransitionSchema,
  DependencyEvent,
  LifecycleState,
  TransitionCause,
} from "../gen/reef/api/v1/lifecycle_pb";
import { reverseHex } from "../ui/ids";
import {
  acceptBlocked,
  actionReducer,
  declineBlocked,
  landingWords,
  type ActionState,
} from "./proposal-actions";
import { diffFiles, nest } from "./proposal-diff";
import { estimateWords } from "./ProposalComments";
import { hunkLines } from "./ProposalDiff";
import { automaticWords, heldWords, needWords, transitionWords } from "./proposal-words";

const symbol = (name: string, enclosing = "", kind = SymbolChangeKind.CHANGED) =>
  create(SymbolChangeSchema, {
    symbol: `rust . . ${name}.`,
    name,
    enclosingSymbol: enclosing,
    kind,
  });

describe("the symbol list", () => {
  it("nests changes by their enclosing definition", () => {
    const outer = symbol("Lease");
    const inner = symbol("renew", outer.symbol);
    const other = symbol("expire", "rust . . Unchanged.");
    const tree = nest([inner, outer, other]);
    expect(tree.map((n) => n.change.name)).toEqual(["Lease", "expire"]);
    expect(tree[0]?.children.map((n) => n.change.name)).toEqual(["renew"]);
  });

  it("lists the most changed files first, and says why a file shows by lines", () => {
    const covered = (path: string, n: number) =>
      create(FileDiffSchema, {
        change: { path },
        symbols: {
          coverage: SymbolCoverage.COVERED,
          changes: Array.from({ length: n }, (_, i) => symbol(`${path}${String(i)}`)),
        },
      });
    const files = diffFiles([
      covered("a.rs", 1),
      create(FileDiffSchema, {
        change: { path: "README.md" },
        symbols: { coverage: SymbolCoverage.UNSUPPORTED },
      }),
      covered("b.rs", 3),
      create(FileDiffSchema, { change: { path: "logo.png" }, binary: true }),
    ]);
    expect(files.map((f) => [f.path, f.changed, f.byLines])).toEqual([
      ["b.rs", 3, ""],
      ["a.rs", 1, ""],
      ["logo.png", 0, "binary"],
      ["README.md", 0, "no symbol index for this language"],
    ]);
  });

  it("orders each page alone, so a later page never moves rows already shown", () => {
    const file = (path: string, n: number) =>
      create(FileDiffSchema, {
        change: { path },
        symbols: {
          coverage: SymbolCoverage.COVERED,
          changes: Array.from({ length: n }, (_, i) => symbol(`${path}${String(i)}`)),
        },
      });
    const pages = [[file("a.rs", 2), file("b.rs", 1)], [file("z.rs", 5)]];
    expect(pages.flatMap((page) => diffFiles(page)).map((f) => f.path)).toEqual([
      "a.rs",
      "b.rs",
      "z.rs",
    ]);
  });

  it("numbers a hunk's lines on each side", () => {
    const hunk = create(HunkSchema, {
      oldStart: 10,
      newStart: 10,
      lines: " keep\n-old\n+new\n+more\n keep\n",
    });
    expect(hunkLines(hunk).map((l) => [l.sign, l.old, l.new, l.text])).toEqual([
      [" ", 10, 10, "keep"],
      ["-", 11, undefined, "old"],
      ["+", undefined, 11, "new"],
      ["+", undefined, 12, "more"],
      [" ", 12, 13, "keep"],
    ]);
  });
});

describe("the sentences", () => {
  it("words each automatic state", () => {
    const words = (state: AutomaticState, init = {}) =>
      automaticWords(create(AutomaticStatusSchema, { state, ...init }));
    expect(words(AutomaticState.WAITING, { missing: [{ kind: "test" }], missingTotal: 1 })).toBe(
      "Waiting for test on this tree.",
    );
    expect(
      words(AutomaticState.NEEDS, {
        missing: [{ kind: "signoff" }, { kind: "test", target: "src/lib.rs" }],
        missingTotal: 3,
      }),
    ).toBe("Needs a sign-off and test of src/lib.rs, and 1 more.");
    expect(words(AutomaticState.READY)).toBe("Ready to land.");
    expect(words(AutomaticState.QUEUED)).toBe("Landing.");
    expect(automaticWords(undefined)).toBe("");
  });

  it("words a carried proposal and a decline from the web UI", () => {
    const below = "1".repeat(32);
    expect(
      transitionWords(
        create(ChangeTransitionSchema, {
          from: LifecycleState.PROPOSED,
          to: LifecycleState.PROPOSED,
          cause: TransitionCause.DEPENDENCY,
          dependency: { changeId: below, event: DependencyEvent.AMENDED },
        }),
      ),
    ).toBe(`Carried along: ${reverseHex(below).slice(0, 8)}, below it, was amended.`);
    expect(
      transitionWords(
        create(ChangeTransitionSchema, {
          to: LifecycleState.DECLINED,
          cause: TransitionCause.REQUEST,
          identity: "arsood",
          reason: "needs a test",
          web: { session: "0123456789abcdef" },
        }),
      ),
    ).toBe("Declined by arsood from the web UI: needs a test.");
  });

  it("says what holds a proposal back", () => {
    expect(
      heldWords([
        { changeId: "2".repeat(32), lifecycle: { state: LifecycleState.PROPOSED } },
      ] as never),
    ).toBe("");
    expect(
      heldWords([
        { changeId: "2".repeat(32), lifecycle: { state: LifecycleState.DECLINED } },
      ] as never),
    ).toMatch(/^Held: .+, below it, was declined\./);
  });

  it("words an instruction's estimate", () => {
    expect(
      estimateWords({
        checksKnown: true,
        checks: 2,
        checkWallMs: 40_000n,
        agentKnown: false,
        unknown: ["Model tokens are not visible to Reef."],
      } as never),
    ).toEqual(["Its checks: 2 jobs, 40 s of wall time.", "Model tokens are not visible to Reef."]);
  });

  it("words what a landing request answered", () => {
    const answer = (outcome: LandingOutcome, message = "") =>
      landingWords(create(LandChangeResponseSchema, { landing: { outcome, message } }));
    expect(answer(LandingOutcome.QUEUED)).toBe("Signed off and queued to land.");
    expect(answer(LandingOutcome.BLOCKED, "needs test")).toBe(
      "Signed off, but the gate blocked the landing: needs test",
    );
    expect(
      landingWords(
        create(LandChangeResponseSchema, {
          waiting: { state: AutomaticState.WAITING, missing: [{ kind: "test" }], missingTotal: 1 },
        }),
      ),
    ).toBe("Signed off. It lands once its checks are in: Waiting for test on this tree.");
  });
});

describe("the actions", () => {
  it("runs one action at a time, and each ends once", () => {
    let state: ActionState = { kind: "idle" };
    state = actionReducer(state, { type: "start", action: "accept", step: "requesting" });
    expect(actionReducer(state, { type: "start", action: "decline", step: "writing" })).toBe(state);
    expect(actionReducer(state, { type: "reset" })).toBe(state);
    state = actionReducer(state, { type: "step", step: "touching" });
    expect(state).toEqual({ kind: "working", action: "accept", step: "touching" });
    state = actionReducer(state, { type: "done", words: "Signed off." });
    expect(state).toEqual({ kind: "done", action: "accept", words: "Signed off." });
    expect(actionReducer(state, { type: "failed", failure: { kind: "browser", words: "" } })).toBe(
      state,
    );
  });

  it("says why Accept and Decline are off", () => {
    const facts = {
      state: LifecycleState.PROPOSED,
      amended: false,
      unproposed: false,
      tree: "4b825dc6",
      signer: { name: "arsood", passkeys: 1 },
      supported: true,
      device: false,
    };
    expect(acceptBlocked(facts)).toBeNull();
    expect(acceptBlocked({ ...facts, state: LifecycleState.LANDED })).toBe(
      "Only a proposed change can be accepted; this one is landed.",
    );
    expect(acceptBlocked({ ...facts, amended: true })).toMatch(/^The agent changed this proposal/);
    expect(acceptBlocked({ ...facts, unproposed: true })).toMatch(/was never proposed/);
    expect(acceptBlocked({ ...facts, tree: "" })).toBe(
      "The change has no visible commit to sign off.",
    );
    expect(acceptBlocked({ ...facts, signer: { name: "arsood", passkeys: 0 } })).toBe(
      "arsood has no passkey.",
    );
    expect(declineBlocked(LifecycleState.QUEUED, false, false)).toBeNull();
    expect(declineBlocked(LifecycleState.DECLINED, false, false)).toBe(
      "Only a change in review can be declined.",
    );
    // A paired phone (R3.40) decides nothing.
    expect(acceptBlocked({ ...facts, device: true })).toBe(DEVICE_READS);
    expect(declineBlocked(LifecycleState.PROPOSED, false, true)).toBe(DEVICE_READS);
  });
});

describe("a need in words", () => {
  it("names a sign-off's rule and a blocking rule (R3.33)", () => {
    expect(needWords({ kind: "signoff", target: "review" })).toBe("a sign-off for review");
    expect(needWords({ kind: "signoff", target: "" })).toBe("a sign-off");
    expect(needWords({ kind: "rule", target: "no-vendor" })).toBe("no-vendor to pass");
    expect(needWords({ kind: "test", target: "src/lib.rs" })).toBe("test of src/lib.rs");
  });
});
