// R2.21 test 8: the repository frame. Its header carries the nav and the
// scrubber; its operation window survives moving between the three pages
// and moving `at`, and is replaced with the repository.
import type { DescMethod, DescService } from "@bufbuild/protobuf";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import type { Connection } from "../api/connection";
import { OperationService } from "../gen/reef/api/v1/operations_pb";
import { RepositoryService } from "../gen/reef/api/v1/repositories_pb";
import { InboxReason } from "../gen/reef/api/v1/inbox_pb";
import { InboxStore } from "../live/inbox";
import { OperationStore } from "../live/operations";
import { PresenceStore } from "../live/presence";
import type { Live } from "../live/share";
import { immediateFrames, operationsFrame, presenceFrame, settle } from "../test/fakes";
import { FakeFleetDaemon, workspace } from "../test/fleet-daemon";
import { FakeOplog, REPO, T } from "../test/oplog-daemon";
import { InboxDaemon, proposalItem, section } from "../test/inbox-daemon";
import { narrow } from "../test/narrow";
import { act, cleanup, render, screen, within } from "../test/testing-library";
import { navigate, usePath, useSearch } from "../ui/router";
import type { RepoPage } from "../ui/RepoNav";
import { RepoFrame } from "./RepoFrame";
import { withDevMode } from "../test/dev-mode";

// These tests check the ids, commands and exact times developer mode shows.
withDevMode();

beforeEach(() => {
  window.history.replaceState(null, "", `/repos/${REPO}/activity`);
});

afterEach(cleanup);

async function flush() {
  await act(async () => {
    await settle(300);
  });
}

function setup() {
  const oplog = new FakeOplog();
  for (let i = 0; i < 3; i++) oplog.add({ endedAt: T + i * 60_000, workspaceId: "w1000000" });
  const fleet = new FakeFleetDaemon();
  fleet.workspaces = [workspace("w1000000", "c1".padEnd(32, "0"))];
  // The workspaces page's reads from one fake, the log's from the other.
  const fromOplog = [OperationService, RepositoryService] as DescService[];
  const log = oplog.connection().transport;
  const workspaces = fleet.connection().transport;
  const pick = (method: DescMethod) => (fromOplog.includes(method.parent) ? log : workspaces);
  const connection: Connection = {
    transport: {
      unary: (method, ...rest) => pick(method).unary(method, ...rest),
      stream: (method, ...rest) => pick(method).stream(method, ...rest),
    },
    label: "test",
  };
  const operations = new OperationStore(immediateFrames);
  operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const presence = new PresenceStore(immediateFrames);
  presence.beginResync();
  presence.applyFrame(presenceFrame([], true));
  const inbox = new InboxStore(immediateFrames);
  const needs = new InboxDaemon();
  needs.sections = [
    section(REPO, {
      needsYouNow: [proposalItem("d".repeat(32), InboxReason.SIGNOFF, 1_000, "Needs a sign-off")],
      needsYouNowTotal: 1,
    }),
  ];
  inbox.beginResync();
  inbox.applyFrame(needs.frame());
  const live = {
    inbox,
    operations,
    presence,
    state: "live",
    subscribeState: () => () => undefined,
  } as unknown as Live;
  function Harness() {
    const path = usePath();
    const search = useSearch();
    const [, , repo = "", page = ""] = path.split("/");
    return (
      <RepoFrame
        change=""
        key={repo}
        repo={repo}
        page={page as RepoPage}
        search={search}
        connection={connection}
        live={live}
        frame={immediateFrames}
        onSessionInvalid={() => undefined}
      />
    );
  }
  render(<Harness />);
  return { oplog };
}

const nav = () => screen.getByRole("navigation", { name: "Repository" });

describe("RepoFrame", () => {
  it("heads the page with the nav, its current page marked, and the scrubber", async () => {
    setup();
    await flush();
    const links = within(nav()).getAllByRole("link");
    expect(links.map((l) => l.textContent)).toEqual([
      "Smartlog",
      "Workspaces",
      "Activity",
      "Inbox 1",
    ]);
    expect(links.map((l) => l.getAttribute("aria-current"))).toEqual([null, null, "page", null]);
    expect(screen.getByRole("slider")).toBeTruthy();
    expect(await screen.findByRole("table", { name: "Activity" })).toBeTruthy();
  });

  it("keeps one window across the pages and `at`, and the nav keeps `at`", async () => {
    const { oplog } = setup();
    await flush();
    await act(async () => {
      navigate(`/repos/${REPO}/workspaces`);
      await settle(300);
    });
    expect(await screen.findByRole("table", { name: "Workspaces" })).toBeTruthy();
    const op = oplog.walk()[1]?.id ?? "";
    await act(async () => {
      navigate(`/repos/${REPO}/workspaces?at=${op}`);
      await settle(300);
    });
    expect(within(nav()).getByRole("link", { name: "Activity" }).getAttribute("href")).toBe(
      `/repos/${REPO}/activity?at=${op}`,
    );
    await act(async () => {
      navigate(`/repos/${REPO}/activity?at=${op}`);
      await settle(300);
    });
    expect(await screen.findByRole("table", { name: "Activity" })).toBeTruthy();
    expect(oplog.listCalls.filter((c) => !c.atOperation)).toHaveLength(1);
  });

  it("has no scrubber on the pages read live", async () => {
    window.history.replaceState(null, "", `/repos/${REPO}/inbox`);
    setup();
    await flush();
    expect(await screen.findByRole("heading", { name: /^Needs you now/ })).toBeTruthy();
    expect(screen.queryByRole("slider")).toBeNull();
    await act(async () => {
      navigate(`/repos/${REPO}/proposal`);
      await settle(300);
    });
    expect(screen.queryByRole("slider")).toBeNull();
  });

  it("on a phone, puts the inbox first", async () => {
    narrow();
    setup();
    await flush();
    const links = within(nav()).getAllByRole("link");
    expect(links.map((l) => l.textContent)).toEqual([
      "Inbox 1",
      "Smartlog",
      "Workspaces",
      "Activity",
    ]);
  });

  it("replaces the window when the repository changes", async () => {
    const { oplog } = setup();
    await flush();
    await act(async () => {
      navigate(`/repos/other/activity`);
      await settle(300);
    });
    expect(oplog.listCalls.filter((c) => !c.atOperation)).toHaveLength(2);
  });
});
