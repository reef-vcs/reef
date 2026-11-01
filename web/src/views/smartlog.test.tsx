// R2.19 test 9: the smartlog view over an in-memory daemon, with the route's
// query driving it as the app's does.
import { Code } from "@connectrpc/connect";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { OperationStore } from "../live/operations";
import { PresenceStore } from "../live/presence";
import type { Live } from "../live/share";
import {
  daemonError,
  immediateFrames,
  operationsFrame,
  presenceFrame,
  settle,
} from "../test/fakes";
import { entries, FakeRepoDaemon, hexId, REPO } from "../test/smartlog-daemon";
import { reverseHex } from "../ui/ids";
import { useSearch } from "../ui/router";
import { Smartlog, smartlogParams } from "./Smartlog";

const LOG = `
a2 a1
a1 t4
b1 t4
t4! :t2
t2! ~t1
`;

beforeEach(() => {
  window.history.replaceState(null, "", `/repos/${REPO}/smartlog`);
});

afterEach(() => {
  cleanup();
});

function setup(
  daemon: FakeRepoDaemon = withLog(LOG),
  { query = "", presence = [] as Parameters<typeof presenceFrame>[0] } = {},
) {
  if (query) window.history.replaceState(null, "", `/repos/${REPO}/smartlog?${query}`);
  const operations = new OperationStore(immediateFrames);
  operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const store = new PresenceStore(immediateFrames);
  store.beginResync();
  store.applyFrame(presenceFrame(presence, true));
  const live = { operations, presence: store } as unknown as Live;
  const connection = daemon.connection();
  const onSessionInvalid = vi.fn();
  function Harness() {
    const params = smartlogParams(REPO, useSearch());
    return (
      <Smartlog
        key={params.at}
        connection={connection}
        live={live}
        frame={immediateFrames}
        onSessionInvalid={onSessionInvalid}
        params={params}
      />
    );
  }
  render(<Harness />);
  return { daemon, operations, onSessionInvalid };
}

function withLog(text: string, fields: Parameters<typeof entries>[1] = {}): FakeRepoDaemon {
  const daemon = new FakeRepoDaemon();
  daemon.log = entries(text, fields);
  return daemon;
}

async function flush() {
  await act(async () => {
    await settle(60);
  });
}

const options = () => screen.getAllByRole("option");
const option = (commit: string) => {
  const found = options().find((o) => o.getAttribute("data-commit") === commit);
  if (!found) throw new Error(`no row for ${commit}`);
  return found;
};
const selectedChange = () => new URLSearchParams(window.location.search).get("change");

describe("Smartlog", () => {
  it("renders each row's state, ids, labels and description", async () => {
    setup(
      withLog(LOG, {
        a2: {
          bookmarks: ["feature", "feature@origin"],
          workspaceIds: ["ws00000000000001"],
          empty: true,
        },
        b1: { description: "", tags: ["v1"], jjWorkspaces: ["default"] },
        t4: { bookmarks: ["main"] },
      }),
      {
        presence: ["ann", "bob", "cat", "dan"].map((agent, i) => ({
          id: `p${String(i)}`,
          agent,
          repoId: REPO,
          ...(i === 0 ? { workspaceId: "ws00000000000001" } : { changeId: hexId("a2") }),
        })),
      },
    );
    await flush();
    expect(options()).toHaveLength(5);
    const a2 = option("a2");
    expect(a2.textContent).toContain(reverseHex(hexId("a2")).slice(0, 8));
    expect(within(a2).getByText("feature").className).toBe("label");
    expect(within(a2).getByText("feature@origin").className).toContain("secondary");
    expect(a2.textContent).toContain("ws ws000000 agent-a");
    expect(within(a2).getByText("ann")).toBeTruthy();
    expect(within(a2).getByText("+1")).toBeTruthy();
    expect(within(a2).queryByText("dan")).toBeNull();
    expect(within(a2).getByText("empty")).toBeTruthy();
    // Draft rows: the shape, no word; landed rows: both.
    expect(within(a2).queryByText("draft")).toBeNull();
    expect(a2.querySelector("svg circle")).not.toBeNull();
    expect(within(option("t4")).getByText("landed")).toBeTruthy();
    const b1 = option("b1");
    expect(within(b1).getByText("tag:v1")).toBeTruthy();
    expect(within(b1).getByText("jj default")).toBeTruthy();
    expect(within(b1).getByText("(no description)")).toBeTruthy();
    expect(within(option("a1")).getByText("a1: a change")).toBeTruthy();
  });

  it("is a listbox whose rows say in words what the graph shows", async () => {
    setup();
    await flush();
    expect(screen.getByRole("listbox", { name: "Changes" })).toBeTruthy();
    const name = option("a2").getAttribute("aria-label") ?? "";
    expect(name).toMatch(/^draft, change /);
    expect(name).toContain("a2: a change");
    expect(option("t4").getAttribute("aria-label")).toContain(
      "joins its parent through left-out commits",
    );
    expect(option("t2").getAttribute("aria-label")).toContain("parent outside this view");
    for (const svg of document.querySelectorAll(".rows svg")) {
      expect(svg.getAttribute("aria-hidden")).toBe("true");
    }
  });

  it("selects by click and keyboard, in the URL without history entries", async () => {
    setup();
    await flush();
    const before = window.history.length;
    act(() => {
      fireEvent.click(option("a1"));
    });
    expect(selectedChange()).toBe(reverseHex(hexId("a1")));
    expect(option("a1").getAttribute("aria-selected")).toBe("true");
    expect(document.activeElement).toBe(option("a1"));
    const list = screen.getByRole("listbox");
    const key = (k: string) => {
      act(() => {
        fireEvent.keyDown(list, { key: k });
      });
    };
    key("ArrowDown");
    expect(selectedChange()).toBe(reverseHex(hexId("b1")));
    key("ArrowUp");
    expect(selectedChange()).toBe(reverseHex(hexId("a1")));
    key("End");
    expect(selectedChange()).toBe(reverseHex(hexId("t2")));
    key("Home");
    expect(selectedChange()).toBe(reverseHex(hexId("a2")));
    key("Escape");
    expect(selectedChange()).toBeNull();
    expect(window.history.length).toBe(before);
  });

  it("leaves focus in place when Escape clears the selection", async () => {
    setup(withLog(LOG), { query: `change=${reverseHex(hexId("t4"))}` });
    await flush();
    act(() => {
      option("t4").focus();
    });
    act(() => {
      fireEvent.keyDown(screen.getByRole("listbox"), { key: "Escape" });
    });
    await flush();
    expect(selectedChange()).toBeNull();
    expect(document.activeElement).toBe(option("t4"));
  });

  it("starts at the first row when the selection is not in view", async () => {
    setup(withLog(LOG), { query: "change=zzzzzzzz" });
    await flush();
    act(() => {
      fireEvent.keyDown(screen.getByRole("listbox"), { key: "ArrowDown" });
    });
    expect(selectedChange()).toBe(reverseHex(hexId("a2")));
  });

  it("puts the state's word in every row's title, draft rows too", async () => {
    setup();
    await flush();
    expect(option("a2").getAttribute("title")).toBe("draft");
    expect(option("t4").getAttribute("title")).toBe("landed");
  });

  it("shows the selected change's details as text, with files and history", async () => {
    const daemon = withLog(LOG, {
      a1: { description: "<script>alert(1)</script>\n\nbody line" },
    });
    setup(daemon, { query: `change=${reverseHex(hexId("a1"))}` });
    await flush();
    const pane = screen.getByRole("complementary", { name: "Selected change" });
    expect(within(pane).getByText(/<script>alert\(1\)<\/script>/)).toBeTruthy();
    expect(document.querySelector("script")).toBeNull();
    expect(within(pane).getByText("direct")).toBeTruthy();
    expect(within(pane).getByText("src/a.rs")).toBeTruthy();
    expect(within(pane).getByText("+3 -1")).toBeTruthy();
    expect(within(pane).getByText("and 119 more files")).toBeTruthy();
    expect(pane.textContent).toContain("agent-a in ws000000");
    expect(daemon.diffCalls[0]?.toRevision).toBe("a1");
    // Live: a commit's files are read at the head, once per commit.
    expect(daemon.diffCalls[0]?.atOperation).toBe("");
    expect(daemon.historyCalls[0]?.changes).toEqual([hexId("a1")]);
  });

  it("shows a history error only under the change it is for", async () => {
    const daemon = withLog(LOG);
    daemon.failHistory = daemonError(Code.Unavailable, { reason: "HISTORY_BEHIND", hint: "later" });
    setup(daemon, { query: `change=${reverseHex(hexId("a1"))}` });
    await flush();
    expect(screen.getByText("later")).toBeTruthy();
    daemon.failHistory = undefined;
    act(() => {
      fireEvent.click(option("b1"));
    });
    expect(screen.queryByText("later")).toBeNull();
  });

  it("says history is off rather than failing", async () => {
    const daemon = withLog(LOG);
    daemon.failHistory = daemonError(Code.FailedPrecondition, { reason: "HISTORY_DISABLED" });
    setup(daemon, { query: `change=${reverseHex(hexId("a2"))}` });
    await flush();
    expect(screen.getByText("History is off for this daemon.")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("lists both commits of a divergent change, and says when a change is not shown", async () => {
    setup(
      withLog(
        `
        x1/x t
        x2/x t
        t!
      `,
        { x1: { divergent: true }, x2: { divergent: true } },
      ),
      { query: `change=${reverseHex(hexId("x"))}` },
    );
    await flush();
    expect(option("x1").getAttribute("aria-selected")).toBe("true");
    expect(option("x2").getAttribute("aria-selected")).toBe("true");
    expect(screen.getByText(/has 2 commits in this view/)).toBeTruthy();
    expect(within(option("x1")).getByText("divergent")).toBeTruthy();

    cleanup();
    setup(withLog(LOG), { query: "change=zzzzzzzz" });
    await flush();
    expect(screen.getByText(/This change is not in this view/)).toBeTruthy();
  });

  it("takes a revset, shows a bad one under the field, and prints the command", async () => {
    const daemon = withLog(LOG);
    setup(daemon);
    await flush();
    expect(screen.getByText(/^reef log --repo a3dda866c7e8f343 -r 'trunk\(\)/)).toBeTruthy();
    daemon.failList = daemonError(
      Code.InvalidArgument,
      {
        reason: "INVALID_REVSET",
        hint: "Check the revset",
      },
      "Failed to parse revset",
    );
    const field = screen.getByLabelText("Revset");
    act(() => {
      fireEvent.change(field, { target: { value: "nonsense(" } });
      fireEvent.submit(field);
    });
    await flush();
    expect(new URLSearchParams(window.location.search).get("revset")).toBe("nonsense(");
    expect(daemon.listCalls.at(-1)?.revset).toBe("nonsense(");
    expect(screen.getByText("Failed to parse revset")).toBeTruthy();
    expect(screen.getByLabelText("Revset").getAttribute("aria-invalid")).toBe("true");
    // The message with its hint, and the last good graph below.
    expect(screen.getByText("Check the revset")).toBeTruthy();
    expect(options()).toHaveLength(5);
    expect(screen.getByText(/^Showing the last graph read/).textContent).toContain(
      "of trunk() | working_copies() | ancestors(mutable(), 2)",
    );
  });

  it("shows 200 more up to 1,000, then asks for a narrower revset", async () => {
    const daemon = withLog(LOG);
    daemon.pageCap = 2;
    setup(daemon, { query: "limit=2" });
    await flush();
    expect(screen.getByText("More changes than shown.")).toBeTruthy();
    act(() => {
      fireEvent.click(screen.getByRole("button", { name: "Show 200 more" }));
    });
    await flush();
    expect(new URLSearchParams(window.location.search).get("limit")).toBe("202");
    expect(daemon.listCalls.at(-1)?.pageSize).toBeLessThanOrEqual(202);
    expect(screen.queryByText("More changes than shown.")).toBeNull();

    cleanup();
    const full = withLog(
      Array.from({ length: 1001 }, (_, i) => `n${String(i)} n${String(i + 1)}`).join("\n"),
    );
    setup(full, { query: "limit=1000" });
    await flush();
    expect(screen.getByText("Narrow the revset to see more.")).toBeTruthy();
  });

  it("shows the view at an operation, frozen, with a way back to live", async () => {
    setup(withLog(LOG), {
      query: "at=abcd",
      presence: [{ id: "p", agent: "ann", repoId: REPO, changeId: hexId("a2") }],
    });
    await flush();
    expect(screen.getByRole("status").textContent).toContain("At operation abcd-ful");
    // The banner's operation links to the feed with it selected (R2.21).
    expect(
      within(screen.getByRole("status"))
        .getByRole("link", { name: "abcd-ful" })
        .getAttribute("href"),
    ).toBe(`/repos/${REPO}/activity?op=abcd-full&at=abcd-full`);
    expect(screen.getByText(/Not live; presence is not replayed/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Back to live" }).getAttribute("href")).toBe(
      `/repos/${REPO}/smartlog`,
    );
    expect(within(option("a2")).queryByText("ann")).toBeNull();
    expect(screen.getByText(/^reef log .*--at-op abcd-full/)).toBeTruthy();
    act(() => {
      fireEvent.click(option("a1"));
    });
    await flush();
    expect(screen.getByText(/^reef diff .* --stat --at-op abcd-full$/)).toBeTruthy();
  });

  it("labels an agent and its owner once, and links them from the pane, not the rows (R2.20 test 10)", async () => {
    setup(withLog(LOG, { a2: { workspaceIds: ["ws00000000000001"] } }), {
      query: `change=${reverseHex(hexId("a2"))}`,
      presence: [
        { id: "c", agent: "reviewer", repoId: REPO, workspaceId: "ws00000000000001" },
        {
          id: "l",
          agent: "agent-a",
          repoId: REPO,
          workspaceId: "ws00000000000001",
          byDaemon: true,
        },
      ],
    });
    await flush();
    const a2 = option("a2");
    expect(within(a2).getByText("reviewer")).toBeTruthy();
    expect(a2.querySelectorAll(".label.agent")).toHaveLength(1);
    expect(a2.querySelector("a")).toBeNull();
    const pane = screen.getByRole("complementary", { name: "Selected change" });
    expect(within(pane).getByRole("link", { name: "ws00000000000001" }).getAttribute("href")).toBe(
      `/repos/${REPO}/workspaces?workspace=ws00000000000001`,
    );
    expect(
      within(pane)
        .getAllByRole("link", { name: "reviewer" })
        .every((l) => l.getAttribute("href") === `/repos/${REPO}/workspaces?agent=reviewer`),
    ).toBe(true);
    // The repository header renders the nav (R2.21).
    expect(screen.queryByRole("navigation", { name: "Repository" })).toBeNull();
    expect(screen.getByRole("link", { name: "1 active workspaces" }).getAttribute("href")).toBe(
      `/repos/${REPO}/workspaces`,
    );
  });

  it("says when nothing matches, and shows a failed read's hint and next", async () => {
    setup(withLog(""));
    await flush();
    expect(screen.getByText("No changes match this revset.")).toBeTruthy();

    cleanup();
    const failing = withLog(LOG);
    failing.failList = daemonError(Code.NotFound, {
      reason: "REPOSITORY_NOT_FOUND",
      hint: "Register it with reef init",
      next: ["reef repo list"],
    });
    setup(failing);
    await flush();
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("Register it with reef init");
    expect(alert.textContent).toContain("reef repo list");
  });
});
