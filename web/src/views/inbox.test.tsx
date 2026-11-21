// The triage inbox (R3.33, #372) over an in-memory daemon's stream: buckets
// held in place behind a count, a repository the stream adds later, a tab
// that shows only its repository, the Answer box, and a digest marked seen
// once on screen that keeps what it showed for the visit.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import { create } from "@bufbuild/protobuf";
import { InboxReason, InboxSectionSchema } from "../gen/reef/api/v1/inbox_pb";
import { InboxStore } from "../live/inbox";
import type { Live } from "../live/share";
import { daemonError, immediateFrames, settle } from "../test/fakes";
import { helpItem, InboxDaemon, OTHER, proposalItem, REPO, section } from "../test/inbox-daemon";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "../test/testing-library";
import { Inbox, SEEN_AFTER_MS } from "./Inbox";
import { InboxModel } from "./inbox-model";

afterEach(() => {
  cleanup();
});

/** The browser's inbox stream, fed by `daemon`: `send` is its next frame. */
function live(daemon: InboxDaemon) {
  const inbox = new InboxStore(immediateFrames);
  inbox.beginResync();
  inbox.applyFrame(daemon.frame());
  return {
    live: { inbox } as unknown as Live,
    send: () => {
      inbox.applyFrame(daemon.frame());
    },
  };
}

async function open(daemon: InboxDaemon, repo?: string) {
  const stream = live(daemon);
  render(
    <Inbox
      repo={repo}
      repoNames={new Map([[REPO, "reef"]])}
      connection={daemon.connection()}
      live={stream.live}
      frame={immediateFrames}
      onSessionInvalid={vi.fn()}
    />,
  );
  await act(async () => {
    await settle(20);
  });
  return stream;
}

const needs = () => screen.getByRole("region", { name: "Needs you now" });
const rows = (region: HTMLElement) =>
  within(region)
    .queryAllByRole("listitem")
    .map((li) => li.getAttribute("data-key"));

describe("the inbox", () => {
  it("holds needs-you-now in place while new items wait behind a count", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    const stream = await open(daemon);
    expect(rows(needs())).toEqual([`help:${REPO}:h1`]);
    expect(document.title).toBe("(1) Inbox — Reef");
    daemon.sections = [
      section(REPO, {
        needsYouNow: [
          helpItem("h2", 3_000),
          proposalItem("d".repeat(32), InboxReason.SIGNOFF, 2_500, "Needs a sign-off from release"),
          helpItem("h1", 2_000),
        ],
        needsYouNowTotal: 3,
      }),
    ];
    await act(() => {
      stream.send();
    });
    const show = await screen.findByRole("button", { name: "Show 2 new items" });
    expect(rows(needs())).toEqual([`help:${REPO}:h1`]);
    expect(document.title).toBe("(3) Inbox — Reef");
    await act(() => {
      show.click();
    });
    expect(rows(needs())).toEqual([
      `help:${REPO}:h2`,
      `proposal:${REPO}:${"d".repeat(32)}`,
      `help:${REPO}:h1`,
    ]);
    expect(screen.getByRole("link", { name: "Change dddd" })).toBeTruthy();
    expect(screen.getByText("Needs a sign-off from release")).toBeTruthy();
  });

  it("on the home page, shows a repository the stream adds after it opened", async () => {
    const daemon = new InboxDaemon();
    const stream = await open(daemon);
    daemon.sections = [
      section(REPO),
      section(OTHER, { needsYouNow: [helpItem("h1", 1_000, OTHER)], needsYouNowTotal: 1 }),
    ];
    await act(() => {
      stream.send();
    });
    const show = await screen.findByRole("button", { name: "Show 1 new item" });
    await act(() => {
      show.click();
    });
    expect(rows(needs())).toEqual([`help:${OTHER}:h1`]);
  });

  it("on a repository's tab, shows only that repository", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
      section(OTHER, { needsYouNow: [helpItem("h2", 1_000, OTHER)], needsYouNowTotal: 1 }),
    ];
    await open(daemon, REPO);
    await waitFor(() => {
      expect(rows(needs())).toEqual([`help:${REPO}:h1`]);
    });
    expect(document.title).toBe("(1) Inbox — Reef");
  });

  it("answers a help request in place, and says so when it leaves", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    const stream = await open(daemon);
    expect(screen.getByText("TOML or YAML?")).toBeTruthy();
    expect(screen.getByText("Tried: read the docs")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Answer" }));
    fireEvent.change(screen.getByLabelText("Your answer"), { target: { value: "TOML" } });
    fireEvent.click(screen.getByRole("button", { name: "Answer" }));
    await waitFor(() => {
      expect(daemon.answers.length).toBe(1);
    });
    expect(daemon.answers[0]?.replyTo).toBe("h1");
    expect(daemon.answers[0]?.text).toBe("TOML");
    daemon.sections = [section(REPO)];
    await act(() => {
      stream.send();
    });
    await waitFor(() => {
      expect(screen.getByText(/Answered/)).toBeTruthy();
    });
  });

  it("marks a digest seen after it was on screen, and keeps what it showed for the visit", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const tick = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
      });
    };
    try {
      const daemon = new InboxDaemon();
      daemon.sections = [
        section(REPO, {
          landed: {
            count: 2n,
            fromSeq: 4n,
            throughSeq: 9n,
            newest: [{ changeId: "e".repeat(32), subject: "Land the parser", seq: 9n }],
            byAgent: [{ agent: "claude@a", count: 2n }],
          },
        }),
      ];
      const stream = live(daemon);
      render(
        <Inbox
          connection={daemon.connection()}
          live={stream.live}
          frame={immediateFrames}
          onSessionInvalid={vi.fn()}
        />,
      );
      await tick(100);
      expect(screen.getByText("Land the parser")).toBeTruthy();
      await tick(SEEN_AFTER_MS - 500);
      expect(daemon.marks).toEqual([]);
      await tick(1_000);
      expect(daemon.marks.map((m) => [m.repo, m.seq])).toEqual([[REPO, 9n]]);
      // The stream's digest now starts at the mark: what the page showed stays.
      daemon.sections = [
        section(REPO, { landed: { count: 0n, fromSeq: 9n, throughSeq: 9n, fromMark: true } }),
      ];
      await act(() => {
        stream.send();
      });
      await tick(100);
      expect(screen.getByText("Land the parser")).toBeTruthy();
      await tick(SEEN_AFTER_MS + 500);
      expect(daemon.marks.length).toBe(1);
      // A landing after the mark adds to it, and is marked in its turn.
      daemon.sections = [
        section(REPO, {
          landed: {
            count: 1n,
            fromSeq: 9n,
            throughSeq: 11n,
            fromMark: true,
            newest: [{ changeId: "f".repeat(32), subject: "Land the lexer", seq: 11n }],
          },
        }),
      ];
      await act(() => {
        stream.send();
      });
      await tick(100);
      expect(screen.getByText("Land the parser")).toBeTruthy();
      expect(screen.getByText("Land the lexer")).toBeTruthy();
      await tick(SEEN_AFTER_MS + 500);
      expect(daemon.marks.map((m) => m.seq)).toEqual([9n, 11n]);
    } finally {
      vi.useRealTimers();
    }
  });

  it("does not bank a mark armed before a landing it never showed", async () => {
    const landed = (through: bigint, newest: bigint[]) => ({
      count: BigInt(newest.length),
      fromSeq: 4n,
      throughSeq: through,
      newest: newest.map((seq) => ({ changeId: String(seq).repeat(32), seq })),
    });
    const daemon = new InboxDaemon();
    daemon.sections = [section(REPO, { landed: landed(5n, [5n]) })];
    const stream = live(daemon);
    const model = new InboxModel(
      daemon.connection(),
      stream.live.inbox,
      immediateFrames,
      vi.fn(),
      undefined,
    );
    const stop = model.start();
    // Seq 6 lands after the render that armed the mark through 5.
    daemon.sections = [section(REPO, { landed: landed(6n, [6n, 5n]) })];
    stream.send();
    await model.markSeen(REPO, 5n);
    expect(daemon.marks, "the next render marks 6 instead").toEqual([]);
    await model.markSeen(REPO, 6n);
    expect(daemon.marks.map((m) => m.seq)).toEqual([6n]);
    daemon.sections = [
      section(REPO, { landed: { count: 0n, fromSeq: 6n, throughSeq: 6n, fromMark: true } }),
    ];
    stream.send();
    const digest = model.getSnapshot().digests[0]?.digest;
    expect(digest?.count, "each landing once").toBe(2n);
    expect(digest?.newest.map((l) => l.seq)).toEqual([6n, 5n]);
    stop();
  });

  it("keeps a failed repository's rows as they were, and says it could not read it", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    const stream = await open(daemon);
    // The daemon keeps a failing repository's items as last read.
    daemon.sections = [
      create(InboxSectionSchema, {
        repoId: REPO,
        needsYouNow: [helpItem("h1", 2_000)],
        needsYouNowTotal: 1,
        error: { reason: "REPOSITORY_NOT_FOUND", message: "the repository is gone" },
      }),
    ];
    await act(() => {
      stream.send();
    });
    await screen.findByText(/the repository is gone/);
    expect(rows(needs())).toEqual([`help:${REPO}:h1`]);
    expect(within(needs()).queryByText(/No longer waiting/)).toBeNull();
    expect(document.title).toBe("(1) Inbox — Reef");
  });

  it("renders an agent's words as text, never markup (S-262)", async () => {
    const daemon = new InboxDaemon();
    const item = helpItem("h1", 2_000);
    if (item.record.case === "comment" && item.record.value.help) {
      item.record.value.help.question = '<img src=x onerror="alert(1)">';
    }
    daemon.sections = [section(REPO, { needsYouNow: [item], needsYouNowTotal: 1 })];
    await open(daemon);
    expect(screen.getByText('<img src=x onerror="alert(1)">')).toBeTruthy();
    expect(document.querySelector("img")).toBeNull();
  });

  it("keeps a digest's first words for the visit, and starts again from a mark set elsewhere", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const tick = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
      });
    };
    try {
      const daemon = new InboxDaemon();
      const landed = (from: bigint, through: bigint, fromMark: boolean, ms: number) => ({
        count: through > from ? 1n : 0n,
        fromSeq: from,
        throughSeq: through,
        fromMark,
        since: timestampFromMs(ms),
        newest:
          through > from
            ? [{ changeId: "e".repeat(32), subject: `Landed at ${String(through)}`, seq: through }]
            : [],
      });
      daemon.sections = [section(REPO, { landed: landed(4n, 5n, false, 1_000) })];
      const stream = live(daemon);
      render(
        <Inbox
          connection={daemon.connection()}
          live={stream.live}
          frame={immediateFrames}
          onSessionInvalid={vi.fn()}
        />,
      );
      await tick(SEEN_AFTER_MS + 500);
      expect(daemon.marks.map((m) => m.seq)).toEqual([5n]);
      daemon.sections = [section(REPO, { landed: landed(5n, 5n, true, 9_000_000_000_000) })];
      await act(() => {
        stream.send();
      });
      await tick(100);
      expect(screen.getByText(/in the last day/)).toBeTruthy();
      expect(screen.queryByText(/since you last looked,/)).toBeNull();
      // Another device marked past this page's mark: the person looked there.
      daemon.sections = [section(REPO, { landed: landed(20n, 21n, true, 9_000_000_000_000) })];
      await act(() => {
        stream.send();
      });
      await tick(100);
      expect(screen.queryByText("Landed at 5")).toBeNull();
      expect(screen.getByText("Landed at 21")).toBeTruthy();
    } finally {
      vi.useRealTimers();
    }
  });

  it("marks nothing while the digest is out of view or the tab is hidden", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const observers: ((entries: { isIntersecting: boolean }[]) => void)[] = [];
    vi.stubGlobal(
      "IntersectionObserver",
      class {
        constructor(callback: (entries: { isIntersecting: boolean }[]) => void) {
          observers.push(callback);
        }
        observe() {
          /* the test says when it intersects */
        }
        disconnect() {
          /* nothing held */
        }
      },
    );
    const tick = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
      });
    };
    try {
      const daemon = new InboxDaemon();
      daemon.sections = [
        section(REPO, {
          landed: {
            count: 1n,
            fromSeq: 4n,
            throughSeq: 5n,
            newest: [{ changeId: "e".repeat(32), subject: "Land the parser", seq: 5n }],
          },
        }),
      ];
      const stream = live(daemon);
      render(
        <Inbox
          connection={daemon.connection()}
          live={stream.live}
          frame={immediateFrames}
          onSessionInvalid={vi.fn()}
        />,
      );
      await tick(100);
      expect(screen.getByText("Land the parser")).toBeTruthy();
      await tick(SEEN_AFTER_MS * 2);
      expect(daemon.marks, "out of view").toEqual([]);
      const hidden = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
      await act(() => {
        document.dispatchEvent(new Event("visibilitychange"));
        for (const callback of observers) callback([{ isIntersecting: true }]);
      });
      await tick(SEEN_AFTER_MS * 2);
      expect(daemon.marks, "in view, but the tab is hidden").toEqual([]);
      hidden.mockReturnValue("visible");
      await act(() => {
        document.dispatchEvent(new Event("visibilitychange"));
      });
      await tick(SEEN_AFTER_MS + 100);
      expect(daemon.marks.map((m) => m.seq)).toEqual([5n]);
    } finally {
      vi.unstubAllGlobals();
      vi.useRealTimers();
    }
  });

  it("stops marking, saying nothing, when the daemon keeps no mark for this caller", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const daemon = new InboxDaemon();
      daemon.refuseMarks = daemonError(Code.PermissionDenied, {
        reason: "REMOTE_IDENTITY_REQUIRED",
      });
      daemon.sections = [
        section(REPO, {
          landed: {
            count: 1n,
            fromSeq: 4n,
            throughSeq: 5n,
            newest: [{ changeId: "e".repeat(32), subject: "Land the parser", seq: 5n }],
          },
        }),
      ];
      const stream = live(daemon);
      render(
        <Inbox
          connection={daemon.connection()}
          live={stream.live}
          frame={immediateFrames}
          onSessionInvalid={vi.fn()}
        />,
      );
      await act(async () => {
        await vi.advanceTimersByTimeAsync(100);
      });
      expect(screen.getByText("Land the parser")).toBeTruthy();
      await act(async () => {
        await vi.advanceTimersByTimeAsync(SEEN_AFTER_MS + 500);
      });
      expect(screen.queryByRole("alert")).toBeNull();
      expect(daemon.markCalls).toBe(1);
      daemon.sections = [
        section(REPO, {
          landed: {
            count: 2n,
            fromSeq: 4n,
            throughSeq: 6n,
            newest: [{ changeId: "f".repeat(32), subject: "Land the lexer", seq: 6n }],
          },
        }),
      ];
      act(() => {
        stream.send();
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(SEEN_AFTER_MS * 2);
      });
      expect(daemon.markCalls, "no mark is asked for again").toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("drops a repository the daemon no longer reads", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
      section(OTHER, { needsYouNow: [helpItem("h2", 1_000, OTHER)], needsYouNowTotal: 1 }),
    ];
    const stream = await open(daemon);
    expect(rows(needs())).toEqual([`help:${REPO}:h1`, `help:${OTHER}:h2`]);
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    await act(() => {
      stream.send();
    });
    await waitFor(() => {
      expect(document.title).toBe("(1) Inbox — Reef");
    });
    expect(within(needs()).getByText(/No longer waiting/)).toBeTruthy();
  });

  it("keeps marking after a refusal that does not mean 'no mark for you'", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    try {
      const daemon = new InboxDaemon();
      daemon.refuseMarks = daemonError(Code.PermissionDenied, { reason: "WEB_ACCESS_DENIED" });
      const landed = (through: bigint) => ({
        count: 1n,
        fromSeq: 4n,
        throughSeq: through,
        newest: [{ changeId: "e".repeat(32), subject: "Land the parser", seq: through }],
      });
      daemon.sections = [section(REPO, { landed: landed(5n) })];
      const stream = live(daemon);
      render(
        <Inbox
          connection={daemon.connection()}
          live={stream.live}
          frame={immediateFrames}
          onSessionInvalid={vi.fn()}
        />,
      );
      await act(async () => {
        await vi.advanceTimersByTimeAsync(100);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(SEEN_AFTER_MS + 500);
      });
      expect(daemon.markCalls).toBe(1);
      expect(screen.getByRole("alert")).toBeTruthy();
      daemon.sections = [section(REPO, { landed: landed(6n) })];
      act(() => {
        stream.send();
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(1_000);
      });
      await act(async () => {
        await vi.advanceTimersByTimeAsync(SEEN_AFTER_MS + 500);
      });
      expect(daemon.markCalls, "the next landing is marked again").toBe(2);
    } finally {
      vi.useRealTimers();
    }
  });
});
