// R2.20 test 7: the workspaces page over an in-memory daemon, with the
// route's query driving it as the app's does.
import { create } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { LeaseSchema, Attention, WorkspaceState } from "../gen/reef/api/v1/workspaces_pb";
import { PresenceEndReason, PresenceKind } from "../gen/reef/api/v1/presence_pb";
import { OperationStore } from "../live/operations";
import { PresenceStore } from "../live/presence";
import type { Live } from "../live/share";
import { immediateFrames, operation, operationsFrame, presenceFrame, settle } from "../test/fakes";
import { change, FakeFleetDaemon, REPO, T, workspace } from "../test/fleet-daemon";
import { reverseHex } from "../ui/ids";
import { useSearch } from "../ui/router";
import { Fleet } from "./Fleet";
import { fleetParams } from "./fleet-route";

const C1 = "c1".padEnd(32, "0");
const C2 = "c2".padEnd(32, "0");

beforeEach(() => {
  window.history.replaceState(null, "", `/repos/${REPO}/workspaces`);
});

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

/** Lets a pushed operation's spaced refresh run. */
async function afterSpacing() {
  await act(async () => {
    await settle(300);
    vi.advanceTimersByTime(1000);
    await settle(300);
  });
}

function withTwo(): FakeFleetDaemon {
  const daemon = new FakeFleetDaemon();
  daemon.workspaces = [
    workspace("w2000000", C2, {
      attention: Attention.WATCH_FAILED,
      purpose: "<script>alert(1)</script>",
      changedFileCount: undefined,
    }),
    workspace("w1000000", C1, { owner: "bob@host" }),
  ];
  daemon.changes = [change(C1, { immutable: true }), change(C2)];
  return daemon;
}

function setup(
  daemon: FakeFleetDaemon = withTwo(),
  {
    query = "",
    presence = [] as Parameters<typeof presenceFrame>[0],
    state = "live",
  }: { query?: string; presence?: Parameters<typeof presenceFrame>[0]; state?: string } = {},
) {
  if (query) window.history.replaceState(null, "", `/repos/${REPO}/workspaces?${query}`);
  const operations = new OperationStore(immediateFrames);
  operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const store = new PresenceStore(immediateFrames);
  store.beginResync();
  store.applyFrame(presenceFrame(presence, true));
  const live = {
    operations,
    presence: store,
    state,
    subscribeState: () => () => undefined,
  } as unknown as Live;
  const connection = daemon.connection();
  function Harness() {
    const params = fleetParams(REPO, useSearch());
    return (
      <Fleet
        key={params.at}
        connection={connection}
        live={live}
        frame={immediateFrames}
        onSessionInvalid={vi.fn()}
        params={params}
      />
    );
  }
  render(<Harness />);
  let cursor = 1;
  const push = (...ops: ReturnType<typeof operation>[]) => {
    operations.applyFrame(
      operationsFrame({ operations: ops, live: true, cursor: `c${String(cursor++)}` }),
    );
  };
  return { daemon, presence: store, push };
}

async function flush() {
  await act(async () => {
    await settle(300);
  });
}

const table = (name: string) => screen.getByRole("table", { name });
const row = (name: string, text: string) =>
  within(table(name))
    .getAllByRole("row")
    .find((r) => r.textContent.includes(text))!;
const cells = (r: HTMLElement) =>
  within(r)
    .getAllByRole("cell")
    .map((c) => c.textContent);

describe("Fleet", () => {
  it("lists workspaces with their columns and a summary", async () => {
    setup();
    await flush();
    const headers = within(table("Workspaces"))
      .getAllByRole("columnheader")
      .map((h) => h.textContent);
    expect(headers).toEqual([
      "Workspace",
      "Owner",
      "Agents",
      "Change",
      "Files",
      "Disk",
      "Lease",
      "Last snapshot",
    ]);
    const w2 = row("Workspaces", "w2000000");
    expect(w2.textContent).toContain("watch failed");
    // Unset counts read "-", and the purpose is text.
    expect(cells(w2)[4]).toBe("-");
    expect(within(w2).getByText("<script>alert(1)</script>")).toBeTruthy();
    const w1 = row("Workspaces", "w1000000");
    expect(cells(w1)[1]).toContain("bob@host");
    expect(cells(w1)[4]).toBe("2");
    expect(w1.textContent).toContain("landed");
    expect(w1.textContent).toContain("not heartbeating");
    expect(screen.getByText(/^2 active · 1 need attention$/)).toBeTruthy();
  });

  it("links changes to the smartlog and selects without a history entry", async () => {
    setup();
    await flush();
    const w1 = row("Workspaces", "w1000000");
    const changeLink = within(w1).getByRole("link", { name: reverseHex(C1).slice(0, 8) });
    expect(changeLink.getAttribute("href")).toBe(
      `/repos/${REPO}/smartlog?change=${reverseHex(C1)}`,
    );
    const length = window.history.length;
    fireEvent.click(within(w1).getByRole("link", { name: "w1000000" }));
    await flush();
    expect(window.location.search).toBe("?workspace=w1000000");
    expect(window.history.length).toBe(length);
    expect(row("Workspaces", "w1000000").getAttribute("aria-current")).toBe("true");
  });

  it("shows disk once measured, in the CLI's units", async () => {
    setup();
    await flush();
    await flush();
    const w2 = row("Workspaces", "w2000000");
    expect(cells(w2)[5]).toBe("18 MiB");
    expect(within(w2).getAllByRole("cell")[5]!.getAttribute("title")).toMatch(
      /^3 files differ from the base, measured \d\d:\d\d:\d\d$/,
    );
  });

  it("joins agents to workspaces and counts each once", async () => {
    setup(withTwo(), {
      presence: [
        {
          id: "a",
          agent: "reviewer",
          repoId: REPO,
          workspaceId: "w1000000",
          kind: PresenceKind.ACTIVITY,
          activity: "running tests",
          updatedAt: timestampFromMs(T),
        },
        {
          id: "l",
          agent: "bob@host",
          repoId: REPO,
          workspaceId: "w1000000",
          kind: PresenceKind.LEASE,
          byDaemon: true,
          lease: create(LeaseSchema, { expiresAt: timestampFromMs(T) }),
        },
      ],
    });
    await flush();
    const agents = within(table("Agents")).getAllByRole("row").slice(1);
    expect(agents).toHaveLength(1);
    expect(cells(agents[0]!)[0]).toBe("reviewer");
    expect(cells(agents[0]!)[3]).toBe("running tests");
    expect(cells(agents[0]!)[4]).toBe("heartbeating");
    expect(cells(row("Workspaces", "w1000000"))[2]).toBe("reviewer");
  });

  it("keeps a gone agent's row with why it ended, until hidden", async () => {
    const t = setup(withTwo(), {
      presence: [{ id: "a", agent: "reviewer", repoId: REPO, workspaceId: "w1000000" }],
    });
    await flush();
    await act(async () => {
      t.presence.applyFrame(
        presenceFrame([
          {
            id: "a",
            agent: "reviewer",
            repoId: REPO,
            ended: true,
            endReason: PresenceEndReason.CLEARED,
          },
        ]),
      );
      await settle();
    });
    expect(row("Agents", "reviewer").textContent).toMatch(/finished \d\d:\d\d:\d\d/);
    fireEvent.click(screen.getByRole("button", { name: "Hide 1 gone" }));
    await flush();
    expect(screen.getByText("No agents are reporting presence in this repository.")).toBeTruthy();
  });

  it("keeps a released workspace's row in place", async () => {
    // Timers only: a read held across a faked clock would stretch the spacing.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const t = setup();
    await flush();
    t.daemon.workspaces = t.daemon.workspaces.filter((w) => w.id !== "w2000000");
    t.daemon.finals.set("w2000000", workspace("w2000000", C2, { state: WorkspaceState.RELEASED }));
    t.push(operation("o-rel", REPO, T, { workspaceId: "w2000000", agent: "ann" }));
    await afterSpacing();
    const rows = within(table("Workspaces")).getAllByRole("row").slice(1);
    expect(rows.map((r) => cells(r)[0]?.slice(0, 8))).toEqual(["w2000000", "w1000000"]);
    expect(rows[0]!.textContent).toContain("released");
    fireEvent.click(screen.getByRole("button", { name: "Hide 1 ended" }));
    await flush();
    expect(within(table("Workspaces")).getAllByRole("row")).toHaveLength(2);
  });

  it("shows the workspace pane with Measure again, and a message for an unknown selection", async () => {
    setup(withTwo(), { query: "workspace=w1000000" });
    await flush();
    await flush();
    const pane = screen.getByRole("complementary", { name: "Selection" });
    expect(pane.textContent).toContain("w1000000");
    expect(pane.textContent).toContain("reef ws inspect w1000000");
    expect(pane.textContent).toContain("18 MiB, 3 files differ from the base");
    expect(within(pane).getByRole("button", { name: "Measure again" })).toBeTruthy();
    cleanup();
    setup(withTwo(), { query: "workspace=gone0000" });
    await flush();
    expect(screen.getByText("Not active in this repository.")).toBeTruthy();
  });

  it("renders no nav of its own: the repository header does (R2.21)", async () => {
    setup();
    await flush();
    expect(screen.queryByRole("navigation", { name: "Repository" })).toBeNull();
  });

  it("says when presence is not synced, and when the page is not live", async () => {
    const daemon = withTwo();
    const operations = new OperationStore(immediateFrames);
    operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
    const store = new PresenceStore(immediateFrames);
    store.beginResync();
    const live = {
      operations,
      presence: store,
      state: "reconnecting",
      subscribeState: () => () => undefined,
    } as unknown as Live;
    render(
      <Fleet
        connection={daemon.connection()}
        live={live}
        frame={immediateFrames}
        onSessionInvalid={vi.fn()}
        params={fleetParams(REPO, "")}
      />,
    );
    await flush();
    expect(screen.getByText("Waiting for presence.")).toBeTruthy();
    expect(screen.getByText(/^Not live: the last update was at \d\d:\d\d:\d\d\.$/)).toBeTruthy();
  });

  it("reads at an operation: a banner, no Agents section, no disk", async () => {
    const t = setup(withTwo(), { query: "at=abcd" });
    await flush();
    expect(screen.getByRole("status").textContent).toMatch(/^At operation abcd-ful/);
    // The banner's operation links to the feed with it selected (R2.21).
    expect(
      within(screen.getByRole("status"))
        .getByRole("link", { name: "abcd-ful" })
        .getAttribute("href"),
    ).toBe(`/repos/${REPO}/activity?op=abcd-full&at=abcd-full`);
    expect(screen.queryByRole("table", { name: "Agents" })).toBeNull();
    expect(cells(row("Workspaces", "w1000000"))[6]).toMatch(/^issued \d\d:\d\d:\d\d$/);
    expect(t.daemon.getCalls).toHaveLength(0);
  });

  it("marks a word change once, and not a count change", async () => {
    // Timers only: a read held across a faked clock would stretch the spacing.
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const t = setup();
    await flush();
    const changed = () =>
      [...row("Workspaces", "w1000000").querySelectorAll(".changed")].map((e) => e.textContent);
    expect(changed()).toEqual([]);
    t.daemon.workspaces = [
      t.daemon.workspaces[0]!,
      workspace("w1000000", C1, { owner: "bob@host", changedFileCount: 9 }),
    ];
    t.push(operation("o1", REPO, T));
    await afterSpacing();
    expect(cells(row("Workspaces", "w1000000"))[4]).toBe("9");
    expect(changed()).toEqual([]);
    t.daemon.workspaces = [
      t.daemon.workspaces[0]!,
      workspace("w1000000", C2, { owner: "bob@host", attention: Attention.DIVERGENT }),
    ];
    t.push(operation("o2", REPO, T + 1));
    await afterSpacing();
    expect(changed().length).toBeGreaterThan(0);
  });

  it("takes the pane's record from the listing, which alone counts files (review finding 1)", async () => {
    setup(withTwo(), { query: "workspace=w1000000" });
    await flush();
    await flush();
    const pane = screen.getByRole("complementary", { name: "Selection" });
    expect(pane.textContent).toContain("18 MiB");
    expect(pane.textContent).toMatch(/Files changed2/);
  });

  it("keeps the operation in every link of a frozen page (review finding 2)", async () => {
    setup(withTwo(), { query: "at=abcd" });
    await flush();
    const w1 = row("Workspaces", "w1000000");
    expect(within(w1).getByRole("link", { name: "w1000000" }).getAttribute("href")).toBe(
      `/repos/${REPO}/workspaces?workspace=w1000000&at=abcd`,
    );
    expect(
      within(w1)
        .getByRole("link", { name: reverseHex(C1).slice(0, 8) })
        .getAttribute("href"),
    ).toContain("&at=abcd");
    fireEvent.click(within(w1).getByRole("link", { name: "w1000000" }));
    await flush();
    expect(window.location.search).toBe("?workspace=w1000000&at=abcd");
    expect(screen.getByRole("status").textContent).toMatch(/^At operation/);
  });

  it("says it is loading, and what to do with no workspaces", async () => {
    const daemon = withTwo();
    let release: () => void = () => undefined;
    daemon.hold = new Promise((resolve) => {
      release = resolve;
    });
    setup(daemon);
    await flush();
    expect(screen.getAllByText("Loading workspaces.")).toHaveLength(2);
    daemon.workspaces = [];
    daemon.hold = undefined;
    release();
    await flush();
    expect(screen.getByText(/^No active workspaces in this repository\./)).toBeTruthy();
  });

  it("marks a row's ended form and a gone agent once, and not a resync's dashes", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const t = setup(withTwo(), {
      presence: [
        { id: "a", agent: "reviewer", repoId: REPO, workspaceId: "w2000000" },
        {
          id: "l",
          agent: "ann@host",
          repoId: REPO,
          workspaceId: "w1000000",
          kind: PresenceKind.LEASE,
          byDaemon: true,
        },
      ],
    });
    await flush();
    const health = () => within(row("Workspaces", "w1000000")).getAllByRole("cell")[6]!;
    expect(health().querySelector(".changed")).toBeNull();
    // A resync: every health reads "-", then the same word again.
    await act(async () => {
      t.presence.beginResync();
      await settle();
    });
    await act(async () => {
      t.presence.applyFrame(
        presenceFrame(
          [
            { id: "a", agent: "reviewer", repoId: REPO, workspaceId: "w2000000" },
            {
              id: "l",
              agent: "ann@host",
              repoId: REPO,
              workspaceId: "w1000000",
              kind: PresenceKind.LEASE,
              byDaemon: true,
            },
          ],
          true,
        ),
      );
      await settle();
    });
    expect(health().querySelector(".changed")).toBeNull();
    await act(async () => {
      t.presence.applyFrame(presenceFrame([{ id: "a", ended: true }]));
      await settle();
    });
    expect(row("Agents", "reviewer").className).toContain("changed");
    t.daemon.workspaces = t.daemon.workspaces.filter((w) => w.id !== "w2000000");
    t.daemon.finals.set("w2000000", workspace("w2000000", C2, { state: WorkspaceState.RELEASED }));
    t.push(operation("o-rel", REPO, T, { workspaceId: "w2000000" }));
    await afterSpacing();
    const first = within(row("Workspaces", "w2000000")).getAllByRole("cell")[0]!;
    expect(first.querySelector(".changed")?.textContent).toContain("released");
  });

  it("marks a workspace that arrives live", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    const t = setup();
    await flush();
    t.daemon.workspaces = [workspace("w3000000", C1), ...t.daemon.workspaces];
    t.push(operation("o-new", REPO, T, { workspaceId: "w3000000" }));
    await afterSpacing();
    expect(row("Workspaces", "w3000000").className).toContain("changed");
    expect(row("Workspaces", "w1000000").className).not.toContain("changed");
  });
});
