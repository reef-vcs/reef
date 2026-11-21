// The repository pages' tabs (R2.20; in the app bar since the design pass):
// Smartlog, Workspaces, Activity and Inbox (R3.33, in place of R3.31's
// Proposals), the current one marked; a proposal's page is under the Inbox.
// A read at an operation keeps its `at` across the first three; the inbox
// is read live.
import { activityHref } from "../views/activity-route";
import { fleetHref } from "../views/fleet-route";
import { smartlogHref } from "../views/smartlog-route";
import { inboxHref } from "../views/inbox-route";
import { GraphIcon, InboxIcon, LayersIcon, PulseIcon } from "./icons";
import { Link } from "./router";

export type RepoPage = "smartlog" | "workspaces" | "activity" | "inbox" | "proposal";

export function RepoNav({
  repo,
  current,
  at,
}: {
  repo: string;
  current: RepoPage;
  /** Empty when live. */
  at?: string;
}) {
  return (
    <nav className="repo-nav" aria-label="Repository">
      <Link
        to={smartlogHref({ repo, at: at ?? "" })}
        current={current === "smartlog" ? "page" : undefined}
      >
        <GraphIcon />
        Smartlog
      </Link>
      <Link
        to={fleetHref({ repo, at: at ?? "" })}
        current={current === "workspaces" ? "page" : undefined}
      >
        <LayersIcon />
        Workspaces
      </Link>
      <Link
        to={activityHref({ repo, at: at ?? "" })}
        current={current === "activity" ? "page" : undefined}
      >
        <PulseIcon />
        Activity
      </Link>
      <Link
        to={inboxHref({ repo })}
        current={current === "inbox" || current === "proposal" ? "page" : undefined}
      >
        <InboxIcon />
        Inbox
      </Link>
    </nav>
  );
}
