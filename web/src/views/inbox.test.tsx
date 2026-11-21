// The triage inbox (R3.33) over an in-memory daemon: buckets held in place
// behind a count, reads on the listed operation and presence kinds and not
// on a snapshot, the Answer box, and a digest marked seen once on screen.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import { create } from "@bufbuild/protobuf";
import { InboxReason, InboxSectionSchema } from "../gen/reef/api/v1/inbox_pb";
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { PresenceKind } from "../gen/reef/api/v1/presence_pb";
import { after } from "../live/backoff";
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
import { helpItem, InboxDaemon, OTHER, proposalItem, REPO, section } from "../test/inbox-daemon";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "../test/testing-library";
import { Inbox, SEEN_AFTER_MS } from "./Inbox";

afterEach(() => {
  cleanup();
});

/** Real time passes: a paced read would have come by then. */
function pause(ms: number): Promise<void> {
  return new Promise((resolve) => {
    after(ms, resolve);
  });
}

function live() {
  const operations = new OperationStore(immediateFrames);
  operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const presence = new PresenceStore(immediateFrames);
  presence.applyFrame(presenceFrame([], true));
  let cursor = 1;
  return {
    live: { operations, presence } as unknown as Live,
    push: (kind: OperationKind, repo = REPO) => {
      operations.applyFrame(
        operationsFrame({
          operations: [
            operation(`op${String(cursor)}`, repo, 1_795_000_000_000 + cursor, { kind }),
          ],
          live: true,
          cursor: `c${String(cursor++)}`,
        }),
      );
    },
    status: (id: string, ms: number) => {
      presence.applyFrame(
        presenceFrame([
          { id, kind: PresenceKind.AUTOMATIC, repoId: REPO, updatedAt: timestampFromMs(ms) },
        ]),
      );
    },
  };
}

async function open(daemon: InboxDaemon, repo?: string) {
  const stream = live();
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
  await waitFor(() => {
    expect(daemon.reads.length).toBe(1);
  });
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
      stream.push(OperationKind.COMMENT);
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

  it("reads on the kinds it names and a status entry, never on a snapshot alone", async () => {
    const daemon = new InboxDaemon();
    const stream = await open(daemon);
    await act(async () => {
      stream.push(OperationKind.SNAPSHOT);
      await settle(20);
    });
    await pause(300);
    expect(daemon.reads.length).toBe(1);
    await act(() => {
      stream.push(OperationKind.TRANSITION);
    });
    await waitFor(() => {
      expect(daemon.reads.length).toBe(2);
    });
    await act(() => {
      stream.status("auto1", 1);
    });
    await waitFor(() => {
      expect(daemon.reads.length).toBe(3);
    });
    await act(async () => {
      stream.push(OperationKind.COMMENT, OTHER);
      await settle(20);
    });
    await pause(300);
    expect(daemon.reads.length).toBe(3);
  });

  it("answers a help request in place, and says so when it leaves", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    await open(daemon);
    expect(screen.getByText("TOML or YAML?")).toBeTruthy();
    expect(screen.getByText("Tried: read the docs")).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Answer" }));
    fireEvent.change(screen.getByLabelText("Your answer"), { target: { value: "TOML" } });
    daemon.sections = [section(REPO)];
    fireEvent.click(screen.getByRole("button", { name: "Answer" }));
    await waitFor(() => {
      expect(daemon.answers.length).toBe(1);
    });
    expect(daemon.answers[0]?.replyTo).toBe("h1");
    expect(daemon.answers[0]?.text).toBe("TOML");
    await waitFor(() => {
      expect(screen.getByText(/Answered/)).toBeTruthy();
    });
  });

  it("marks a digest seen after it was on screen, and keeps its base for the visit", async () => {
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
      const stream = live();
      render(
        <Inbox
          connection={daemon.connection()}
          live={stream.live}
          frame={immediateFrames}
          onSessionInvalid={vi.fn()}
        />,
      );
      await tick(100);
      expect(daemon.reads.length).toBe(1);
      expect(screen.getByText("Land the parser")).toBeTruthy();
      await tick(SEEN_AFTER_MS - 500);
      expect(daemon.marks).toEqual([]);
      await tick(1_000);
      expect(daemon.marks.map((m) => [m.repo, m.seq])).toEqual([[REPO, 9n]]);
      stream.push(OperationKind.TRANSITION);
      await tick(1_000);
      expect(daemon.reads.length).toBe(2);
      expect(daemon.reads[1]?.digestFrom.map((m) => [m.repo, m.seq])).toEqual([[REPO, 4n]]);
      expect(screen.getByText("Land the parser")).toBeTruthy();
      await tick(SEEN_AFTER_MS + 500);
      expect(daemon.marks.length).toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("keeps a failed repository's rows as they were, and says it could not read it", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    const stream = await open(daemon);
    daemon.sections = [
      create(InboxSectionSchema, {
        repoId: REPO,
        error: { reason: "REPOSITORY_NOT_FOUND", message: "the repository is gone" },
      }),
    ];
    await act(() => {
      stream.push(OperationKind.COMMENT);
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

  it("keeps a digest's first words for the visit: a mark set since does not change them", async () => {
    const daemon = new InboxDaemon();
    const landed = (fromMark: boolean, ms: number) => ({
      count: 1n,
      fromSeq: 4n,
      throughSeq: 5n,
      fromMark,
      since: timestampFromMs(ms),
      newest: [{ changeId: "e".repeat(32), subject: "Land the parser", seq: 5n }],
    });
    daemon.sections = [section(REPO, { landed: landed(false, 1_000) })];
    const stream = await open(daemon);
    expect(screen.getByText(/in the last day/)).toBeTruthy();
    daemon.sections = [section(REPO, { landed: landed(true, 9_000_000_000_000) })];
    await act(() => {
      stream.push(OperationKind.TRANSITION);
    });
    await waitFor(() => {
      expect(daemon.reads.length).toBe(2);
    });
    await act(async () => {
      await settle(20);
    });
    expect(screen.getByText(/in the last day/)).toBeTruthy();
    expect(screen.queryByText(/since you last looked,/)).toBeNull();
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
      const stream = live();
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
      const stream = live();
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
      stream.push(OperationKind.TRANSITION);
      await act(async () => {
        await vi.advanceTimersByTimeAsync(SEEN_AFTER_MS * 2);
      });
      expect(daemon.markCalls, "no mark is asked for again").toBe(1);
    } finally {
      vi.useRealTimers();
    }
  });

  it("drops a repository the daemon no longer reads, and its kept base", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
      section(OTHER, {
        needsYouNow: [helpItem("h2", 1_000, OTHER)],
        needsYouNowTotal: 1,
        landed: { count: 0n, fromSeq: 7n, throughSeq: 7n },
      }),
    ];
    const stream = await open(daemon);
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    await act(() => {
      stream.push(OperationKind.COMMENT);
    });
    await waitFor(() => {
      expect(daemon.reads.length).toBe(2);
    });
    await act(() => {
      stream.push(OperationKind.COMMENT);
    });
    await waitFor(() => {
      expect(daemon.reads.length).toBe(3);
    });
    expect(daemon.reads[2]?.digestFrom.map((m) => m.repo)).toEqual([REPO]);
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
      const stream = live();
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
      stream.push(OperationKind.TRANSITION);
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
