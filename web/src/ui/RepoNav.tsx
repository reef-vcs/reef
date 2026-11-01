// The repository pages' nav (R2.20, in the shared header since R2.21):
// "Smartlog · Workspaces · Activity", the current one marked. A read at an
// operation keeps its `at` across the three.
import { activityHref } from "../views/activity-route";
import { fleetHref } from "../views/fleet-route";
import { smartlogHref } from "../views/smartlog-route";
import { Link } from "./router";

export type RepoPage = "smartlog" | "workspaces" | "activity";

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
        Smartlog
      </Link>
      {" · "}
      <Link
        to={fleetHref({ repo, at: at ?? "" })}
        current={current === "workspaces" ? "page" : undefined}
      >
        Workspaces
      </Link>
      {" · "}
      <Link
        to={activityHref({ repo, at: at ?? "" })}
        current={current === "activity" ? "page" : undefined}
      >
        Activity
      </Link>
    </nav>
  );
}
