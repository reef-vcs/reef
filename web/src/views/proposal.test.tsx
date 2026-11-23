// The proposal page (R3.31) over an in-memory daemon
// and a fake authenticator: the reads in the page's order, a definition
// expanded to its lines, Accept (one touch, then a landing request for the
// reviewed commit), Decline (a reason, at the commit shown), a note and an
// instruction confirmed, an amended proposal holding its actions, and the
// tab's list held in place.
import { create } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  AutomaticState,
  AutomaticStatusSchema,
  ProposalDependencySchema,
} from "../gen/reef/api/v1/changes_pb";
import { CommentKind, HelpState, InstructionState } from "../gen/reef/api/v1/comment_event_pb";
import { CommentSchema } from "../gen/reef/api/v1/comments_pb";
import { reverseHex } from "../ui/ids";
import {
  GetDiffResponseSchema,
  SymbolChangeKind,
  SymbolCoverage,
} from "../gen/reef/api/v1/diff_pb";
import { LifecycleState, TransitionActor } from "../gen/reef/api/v1/lifecycle_pb";
import {
  PolicyDecision,
  PolicyRuleKind,
  PolicyRuleResultSchema,
  PolicyRuleStatus,
} from "../gen/reef/api/v1/policy_pb";
import { PresenceKind } from "../gen/reef/api/v1/presence_pb";
import { OperationStore } from "../live/operations";
import { PresenceStore } from "../live/presence";
import type { Live } from "../live/share";
import {
  daemonError,
  immediateFrames,
  operation,
  operationsFrame,
  presenceFrame,
  settle,
} from "../test/fakes";
import { DEVICE_READS } from "../device";
import { passkey } from "../test/identity-daemon";
import { paired } from "../test/paired";
import { CHANGE, COMMIT, proposal, ProposalDaemon, REPO, TREE } from "../test/proposal-daemon";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "../test/testing-library";
import { installAuthenticator, removeAuthenticator } from "../test/webauthn";
import { Proposal } from "./Proposal";
import { MOVED_AFTER_SIGNING } from "./proposal-actions";
import { POLL_MS } from "./proposal-model";

afterEach(() => {
  cleanup();
  removeAuthenticator();
});

async function flush() {
  await act(async () => {
    await settle(40);
  });
}

function live() {
  const operations = new OperationStore(immediateFrames);
  operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const presence = new PresenceStore(immediateFrames);
  presence.applyFrame(presenceFrame([], true));
  let cursor = 1;
  let reported = 1;
  return {
    live: { operations, presence } as unknown as Live,
    /** The daemon replaces the proposal's AUTOMATIC entry (R3.33). */
    judged: () => {
      presence.applyFrame(
        presenceFrame([
          {
            id: "auto1",
            kind: PresenceKind.AUTOMATIC,
            repoId: REPO,
            changeId: CHANGE,
            updatedAt: timestampFromMs(1_795_000_000_000 + reported++),
          },
        ]),
      );
    },
    push: () => {
      operations.applyFrame(
        operationsFrame({
          operations: [operation(`op${String(cursor)}`, REPO, 1_795_000_000_000 + cursor)],
          live: true,
          cursor: `c${String(cursor++)}`,
        }),
      );
    },
  };
}

function daemonWithDiff(): ProposalDaemon {
  const daemon = new ProposalDaemon();
  daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
  const entry = daemon.entries[0];
  if (entry) {
    entry.automatic = create(AutomaticStatusSchema, {
      state: AutomaticState.NEEDS,
      missing: [{ kind: "signoff" }],
      missingTotal: 1,
      reason: "policy",
    });
  }
  daemon.evaluation.decision = PolicyDecision.BLOCK;
  daemon.evaluation.rules = [
    create(PolicyRuleResultSchema, {
      name: "verified",
      kind: PolicyRuleKind.SIGNOFF,
      status: PolicyRuleStatus.UNSATISFIED,
      signoff: { identities: ["arsood"], count: 1, have: 0, presence: "user_verified" },
    }),
  ];
  daemon.diff = create(GetDiffResponseSchema, {
    totalFiles: 2,
    symbolSummary: { changed: 2, filesCovered: 1, filesUnsupported: 1 },
    files: [
      {
        change: { path: "README.md" },
        linesAdded: 1,
        symbols: { coverage: SymbolCoverage.UNSUPPORTED },
      },
      {
        change: { path: "src/lease.rs" },
        linesAdded: 4,
        linesRemoved: 1,
        symbols: {
          coverage: SymbolCoverage.COVERED,
          changes: [
            {
              kind: SymbolChangeKind.CHANGED,
              symbol: "rust . . Lease#",
              name: "Lease",
              nestedOnly: true,
            },
            {
              kind: SymbolChangeKind.CHANGED,
              symbol: "rust . . Lease#renew().",
              name: "renew",
              enclosingSymbol: "rust . . Lease#",
              linesAdded: 4,
              linesRemoved: 1,
            },
          ],
        },
      },
    ],
  });
  daemon.expand = (request) =>
    create(GetDiffResponseSchema, {
      files: [
        {
          change: { path: request.paths[0] ?? "" },
          hunks: [
            {
              oldStart: 7,
              oldLines: 1,
              newStart: 7,
              newLines: 2,
              lines: "-old\n+renewed\n+again\n",
            },
          ],
          symbols: {
            coverage: SymbolCoverage.COVERED,
            changes: [
              {
                symbol: "rust . . Lease#renew().",
                callSiteCount: 1,
                callSites: [{ path: "src/main.rs", range: { startLine: 3 } }],
              },
            ],
          },
        },
      ],
    });
  return daemon;
}

async function open(daemon: ProposalDaemon) {
  const stream = live();
  render(
    <Proposal
      repo={REPO}
      change={CHANGE}
      connection={daemon.connection()}
      live={stream.live}
      frame={immediateFrames}
      onSessionInvalid={vi.fn()}
    />,
  );
  await flush();
  return stream;
}

const button = (name: string | RegExp) => screen.getByRole<HTMLButtonElement>("button", { name });

describe("the proposal page", () => {
  it("reads intent, then evidence, then the symbol diff, and expands a definition to its lines", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    await open(daemon);
    expect(screen.getByRole("heading", { level: 2 }).textContent).toContain(
      "Fix the lease renewal race",
    );
    expect(screen.getByText("Needs a sign-off.")).toBeTruthy();
    const headings = screen.getAllByRole("heading", { level: 3 }).map((h) => h.textContent);
    expect(headings).toEqual(["Intent", "Evidence", "What changed", "Comments"]);
    expect(screen.getByText("Renew before the grace ends.")).toBeTruthy();
    expect(screen.getByText("The policy blocks it.")).toBeTruthy();
    expect(screen.getByText(/sign-offs 0 of 1, user verified/)).toBeTruthy();

    // The covered file first, its method nested under its class.
    const files = screen.getAllByRole("region").filter((r) => r.classList.contains("diff-file"));
    expect(files.map((f) => f.getAttribute("aria-label"))).toEqual(["src/lease.rs", "README.md"]);
    expect(within(files[1] ?? document.body).getByText(/By lines: no symbol index/)).toBeTruthy();
    expect(daemon.diffs[0]).toMatchObject({ symbols: true, maxCallSites: 0, toRevision: COMMIT });

    await act(() => {
      within(files[0] ?? document.body)
        .getAllByRole("button", { name: "Show lines" })[0]
        ?.click();
    });
    await flush();
    expect(daemon.diffs[1]).toMatchObject({
      paths: ["src/lease.rs"],
      expandSymbols: ["rust . . Lease#renew()."],
    });
    expect(screen.getByText("renewed")).toBeTruthy();
    expect(screen.getByText("src/main.rs:3")).toBeTruthy();
  });

  it("accepts with one touch on the reviewed tree, then asks to land that commit", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    await open(daemon);
    expect(button("Accept").disabled).toBe(false);
    await act(() => {
      button("Accept").click();
    });
    await waitFor(() => {
      expect(screen.queryByText("Signed off and queued to land.")).not.toBeNull();
    });
    expect(daemon.requested[0]?.revision).toBe(COMMIT);
    const statement = JSON.parse(new TextDecoder().decode(daemon.requested[0]?.statement)) as {
      subject: { digest: { gitTree: string } }[];
    };
    expect(statement.subject[0]?.digest.gitTree).toBe(TREE);
    expect(daemon.added).toHaveLength(1);
    expect(daemon.landed[0]).toMatchObject({
      revision: reverseHex(CHANGE),
      expectedCommitId: COMMIT,
      actor: TransitionActor.HUMAN,
    });
  });

  it("says so when the agent amends between the sign-off and the landing", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    daemon.failures.landChange = daemonError(Code.FailedPrecondition, {
      reason: "EXPECTED_COMMIT_MISMATCH",
    });
    await open(daemon);
    await act(() => {
      button("Accept").click();
    });
    await waitFor(() => {
      expect(screen.queryByText(MOVED_AFTER_SIGNING)).not.toBeNull();
    });
    expect(daemon.added).toHaveLength(1);
  });

  it.each([
    [
      "pin",
      "no_pins",
      /Signed off, but main pins no passkey, so the web UI cannot land on it\. .*reef identity passkey list.*from a terminal, then Accept again; or land it from a terminal: reef land /,
      true,
    ],
    [
      "pinned_touch",
      "",
      /Signed off, but main does not pin the passkey you touched.*reef identity passkey list.*reef land /,
      true,
    ],
    [
      "pinned_origin",
      "",
      /Signed off, but your touch ran at this address, and main's reef-policy\.toml pins that passkey at another.*reef identity passkey list.*reef land /,
      true,
    ],
    [
      "pin",
      "no_policy",
      /Signed off, but main has no reef-policy\.toml, so nothing lands on it, from here or from a terminal\. Commit a first one to main by hand \(reef policy init writes a starter\) with the \[passkeys\] line that reef identity passkey list prints, then Accept again\./,
      false,
    ],
    [
      "pin",
      "invalid_policy",
      /Signed off, but main's reef-policy\.toml does not load, so nothing lands on it, from here or from a terminal\. Commit a fix to main by hand \(reef policy check says what is wrong\) with the \[passkeys\] line that reef identity passkey list prints, then Accept again\./,
      false,
    ],
  ])(
    "says what to pin when the landing refuses for want of %s (%s)",
    async (missing, policy, words, landsFromATerminal) => {
      installAuthenticator();
      const daemon = daemonWithDiff();
      daemon.failures.landChange = daemonError(Code.FailedPrecondition, {
        reason: "VERIFIED_SIGNOFF_REQUIRED",
        metadata: { missing, policy, destination: "main", commit_id: COMMIT },
      });
      await open(daemon);
      await act(() => {
        button("Accept").click();
      });
      await waitFor(() => {
        expect(screen.queryByText(words)).not.toBeNull();
      });
      const terminal = screen.queryByText(new RegExp(`reef land ${COMMIT.slice(0, 12)}\\.`));
      expect(terminal !== null).toBe(landsFromATerminal);
      expect(screen.queryByText(/reef land /) !== null).toBe(landsFromATerminal);
      expect(daemon.added).toHaveLength(1);
    },
  );

  it("explains Accept without a passkey, and still declines and comments", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    daemon.passkeys = [];
    await open(daemon);
    expect(button("Accept").disabled).toBe(true);
    expect(screen.getByText(/arsood has no passkey\./)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Add one" }).getAttribute("href")).toBe("/passkeys");
    expect(button("Decline").disabled).toBe(false);
  });

  it("on a paired phone, offers no write and says why under the buttons", async () => {
    paired();
    installAuthenticator();
    const daemon = daemonWithDiff();
    daemon.comments = [
      create(CommentSchema, {
        id: "cmt_00000000000000h1",
        changeId: CHANGE,
        kind: CommentKind.HELP_REQUEST,
        help: { state: HelpState.OPEN, question: "Keep the old parser?" },
      }),
    ];
    await open(daemon);
    expect(screen.getByText("Keep the old parser?")).toBeTruthy();
    for (const name of ["Accept", "Decline"]) {
      expect(button(name).disabled).toBe(true);
      const why = button(name).getAttribute("aria-describedby") ?? "";
      expect(document.getElementById(why)?.textContent).toBe(DEVICE_READS);
    }
    expect(screen.queryByRole("link", { name: "Comment" })).toBeNull();
    expect(screen.queryByRole("textbox")).toBeNull();
    await act(() => {
      screen.getAllByRole("button", { name: /^Show / })[0]?.click();
    });
    await flush();
    expect(screen.queryByRole("button", { name: /^Comment on .* line/ })).toBeNull();
  });

  it("declines with a reason, at the commit and state the page showed", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    await open(daemon);
    await act(() => {
      button("Decline").click();
    });
    const submit = screen.getAllByRole<HTMLButtonElement>("button", { name: "Decline" })[1];
    expect(submit?.disabled).toBe(true);
    fireEvent.input(screen.getByLabelText(/Why/), { target: { value: "No test for the race." } });
    await act(() => {
      submit?.click();
    });
    await flush();
    expect(daemon.declined[0]).toMatchObject({
      revision: reverseHex(CHANGE),
      to: LifecycleState.DECLINED,
      reason: "No test for the race.",
      expectedCommitId: COMMIT,
      expectedState: LifecycleState.PROPOSED,
      stack: false,
    });
    expect(screen.getByRole("status").textContent).toBe("Declined.");
  });

  it("writes a note, and an instruction it confirms after showing the estimate", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    await open(daemon);
    const box = screen.getByRole("textbox", { name: "Comment" });
    fireEvent.input(box, { target: { value: "Looks right." } });
    await act(() => {
      button("Comment").click();
    });
    await flush();
    expect(daemon.written[0]).toMatchObject({ kind: CommentKind.NOTE, text: "Looks right." });
    expect(await screen.findByText("Looks right.")).toBeTruthy();

    fireEvent.input(screen.getByRole("textbox", { name: "Comment" }), {
      target: { value: "Add a test for the race." },
    });
    await act(() => {
      screen.getByRole("checkbox", { name: /Send as an instruction/ }).click();
    });
    await act(() => {
      button("Send the instruction").click();
    });
    await flush();
    expect(daemon.written[1]).toMatchObject({
      kind: CommentKind.INSTRUCTION,
      expectedCommitId: COMMIT,
    });
    const confirm = await screen.findByRole("group", { name: "Confirm the instruction" });
    expect(within(confirm).getByText("Its checks: 2 jobs, 40 s of wall time.")).toBeTruthy();
    await act(() => {
      within(confirm).getByRole("button", { name: "Confirm" }).click();
    });
    await flush();
    expect(daemon.moved[0]).toMatchObject({
      to: InstructionState.PENDING,
      expectedState: InstructionState.AWAITING_CONFIRMATION,
    });
  });

  it("answers a help request, naming the change and the commit shown", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    daemon.comments = [
      create(CommentSchema, {
        id: "cmt_00000000000000h1",
        changeId: CHANGE,
        kind: CommentKind.HELP_REQUEST,
        author: { agent: "claude@mac" },
        help: { state: HelpState.OPEN, question: "Keep the old parser?" },
      }),
    ];
    await open(daemon);
    expect(screen.getByText("Keep the old parser?")).toBeTruthy();
    fireEvent.input(screen.getByRole("textbox", { name: "Your answer" }), {
      target: { value: "Yes, keep it." },
    });
    await act(() => {
      button("Answer").click();
    });
    await flush();
    expect(daemon.written[0]).toMatchObject({
      kind: CommentKind.INSTRUCTION,
      replyTo: "cmt_00000000000000h1",
      text: "Yes, keep it.",
      revision: reverseHex(CHANGE),
      expectedCommitId: COMMIT,
    });
    expect(screen.getByRole("status").textContent).toBe(
      "Answer sent: the agent gets it as its next instruction.",
    );
  });

  it("says a late reply waits for its Confirm when someone answered first", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    const question = create(CommentSchema, {
      id: "cmt_00000000000000h1",
      changeId: CHANGE,
      kind: CommentKind.HELP_REQUEST,
      author: { agent: "claude@mac" },
      help: { state: HelpState.OPEN, question: "Keep the old parser?" },
    });
    daemon.comments = [question];
    await open(daemon);
    // Answered elsewhere after the page read it.
    if (question.help) question.help.state = HelpState.ANSWERED;
    fireEvent.input(screen.getByRole("textbox", { name: "Your answer" }), {
      target: { value: "Yes, keep it." },
    });
    await act(() => {
      button("Answer").click();
    });
    await flush();
    expect(screen.getByRole("status").textContent).toBe(
      "This question was already answered: your reply waits for your Confirm below.",
    );
  });

  it("holds Accept and Decline when the agent amends the proposal, until the person moves on", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    const { push } = await open(daemon);
    const amended = "a".repeat(40);
    daemon.entries = [proposal(CHANGE, amended, "Fix the lease renewal race")];
    await act(() => {
      push();
    });
    // A refresh is spaced after the last read (live/paced.ts).
    const notice = await screen.findByRole("status");
    expect(notice.textContent).toMatch(/^The agent changed this proposal since you opened it\./);
    expect(button("Accept").disabled).toBe(true);
    expect(button("Decline").disabled).toBe(true);
    await act(() => {
      button("Review the new version").click();
    });
    await flush();
    expect(button("Accept").disabled).toBe(false);
    expect(daemon.diffs.at(-1)?.toRevision).toBe(amended);
  });

  it("does not accept a change moved since it was proposed, and says so", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    const entry = daemon.entries[0];
    if (!entry?.lifecycle) throw new Error("no entry");
    entry.lifecycle.amended = true;
    await open(daemon);
    expect(button("Accept").disabled).toBe(true);
    expect(screen.getAllByText(/what is shown was never proposed/).length).toBeGreaterThan(0);
    expect(button("Decline").disabled).toBe(false);
  });

  it("reads again when its status entry moves: evidence records no operation", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    const stream = await open(daemon);
    expect(screen.getByText("The policy blocks it.")).toBeTruthy();
    daemon.evaluation.decision = PolicyDecision.ALLOW;
    daemon.evaluation.rules = [];
    await act(() => {
      stream.judged();
    });
    // Paced: the read follows the last one by 250 ms at least.
    await waitFor(() => {
      expect(screen.getByText("The policy allows it to land.")).toBeTruthy();
    });
  });

  it("polls while in review only where no policy judges it", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      installAuthenticator();
      const daemon = daemonWithDiff();
      const entry = daemon.entries[0];
      if (entry) {
        entry.automatic = create(AutomaticStatusSchema, { state: AutomaticState.OFF });
      }
      await open(daemon);
      expect(screen.getByText("The policy blocks it.")).toBeTruthy();
      daemon.evaluation.decision = PolicyDecision.ALLOW;
      daemon.evaluation.rules = [];
      await act(async () => {
        await vi.advanceTimersByTimeAsync(POLL_MS + 1_000);
      });
      await flush();
      expect(screen.getByText("The policy allows it to land.")).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("shows the changes below it and what holds it", async () => {
    installAuthenticator();
    const daemon = daemonWithDiff();
    const below = "b".repeat(32);
    const head = daemon.entries[0];
    if (!head) throw new Error("no entry");
    head.dependencies = [
      create(ProposalDependencySchema, {
        changeId: below,
        proposedBy: "claude@mac",
        lifecycle: { state: LifecycleState.DECLINED, recorded: true },
      }),
    ];
    daemon.entries.push(
      proposal(below, "b".repeat(40), "Add the lease clock", {
        lifecycle: {
          state: LifecycleState.DECLINED,
          recorded: true,
          last: { reason: "wrong clock" },
        },
      }),
    );
    await open(daemon);
    expect(screen.getByText(/^Held: .+, below it, was declined \(“wrong clock”\)\./)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Add the lease clock" }).getAttribute("href")).toBe(
      `/repos/${REPO}/proposals/${below}`,
    );
    expect(button(/^Accept with the 1 change below$/)).toBeTruthy();
  });
});
