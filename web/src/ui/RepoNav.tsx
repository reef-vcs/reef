// The repository pages' nav (R2.20): "Smartlog · Workspaces", the current
// one marked. A read at an operation keeps its `at` across the two.
import { fleetHref } from "../views/fleet-route";
import { smartlogHref } from "../views/smartlog-route";
import { Link } from "./router";

export function RepoNav({
  repo,
  current,
  at,
}: {
  repo: string;
  current: "smartlog" | "workspaces";
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
    </nav>
  );
}
