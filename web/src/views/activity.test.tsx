// R2.21 test 7: the activity page over an in-memory log, with the route's
// query driving it as the app's does.
import { create } from "@bufbuild/protobuf";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  ChangeEffectKind,
  ChangeEffectSchema,
  OperationEffectsSchema,
  OperationKind,
  type Operation,
} from "../gen/reef/api/v1/operations_pb";
import { OperationStore } from "../live/operations";
import type { Live } from "../live/share";
import { OperationWindow } from "../log/window";
import { Code } from "@connectrpc/connect";
import { daemonError, immediateFrames, operationsFrame, settle } from "../test/fakes";
import { FakeOplog, REPO, T } from "../test/oplog-daemon";
import { useSearch } from "../ui/router";
import { Activity } from "./Activity";
import { activityParams } from "./activity-route";

const MIN = 60_000;

beforeEach(() => {
  window.history.replaceState(null, "", `/repos/${REPO}/activity`);
});

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
});

/** A workspace's life: created, three snapshots by ann, described, released; and a jj operation. */
function story(oplog = new FakeOplog()): { oplog: FakeOplog; ops: Operation[] } {
  const ops = [
    oplog.add({
      endedAt: T,
      kind: OperationKind.CREATE_WORKSPACE,
      workspaceId: "w1",
      agent: "ann",
      description: "reef: create workspace w1",
    }),
    oplog.add({ endedAt: T + MIN, workspaceId: "w1", agent: "ann" }),
    oplog.add({ endedAt: T + 2 * MIN, workspaceId: "w1", agent: "ann" }),
    oplog.add({ endedAt: T + 3 * MIN, workspaceId: "w1", agent: "ann" }),
    oplog.add({
      endedAt: T + 4 * MIN,
      kind: OperationKind.EXTERNAL,
      jjWorkspace: "default",
      username: "bob",
      hostname: "box",
      description: "describe commit 1234",
    }),
    oplog.add({
      endedAt: T + 5 * MIN,
      kind: OperationKind.DESCRIBE,
      workspaceId: "w1",
      agent: "ann",
      description: "<script>alert(1)</script>",
    }),
  ];
  return { oplog, ops };
}

async function setup(
  oplog: FakeOplog,
  { query = "", state = "live" }: { query?: string; state?: string } = {},
) {
  if (query) window.history.replaceState(null, "", `/repos/${REPO}/activity?${query}`);
  const store = new OperationStore(immediateFrames);
  let cursor = 0;
  const push = (operations: Operation[]) => {
    cursor += 1;
    store.applyFrame(operationsFrame({ operations, live: true, cursor: `c${String(cursor)}` }));
  };
  push([]);
  const live = {
    operations: store,
    state,
    subscribeState: () => () => undefined,
  } as unknown as Live;
  const connection = oplog.connection();
  const win = new OperationWindow(connection, store, immediateFrames, () => undefined, REPO);
  win.start();
  function Harness() {
    return (
      <Activity
        connection={connection}
        live={live}
        frame={immediateFrames}
        onSessionInvalid={() => undefined}
        window={win}
        params={activityParams(REPO, useSearch())}
      />
    );
  }
  render(<Harness />);
  await flush();
  return { win, push };
}

async function flush() {
  await act(async () => {
    await settle(300);
  });
}

const rows = () => within(screen.getByRole("table")).getAllByRole("row").slice(1);
const cells = (row: HTMLElement) =>
  within(row)
    .getAllByRole("cell")
    .map((c) => c.textContent);

describe("Activity", () => {
  it("renders each row's columns, the summary and the command", async () => {
    const s = story();
    await setup(s.oplog);
    const r = rows();
    expect(r.map((row) => cells(row)[1])).toEqual([
      "describe<script>alert(1)</script>",
      "1 jj operation",
      "3 snapshots",
      "create workspacereef: create workspace w1",
    ]);
    expect(cells(r[2] as HTMLElement)[2]).toBe("ann");
    expect(cells(r[2] as HTMLElement)[4]).toBe("3");
    expect(cells(r[1] as HTMLElement)[2]).toBe("bob@box");
    expect(cells(r[1] as HTMLElement)[3]).toBe("default");
    expect(
      within(r[2] as HTMLElement)
        .getByRole("link", { name: "ann" })
        .getAttribute("href"),
    ).toBe(`/repos/${REPO}/workspaces?agent=ann`);
    expect(
      within(r[2] as HTMLElement)
        .getByRole("link", { name: "w1" })
        .getAttribute("href"),
    ).toBe(`/repos/${REPO}/workspaces?workspace=w1`);
    expect(screen.getByText(/^6 operations from/).textContent).toContain(
      "in 4 rows · 3 snapshots · 1 jj · 1 create · 1 describe · 1 agent · 1 workspace",
    );
    // The CLI lists the root too, which the page reached.
    expect(screen.getByText(/^reef op log/).textContent).toBe(
      `reef op log --repo ${REPO} --at-op ${s.ops[5]?.id.slice(0, 16) ?? ""} --limit 7`,
    );
    expect(screen.getByText(/Start of the retained log/)).toBeTruthy();
  });

  it("renders API text as text", async () => {
    await setup(story().oplog);
    expect(screen.getByText("<script>alert(1)</script>")).toBeTruthy();
    expect(document.querySelector("script")).toBeNull();
  });

  it("offers older pages until the root", async () => {
    const oplog = new FakeOplog();
    for (let i = 0; i < 1500; i++)
      oplog.add({ endedAt: T + i * 1000, workspaceId: "w1", agent: "ann" });
    await setup(oplog);
    expect(screen.getByText(/^1,000 operations/)).toBeTruthy();
    fireEvent.click(screen.getByRole("button", { name: "Load 1,000 older" }));
    await flush();
    expect(screen.getByText(/^1,500 operations/)).toBeTruthy();
    expect(screen.queryByRole("button", { name: /older/ })).toBeNull();
    expect(screen.getByText(/Start of the retained log/)).toBeTruthy();
  });

  it("applies new operations at once while following, and the run grows in place", async () => {
    const s = story();
    const { push } = await setup(s.oplog);
    const next = s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w1", agent: "ann" });
    await act(async () => {
      push([next]);
      await settle(20);
    });
    expect(cells(rows()[0] as HTMLElement)[1]).toBe("1 snapshot");
    expect(rows()[0]?.className).toContain("changed");
    expect(screen.getByText(/^7 operations from/)).toBeTruthy();
  });

  it("holds new operations while the first row is out of view, behind a count", async () => {
    const s = story();
    const { push } = await setup(s.oplog);
    const first = rows()[0] as HTMLElement;
    vi.spyOn(first, "getBoundingClientRect").mockReturnValue({
      bottom: -10,
      height: 30,
    } as DOMRect);
    await act(async () => {
      window.dispatchEvent(new Event("scroll"));
      await settle(10);
    });
    const next = s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w1", agent: "ann" });
    await act(async () => {
      push([next]);
      await settle(20);
    });
    expect(rows()).toHaveLength(4);
    expect(screen.getByText(/^6 operations from/)).toBeTruthy();
    Element.prototype.scrollIntoView = () => undefined;
    fireEvent.click(screen.getByRole("button", { name: "1 new operation · Show" }));
    await flush();
    expect(rows()).toHaveLength(5);
    expect(screen.getByText(/^7 operations from/)).toBeTruthy();
  });

  it("holds while focus is in the table", async () => {
    const s = story();
    const { push } = await setup(s.oplog);
    const link = within(rows()[2] as HTMLElement).getByRole("link", { name: "3 snapshots" });
    await act(async () => {
      link.focus();
      await settle(10);
    });
    await act(async () => {
      push([s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w2", agent: "cy" })]);
      await settle(20);
    });
    expect(screen.getByRole("button", { name: "1 new operation · Show" })).toBeTruthy();
    expect(rows()).toHaveLength(4);
  });

  it("selects a run's newest operation and shows it with its effects", async () => {
    const s = story();
    const snap = s.ops[3] as Operation;
    snap.effects = create(OperationEffectsSchema, {
      changes: [
        create(ChangeEffectSchema, {
          changeId: "0123456789abcdef",
          kind: ChangeEffectKind.REWRITTEN,
          commitId: "c0ffee",
        }),
      ],
      changeCount: 3,
    });
    await setup(s.oplog);
    fireEvent.click(screen.getByRole("link", { name: "3 snapshots" }));
    await flush();
    expect(window.location.search).toBe(`?op=${snap.id}`);
    const pane = screen.getByRole("complementary", { name: "Selection" });
    expect(within(pane).getByText("3 operations in this row")).toBeTruthy();
    expect(within(pane).getByText(snap.id)).toBeTruthy();
    const change = within(pane).getByRole("link", { name: "zyxwvuts" });
    expect(change.getAttribute("href")).toBe(
      `/repos/${REPO}/smartlog?change=zyxwvutsrqponmlk&at=${snap.id}`,
    );
    expect(within(pane).getByText("and 2 more")).toBeTruthy();
    expect(rows()[2]?.getAttribute("aria-current")).toBe("true");
  });

  it("links an abandoned change at the operation's parent", async () => {
    const s = story();
    const op = s.ops[5] as Operation;
    op.effects = create(OperationEffectsSchema, {
      changes: [
        create(ChangeEffectSchema, {
          changeId: "0123456789abcdef",
          kind: ChangeEffectKind.ABANDONED,
        }),
      ],
      changeCount: 1,
    });
    await setup(s.oplog, { query: `op=${op.id}` });
    const pane = screen.getByRole("complementary", { name: "Selection" });
    expect(within(pane).getByRole("link", { name: "zyxwvuts" }).getAttribute("href")).toContain(
      `at=${op.parentIds[0] ?? ""}`,
    );
  });

  it("shows the feed at a point: the banner and the point's ancestors", async () => {
    const s = story();
    await setup(s.oplog, { query: `at=${s.ops[2]?.id ?? ""}` });
    await flush();
    expect(screen.getByRole("status").textContent).toContain("At operation");
    expect(rows().map((row) => cells(row)[1])).toEqual([
      "2 snapshots",
      "create workspacereef: create workspace w1",
    ]);
    expect(screen.queryByText(/new operation/)).toBeNull();
  });

  it("says so when not live, and when the feed was reloaded", async () => {
    const s = story();
    const { push } = await setup(s.oplog, { state: "reconnecting" });
    expect(screen.getByText(/Not live: the last operation arrived at/)).toBeTruthy();
    s.oplog.gc();
    await act(async () => {
      push([s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w1", agent: "ann" })]);
      await settle(300);
    });
    expect(
      screen.getByText("Garbage collection rewrote the operation log; the feed was reloaded."),
    ).toBeTruthy();
    expect(screen.getByText(/^7 operations from/)).toBeTruthy();
  });

  it("says when the log holds only the root", async () => {
    await setup(new FakeOplog());
    expect(screen.getByText("No operations yet besides the root.")).toBeTruthy();
  });

  it("has a caption, column headers and a labelled pane", async () => {
    await setup(story().oplog);
    expect(screen.getByRole("table", { name: "Activity" })).toBeTruthy();
    expect(screen.getAllByRole("columnheader").map((h) => h.textContent)).toEqual([
      "When",
      "What",
      "Who",
      "Where",
      "Ops",
    ]);
    expect(screen.getByRole("complementary", { name: "Selection" })).toBeTruthy();
  });
});

describe("Activity, after review", () => {
  it("grows a run in place without the arrival transition", async () => {
    const oplog = new FakeOplog();
    for (let i = 0; i < 3; i++)
      oplog.add({ endedAt: T + i * MIN, workspaceId: "w1", agent: "ann" });
    const { push } = await setup(oplog);
    const before = rows()[0];
    await act(async () => {
      push([oplog.add({ endedAt: T + 3 * MIN, workspaceId: "w1", agent: "ann" })]);
      await settle(20);
    });
    expect(rows()[0]).toBe(before);
    expect(cells(rows()[0] as HTMLElement)[4]).toBe("4");
    expect(rows()[0]?.className ?? "").not.toContain("changed");
  });

  it("does not flash older pages or a reload as arrivals", async () => {
    const oplog = new FakeOplog();
    // One run: an older page extends it at the bottom, which changes its key.
    for (let i = 0; i < 1500; i++)
      oplog.add({ endedAt: T + i * 1000, workspaceId: "w1", agent: "ann" });
    await setup(oplog);
    expect(rows()).toHaveLength(1);
    fireEvent.click(screen.getByRole("button", { name: "Load 1,000 older" }));
    await flush();
    expect(cells(rows()[0] as HTMLElement)[4]).toBe("1,500");
    expect(document.querySelectorAll("tr.changed")).toHaveLength(0);
  });

  it("shows the cap's note with the command for older history", async () => {
    const s = story();
    const { win, push } = await setup(s.oplog);
    const real = win.getSnapshot();
    vi.spyOn(win, "getSnapshot").mockReturnValue({
      ...real,
      reachedRoot: false,
      capped: true,
      more: false,
      version: real.version + 1,
    });
    // Any publish re-reads the (mocked) snapshot.
    await act(async () => {
      push([s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w1", agent: "ann" })]);
      await settle(20);
    });
    expect(screen.getByText(/10,000 operations, the most this page holds/)).toBeTruthy();
    expect(screen.getByText(/reef op log --repo .* --limit 0/)).toBeTruthy();
    // Without the root the command counts what the feed counts.
    expect(document.querySelector(".feed .command code")?.textContent).toMatch(/--limit 6$/);
  });

  it("shows a failed list's error with its hint", async () => {
    const oplog = story().oplog;
    oplog.failList = daemonError(Code.Internal, { hint: "see the daemon's log" });
    await setup(oplog);
    expect(screen.getByRole("alert").textContent).toContain("see the daemon's log");
  });

  it("labels a detached point's page", async () => {
    const s = story();
    const { win, push } = await setup(s.oplog, { query: `at=${s.ops[1]?.id ?? ""}` });
    vi.spyOn(win, "point").mockReturnValue({
      state: "detached",
      operation: s.ops[1] as Operation,
      records: win.ancestors(s.ops[1]?.id ?? ""),
    });
    await act(async () => {
      push([s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w1", agent: "ann" })]);
      await settle(20);
    });
    expect(screen.getByText(/ending at this point, not joined to the loaded period/)).toBeTruthy();
  });

  it("shows a selection the window does not hold as not in the loaded period", async () => {
    const s = story();
    const old = s.ops[2]?.id ?? "";
    const { push } = await setup(s.oplog, { query: `op=${old}` });
    s.oplog.gc();
    await act(async () => {
      push([s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w1", agent: "ann" })]);
      await settle(300);
    });
    // The pane is keyed by the selection: reopen it after the reload.
    window.history.replaceState(null, "", `/repos/${REPO}/activity`);
    await act(async () => {
      window.dispatchEvent(new PopStateEvent("popstate"));
      await settle(20);
    });
    window.history.replaceState(null, "", `/repos/${REPO}/activity?op=${old}`);
    await act(async () => {
      window.dispatchEvent(new PopStateEvent("popstate"));
      await settle(100);
    });
    const pane = screen.getByRole("complementary", { name: "Selection" });
    expect(within(pane).getByText(/Not in the loaded period/)).toBeTruthy();
  });

  it("goes to the top after Show applies the held rows", async () => {
    const s = story();
    const { push } = await setup(s.oplog);
    const first = rows()[0] as HTMLElement;
    vi.spyOn(first, "getBoundingClientRect").mockReturnValue({
      bottom: -10,
      height: 30,
    } as DOMRect);
    await act(async () => {
      window.dispatchEvent(new Event("scroll"));
      await settle(10);
    });
    await act(async () => {
      push([s.oplog.add({ endedAt: T + 6 * MIN, workspaceId: "w2", agent: "cy" })]);
      await settle(20);
    });
    const scrolled = vi.fn();
    Element.prototype.scrollIntoView = scrolled;
    fireEvent.click(screen.getByRole("button", { name: "1 new operation · Show" }));
    await flush();
    expect(rows()).toHaveLength(5);
    expect(scrolled).toHaveBeenCalledTimes(1);
  });
});
