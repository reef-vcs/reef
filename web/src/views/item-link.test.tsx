import { afterEach, describe, expect, it } from "vitest";
import { create } from "@bufbuild/protobuf";
import { InboxReason, WatchInboxResponseSchema } from "../gen/reef/api/v1/inbox_pb";
import { InboxStore } from "../live/inbox";
import type { Live } from "../live/share";
import { immediateFrames } from "../test/fakes";
import { helpItem, InboxDaemon, proposalItem, REPO, section } from "../test/inbox-daemon";
import { act, cleanup, render, screen } from "../test/testing-library";
import { inboxHref, itemHref } from "./inbox-route";
import { ItemLink } from "./ItemLink";
import { proposalHref } from "./proposal-route";

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
});

function at(daemon: InboxDaemon, key: string, state = "live") {
  const inbox = new InboxStore(immediateFrames);
  let listener = () => {};
  const live = {
    inbox,
    state,
    subscribeState: (run: () => void) => {
      listener = run;
      return () => undefined;
    },
  };
  inbox.beginResync();
  inbox.applyFrame(daemon.frame());
  window.history.replaceState(null, "", itemHref({ repo: REPO, key }));
  render(<ItemLink live={live as unknown as Live} repo={REPO} itemKey={key} />);
  return {
    inbox,
    become: (next: string) => {
      live.state = next;
      listener();
    },
  };
}

const CHANGE = "d".repeat(32);

describe("an inbox item's link", () => {
  it("goes on to a proposal's page", () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, {
        needsYouNow: [proposalItem(CHANGE, InboxReason.SIGNOFF, 2_000, "Needs a sign-off")],
        needsYouNowTotal: 1,
      }),
    ];
    at(daemon, `proposal:${REPO}:${CHANGE}`);
    expect(window.location.pathname).toBe(proposalHref({ repo: REPO, change: CHANGE }));
  });

  it("opens a help request in its repository's inbox, where it is answered", () => {
    const daemon = new InboxDaemon();
    daemon.sections = [
      section(REPO, { needsYouNow: [helpItem("h1", 2_000)], needsYouNowTotal: 1 }),
    ];
    at(daemon, `help:${REPO}:h1`);
    expect(window.location.pathname + window.location.search).toBe(
      inboxHref({ repo: REPO, item: `help:${REPO}:h1` }),
    );
  });

  it("says when the item no longer needs you", () => {
    const daemon = new InboxDaemon();
    daemon.sections = [section(REPO)];
    at(daemon, `help:${REPO}:gone`);
    expect(screen.getByText("That no longer needs you.")).toBeTruthy();
    expect(screen.getByRole("link", { name: "Open the inbox" }).getAttribute("href")).toBe(
      inboxHref({ repo: REPO }),
    );
  });

  it("says so only of an inbox that is current", async () => {
    const daemon = new InboxDaemon();
    daemon.sections = [section(REPO)];
    const { inbox, become } = at(daemon, `help:${REPO}:h1`, "reconnecting");
    const gone = () => screen.queryByText("That no longer needs you.");
    expect(gone()).toBeNull();
    expect(screen.getByText("Finding it in the inbox.")).toBeTruthy();
    await act(() => {
      inbox.beginResync();
      become("live");
    });
    expect(gone()).toBeNull();
    await act(() => {
      inbox.applyFrame(create(WatchInboxResponseSchema, { synced: true }));
    });
    expect(gone()).not.toBeNull();
  });
});
