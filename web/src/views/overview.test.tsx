// W7: the overview over an in-memory daemon (Connect's router transport) and
// stores the test feeds.
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError, createRouterTransport } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import {
  ListOperationsResponseSchema,
  OperationKind,
  OperationService,
  SnapshotKind,
} from "../gen/reef/api/v1/operations_pb";
import {
  ListRepositoriesResponseSchema,
  RepositoryService,
  type Repository,
} from "../gen/reef/api/v1/repositories_pb";
import { WatchInboxResponseSchema } from "../gen/reef/api/v1/inbox_pb";
import { InboxStore } from "../live/inbox";
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
import { act, cleanup, render, screen, within } from "../test/testing-library";
import { setDevMode } from "../ui/dev-mode";
import { Overview } from "./Overview";
import { RELOAD_EVERY_MS } from "./overview-model";

afterEach(() => {
  cleanup();
  vi.useRealTimers();
});

const repo = (id: string, root: string): Repository =>
  ({ id, root, colocated: true }) as Repository;

interface Daemon {
  repositories: Repository[];
  latest: Record<string, ReturnType<typeof operation>>;
  listLoads: number;
  listOperations: (repo: string) => Promise<void>;
  failRepositories?: Error;
  /** Holds each second-page answer (computed when called) until it resolves. */
  hold?: Promise<void> | undefined;
}

/** Renders the overview. The stream is live first unless `live` is false. */
function setup(daemon: Partial<Daemon> = {}, live = true) {
  const state: Daemon = {
    repositories: [],
    latest: {},
    listLoads: 0,
    listOperations: async () => undefined,
    ...daemon,
  };
  const transport = createRouterTransport(({ service }) => {
    service(RepositoryService, {
      async listRepositories(request) {
        if (state.failRepositories) throw state.failRepositories;
        if (!request.pageToken) state.listLoads += 1;
        // Two pages: the first repository, then the rest.
        const page = create(ListRepositoriesResponseSchema, {
          repositories: request.pageToken
            ? state.repositories.slice(1)
            : state.repositories.slice(0, 1),
          nextPageToken: request.pageToken || state.repositories.length <= 1 ? "" : "page2",
        });
        if (request.pageToken) await state.hold;
        return page;
      },
    });
    service(OperationService, {
      async listOperations(request) {
        await state.listOperations(request.repo);
        const latest = state.latest[request.repo];
        return create(ListOperationsResponseSchema, { operations: latest ? [latest] : [] });
      },
    });
  });
  const operations = new OperationStore(immediateFrames);
  const presence = new PresenceStore(immediateFrames);
  const inbox = new InboxStore(immediateFrames);
  if (live) {
    operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
    inbox.applyFrame(create(WatchInboxResponseSchema, { synced: true }));
  }
  const onSessionInvalid = vi.fn();
  render(
    <Overview
      connection={{ transport, label: "test" }}
      live={{ operations, presence, inbox } as unknown as Live}
      frame={immediateFrames}
      onSessionInvalid={onSessionInvalid}
    />,
  );
  return { state, operations, presence, onSessionInvalid };
}

afterEach(() => {
  setDevMode(false);
});

function row(name: string): HTMLElement {
  const cell = screen.getByText(name);
  const card = cell.closest("li");
  if (!card) throw new Error(`no card for ${name}`);
  return card;
}

async function flush() {
  await act(async () => {
    await settle(30);
  });
}

describe("Overview", () => {
  it("renders repositories from two pages", async () => {
    setDevMode(true);
    setup({ repositories: [repo("aaaaaaaa11", "/src/alpha"), repo("bbbbbbbb22", "/src/beta/")] });
    await flush();
    expect(row("alpha").textContent).toContain("aaaaaaaa");
    act(() => {
      setDevMode(false);
    });
    // The id is a developer detail.
    expect(row("alpha").textContent).not.toContain("aaaaaaaa");
    expect(row("beta").getAttribute("title")).toBeNull();
    expect(screen.getByText("beta").getAttribute("title")).toBe("/src/beta/");
  });

  it("links each repository to its smartlog (R2.19 test 10)", async () => {
    setup({ repositories: [repo("aaaaaaaa11", "/src/alpha")] });
    await flush();
    const link = screen.getByRole("link", { name: "alpha" });
    expect(link.getAttribute("href")).toBe("/repos/aaaaaaaa11/smartlog");
    act(() => {
      link.click();
    });
    expect(window.location.pathname).toBe("/repos/aaaaaaaa11/smartlog");
    window.history.replaceState(null, "", "/");
  });

  it("keeps the newer of the listed and the streamed operation, whichever arrives first", async () => {
    setDevMode(true);
    let release: () => void = () => undefined;
    const listed = new Promise<void>((resolve) => {
      release = resolve;
    });
    const t = setup({
      repositories: [repo("ra", "/src/alpha"), repo("rb", "/src/beta")],
      latest: {
        ra: operation("ra-old", "ra", 1000, { kind: OperationKind.DESCRIBE, agent: "ann" }),
        rb: operation("rb-new", "rb", 5000, {
          kind: OperationKind.SNAPSHOT,
          snapshotKind: SnapshotKind.WATCHER,
          agent: "bob",
          workspaceId: "0123456789abcdef",
        }),
      },
      listOperations: () => listed,
    });
    await flush();
    await act(async () => {
      // Streamed before the list returns: newer for ra, older for rb.
      t.operations.applyFrame(
        operationsFrame({
          live: true,
          operations: [
            operation("ra-new", "ra", 2000, { kind: OperationKind.ABANDON, agent: "cat" }),
            operation("rb-old", "rb", 3000, { kind: OperationKind.EDIT, agent: "dan" }),
          ],
        }),
      );
      await settle();
    });
    release();
    await flush();
    expect(row("alpha").textContent).toContain("abandon · cat");
    expect(row("beta").textContent).toContain("snapshot (watcher) · bob · 01234567");
    expect(
      within(row("beta"))
        .getByText(/\d\d:\d\d:\d\d/)
        .getAttribute("title"),
    ).toBe(new Date(5000).toISOString());
  });

  it("counts distinct active agents per repository from presence", async () => {
    const t = setup({ repositories: [repo("ra", "/src/alpha")] });
    await flush();
    const agents = () =>
      within(row("alpha")).getByRole("link", { name: /agents? working$/ }).textContent;
    expect(agents()).toBe("No agents working");
    await act(async () => {
      t.presence.applyFrame(
        presenceFrame(
          [
            { id: "p1", agent: "ann", repoId: "ra" },
            { id: "p2", agent: "ann", repoId: "ra" },
            { id: "p3", agent: "bob", repoId: "ra" },
            { id: "p4", agent: "cat", repoId: "" },
          ],
          true,
        ),
      );
      await settle();
    });
    expect(agents()).toBe("2 agents working");
    await act(async () => {
      t.presence.applyFrame(presenceFrame([{ id: "p3", ended: true }]));
      await settle();
    });
    expect(agents()).toBe("1 agent working");
  });

  it("counts an agent and its workspace's owner once, and links into the workspaces page (R2.20 test 9)", async () => {
    const t = setup({
      repositories: [repo("ra", "/src/alpha")],
      latest: {
        ra: operation("o1", "ra", 1000, {
          kind: OperationKind.SNAPSHOT,
          agent: "reviewer",
          workspaceId: "ws12345678abcdef",
        }),
      },
    });
    await flush();
    await act(async () => {
      t.presence.applyFrame(
        presenceFrame(
          [
            { id: "a", agent: "reviewer", repoId: "ra", workspaceId: "ws1" },
            { id: "l", agent: "ann@host:42", repoId: "ra", workspaceId: "ws1", byDaemon: true },
          ],
          true,
        ),
      );
      await settle();
    });
    const count = within(row("alpha")).getByRole("link", { name: "1 agent working" });
    expect(count.getAttribute("href")).toBe("/repos/ra/workspaces");
    expect(within(row("alpha")).getByRole("link", { name: "reviewer" }).getAttribute("href")).toBe(
      "/repos/ra/workspaces?agent=reviewer",
    );
    expect(within(row("alpha")).getByRole("link", { name: "ws123456" }).getAttribute("href")).toBe(
      "/repos/ra/workspaces?workspace=ws12345678abcdef",
    );
    // The operation itself links to the feed with it selected (R2.21), as a
    // sentence, or the API's word in developer mode.
    expect(
      within(row("alpha")).getByRole("link", { name: "saved a snapshot" }).getAttribute("href"),
    ).toBe("/repos/ra/activity?op=o1");
    act(() => {
      setDevMode(true);
    });
    expect(within(row("alpha")).getByRole("link", { name: "snapshot" }).getAttribute("href")).toBe(
      "/repos/ra/activity?op=o1",
    );
  });

  it("reloads the list for an unknown repository at most once per second", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const t = setup({ repositories: [repo("ra", "/src/alpha")] });
    await flush();
    expect(t.state.listLoads).toBe(1);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS);
    });
    const unknown = (id: string, repoId: string) => {
      t.operations.applyFrame(
        operationsFrame({ live: true, operations: [operation(id, repoId, 1)] }),
      );
    };
    t.state.repositories.push(repo("rz", "/src/zeta"));
    await act(async () => {
      unknown("z1", "rz");
      await settle(30);
    });
    expect(t.state.listLoads).toBe(2);
    expect(screen.getByText("zeta")).toBeTruthy();
    // Operations for another unknown repository within the next second wait for it.
    await act(async () => {
      unknown("y1", "ry");
      unknown("y2", "ry");
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS / 2);
    });
    expect(t.state.listLoads).toBe(2);
    await act(async () => {
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS / 2);
      await settle(30);
    });
    expect(t.state.listLoads).toBe(3);
  });

  it("shows a failed read's hint and next", async () => {
    setup({
      failRepositories: daemonError(
        Code.Unavailable,
        { hint: "Start the daemon.", next: ["reef daemon start"] },
        "the daemon is not running",
      ),
    });
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("the daemon is not running");
    expect(screen.getByText("Start the daemon.")).toBeTruthy();
    expect(screen.getByText("reef daemon start")).toBeTruthy();
  });

  it("reads a repository whose folder is gone every 30 s, and says so all along", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    let gone = true;
    let asked = 0;
    const t = setup({
      repositories: [repo("aaaaaaaa11", "/src/alpha"), repo("bbbbbbbb22", "/src/gone")],
      listOperations: async (id) => {
        await settle(1);
        if (id !== "bbbbbbbb22") return;
        asked += 1;
        if (gone) throw daemonError(Code.Unavailable, { reason: "" }, "it is not open");
      },
    });
    const pass = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
        await settle(60);
      });
    };
    await pass(0);
    for (let s = 0; s < 60; s++) {
      await pass(1_000);
      expect(screen.getByRole("alert").textContent).toContain("it is not open");
    }
    const minute = asked;
    expect(minute).toBeLessThan(12);
    await pass(9 * 60_000);
    expect(asked - minute).toBeLessThanOrEqual(18);
    expect(t.state.listLoads, "the repositories are not read for it").toBe(1);
    gone = false;
    await pass(30_000);
    expect(screen.queryByRole("alert")).toBeNull();
    const healed = asked;
    await pass(10 * 60_000);
    expect(asked).toBe(healed);
    expect(t.state.listLoads).toBe(1);
    cleanup();
    setup({
      repositories: [repo("bbbbbbbb22", "/src/gone")],
      listOperations: () => Promise.reject(daemonError(Code.Unavailable, { reason: "" })),
    });
    await pass(1_000);
    cleanup();
    expect(vi.getTimerCount(), "a page left reads nothing").toBe(0);
  });

  it("says that a repository could not be read until it was, though the list is", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const t = setup({
      repositories: [repo("aaaaaaaa11", "/src/alpha")],
      listOperations: async () => {
        await settle(1);
        throw daemonError(Code.Internal, { reason: "BROKEN" }, "broken");
      },
    });
    await flush();
    t.operations.applyFrame(
      operationsFrame({ operations: [operation("o1", "unknown", 1)], live: true, cursor: "c1" }),
    );
    for (let i = 0; i < 10; i++) {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS / 5);
        await settle(5);
      });
      expect(screen.getByRole("alert").textContent).toContain("broken");
    }
    expect(t.state.listLoads).toBe(2);
    const loads = t.state.listLoads;
    // One that is forgotten is no error.
    t.state.repositories = [];
    t.operations.applyFrame(
      operationsFrame({ operations: [operation("o2", "other", 2)], live: true, cursor: "c2" }),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS);
      await settle(60);
    });
    await flush();
    expect(t.state.listLoads).toBe(loads + 1);
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("with several gone, asks for them together, and says so until the last is read", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const gone = new Set(["bbbbbbbb22", "cccccccc33"]);
    let held = false;
    let asked = 0;
    const t = setup({
      repositories: [
        repo("aaaaaaaa11", "/src/alpha"),
        repo("bbbbbbbb22", "/src/b"),
        repo("cccccccc33", "/src/c"),
      ],
      listOperations: async (id) => {
        await settle(1);
        asked += 1;
        if (held && id === "cccccccc33") await new Promise(() => undefined);
        if (gone.has(id)) throw daemonError(Code.Unavailable, { reason: "" }, `${id} is not open`);
      },
    });
    const pass = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
        await settle(60);
      });
    };
    await pass(0);
    expect(vi.getTimerCount(), "one backoff for all").toBe(1);
    await pass(60_000);
    const before = asked;
    await pass(5 * 60_000);
    expect(asked - before).toBeLessThanOrEqual(22);
    gone.delete("cccccccc33");
    await pass(31_000);
    expect(screen.getByRole("alert").textContent).toContain("bbbbbbbb22");
    gone.delete("bbbbbbbb22");
    await pass(31_000);
    expect(screen.queryByRole("alert")).toBeNull();
    // The backoff starts over for the next failure.
    gone.add("bbbbbbbb22");
    t.operations.reset();
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c2" }));
    await pass(1_000);
    asked = 0;
    await pass(2_000);
    expect(asked).toBeGreaterThan(0);
    // One that was read says nothing of another.
    gone.add("cccccccc33");
    t.operations.reset();
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c3" }));
    await pass(1_000);
    expect(screen.getByRole("alert").textContent).toContain("bbbbbbbb22");
    held = true;
    gone.delete("bbbbbbbb22");
    await pass(31_000);
    expect(screen.getByRole("alert").textContent).toContain("cccccccc33");
  });

  it("drops what failed before the stream started over, and after the page was left", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const held: ((error: Error) => void)[] = [];
    const t = setup({
      repositories: [repo("aaaaaaaa11", "/src/alpha")],
      listOperations: () => new Promise<void>((_, fail) => held.push(fail)),
    });
    await flush();
    expect(held).toHaveLength(1);
    t.operations.reset();
    t.operations.applyFrame(operationsFrame({ live: true, cursor: "c2" }));
    await flush();
    held.shift()?.(daemonError(Code.Unavailable, { reason: "" }, "stale"));
    await flush();
    expect(screen.queryByRole("alert")).toBeNull();
    cleanup();
    for (const fail of held) fail(daemonError(Code.Unavailable, { reason: "" }, "late"));
    await flush();
    expect(vi.getTimerCount()).toBe(0);
    const u = setup({ repositories: [repo("aaaaaaaa11", "/src/alpha")] });
    u.state.failRepositories = new ConnectError("Load failed", Code.Unknown);
    cleanup();
    await flush();
    expect(vi.getTimerCount()).toBe(0);
  });

  it("lists the repositories again after no answer, and not after a refusal", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    for (const [thrown, again] of [
      [new ConnectError("Load failed", Code.Unknown), true],
      [daemonError(Code.Internal, { reason: "BROKEN" }, "broken"), false],
    ] as const) {
      const t = setup({ repositories: [repo("aaaaaaaa11", "/src/alpha")] });
      t.state.failRepositories = thrown;
      await flush();
      expect(screen.getByRole("alert")).toBeTruthy();
      delete t.state.failRepositories;
      await act(async () => {
        await vi.advanceTimersByTimeAsync(60_000);
        await settle(60);
      });
      expect(t.state.listLoads).toBe(again ? 1 : 0);
      expect(screen.queryByRole("alert") === null).toBe(again);
      cleanup();
      expect(vi.getTimerCount()).toBe(0);
    }
  });

  it("waits a second at most to list again when the clock was set back", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const t = setup({ repositories: [repo("aaaaaaaa11", "/src/alpha")] });
    await flush();
    vi.setSystemTime(Date.now() - 3_600_000);
    t.operations.applyFrame(
      operationsFrame({ operations: [operation("o1", "unknown", 1)], live: true, cursor: "c1" }),
    );
    await act(async () => {
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS);
      await settle(60);
    });
    expect(t.state.listLoads).toBe(2);
  });

  it("ends the session when a read is refused", async () => {
    const t = setup({
      failRepositories: daemonError(Code.PermissionDenied, { reason: "WEB_SESSION_INVALID" }),
    });
    await flush();
    expect(t.onSessionInvalid).toHaveBeenCalled();
  });

  it("opens on the inbox, above the repositories (R3.33)", async () => {
    setup();
    await flush();
    const inbox = await screen.findByRole("region", { name: "Needs you now" });
    expect(within(inbox).getByText("Nothing needs you now.")).toBeTruthy();
    const headings = screen.getAllByRole("heading", { level: 1 }).map((h) => h.textContent);
    expect(headings).toEqual(["Inbox", "Repositories"]);
  });

  it("says how to register a repository when there are none", async () => {
    setup();
    await flush();
    expect(screen.getByText(/No repositories are registered/).textContent).toContain("reef init");
  });

  it("reads nothing before the stream is live", async () => {
    const calls: string[] = [];
    const t = setup(
      {
        repositories: [repo("ra", "/src/alpha")],
        listOperations: async (r) => {
          calls.push(r);
        },
      },
      false,
    );
    await flush();
    expect(t.state.listLoads).toBe(0);
    expect(calls).toEqual([]);
    expect(screen.getByText("Loading repositories.")).toBeTruthy();
    await act(async () => {
      t.operations.applyFrame(operationsFrame({ live: true, cursor: "c" }));
      await settle(30);
    });
    expect(t.state.listLoads).toBe(1);
    expect(calls).toEqual(["ra"]);
  });

  it("runs a reload asked for during a load after it", async () => {
    vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
    const t = setup({ repositories: [repo("ra", "/src/alpha")] });
    await flush();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS);
    });
    let release: () => void = () => undefined;
    t.state.hold = new Promise<void>((resolve) => {
      release = resolve;
    });
    t.state.repositories.push(repo("rz", "/src/zeta"));
    const unknown = (repoId: string) => {
      t.operations.applyFrame(
        operationsFrame({ live: true, operations: [operation(`${repoId}-1`, repoId, 1)] }),
      );
    };
    await act(async () => {
      unknown("rz");
      await settle(30);
    });
    expect(t.state.listLoads).toBe(2);
    // Registered after that load read the list, and announced while it is
    // still under way.
    t.state.repositories.push(repo("ry", "/src/yota"));
    await act(async () => {
      unknown("ry");
      t.state.hold = undefined;
      release();
      await settle(30);
    });
    expect(screen.getByText("zeta")).toBeTruthy();
    expect(screen.queryByText("yota")).toBeNull();
    await act(async () => {
      await vi.advanceTimersByTimeAsync(RELOAD_EVERY_MS);
      await settle(30);
    });
    expect(t.state.listLoads).toBe(3);
    expect(screen.getByText("yota")).toBeTruthy();
  });
});

describe("Overview, outside developer mode (the design pass)", () => {
  it("tells the latest operation as a sentence and sums up the fleet", async () => {
    const t = setup({
      repositories: [repo("ra", "/src/alpha")],
      latest: {
        ra: operation("o1", "ra", 1000, {
          kind: OperationKind.SNAPSHOT,
          snapshotKind: SnapshotKind.WATCHER,
          agent: "reviewer",
          workspaceId: "ws12345678abcdef",
          description: "reef: snapshot\nmore",
        }),
      },
    });
    await flush();
    await act(async () => {
      t.presence.applyFrame(
        presenceFrame([{ id: "a", agent: "reviewer", repoId: "ra", workspaceId: "ws1" }], true),
      );
      await settle();
    });
    const card = row("alpha");
    expect(card.textContent).toContain("reviewer saved a snapshot in ws123456 · ");
    expect(card.textContent).toContain("reef: snapshot");
    expect(card.textContent).not.toContain("more");
    const time = card.querySelector("time");
    expect(time?.getAttribute("datetime")).toBe(new Date(1000).toISOString());
    expect(time?.getAttribute("title")).toMatch(/^\d{4}-\d\d-\d\d \d\d:\d\d:\d\d$/);
    expect(within(card).getByRole("link", { name: "1 agent working" })).toBeTruthy();
    expect(card.querySelectorAll(".repo-agents .avatar")).toHaveLength(1);
    expect(screen.getByText(/^1 repository · 1 agent working now · last activity/)).toBeTruthy();
  });
});
