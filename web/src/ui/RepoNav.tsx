// The repository pages' tabs (R2.20; in the app bar since the design pass):
// Smartlog, Workspaces, Activity and Proposals (R3.31), the current one
// marked. A read at an operation keeps its `at` across the first three;
// proposals are read live.
import { activityHref } from "../views/activity-route";
import { fleetHref } from "../views/fleet-route";
import { smartlogHref } from "../views/smartlog-route";
import { proposalsHref } from "../views/proposal-route";
import { GraphIcon, InboxIcon, LayersIcon, PulseIcon } from "./icons";
import { Link } from "./router";

export type RepoPage = "smartlog" | "workspaces" | "activity" | "proposals";

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
      <Link to={proposalsHref({ repo })} current={current === "proposals" ? "page" : undefined}>
        <InboxIcon />
        Proposals
      </Link>
    </nav>
  );
}
