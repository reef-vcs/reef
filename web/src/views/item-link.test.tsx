import { afterEach, describe, expect, it } from "vitest";
import { InboxReason } from "../gen/reef/api/v1/inbox_pb";
import { InboxStore } from "../live/inbox";
import type { Live } from "../live/share";
import { immediateFrames } from "../test/fakes";
import { helpItem, InboxDaemon, proposalItem, REPO, section } from "../test/inbox-daemon";
import { cleanup, render, screen } from "../test/testing-library";
import { inboxHref, itemHref } from "./inbox-route";
import { ItemLink } from "./ItemLink";
import { proposalHref } from "./proposal-route";

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
});

function at(daemon: InboxDaemon, key: string) {
  const inbox = new InboxStore(immediateFrames);
  inbox.beginResync();
  inbox.applyFrame(daemon.frame());
  window.history.replaceState(null, "", itemHref({ repo: REPO, key }));
  render(<ItemLink live={{ inbox } as unknown as Live} repo={REPO} itemKey={key} />);
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
    expect(window.location.pathname).toBe(inboxHref({ repo: REPO }));
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
});
