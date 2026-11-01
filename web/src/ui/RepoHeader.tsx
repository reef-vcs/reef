// The header of the three repository pages (R2.21, in the app bar since the
// design pass): the repository's name and the tabs in the bar, and the
// scrubber in the strip below it, over the repository's operation window.
import { useEffect, useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import type { OperationWindow } from "../log/window";
import { smartlogHref } from "../views/smartlog-route";
import { InSlot } from "./header-slots";
import { ChevronIcon } from "./icons";
import { shortId } from "./ids";
import { RepoNav, type RepoPage } from "./RepoNav";
import { Link } from "./router";
import { Scrubber } from "./Scrubber";

const PAGE_TITLES: Record<RepoPage, string> = {
  smartlog: "Smartlog",
  workspaces: "Workspaces",
  activity: "Activity",
};

/** A repository's name: the last component of its root. */
export function repoName(root: string | undefined, repo: string): string {
  return root ? root.replace(/\/+$/, "").split("/").pop() || root : shortId(repo);
}

export function RepoHeader({
  repo,
  current,
  at,
  window: win,
  connection,
  onSessionInvalid,
}: {
  repo: string;
  current: RepoPage;
  at: string;
  window: OperationWindow;
  connection: Connection;
  onSessionInvalid: () => void;
}) {
  const { fatal, repository } = useSyncExternalStore(win.subscribe, win.getSnapshot);
  const name = repoName(repository?.root, repo);
  useEffect(() => {
    document.title = `${name} · ${PAGE_TITLES[current]} — Reef`;
  }, [name, current]);
  return (
    <div className="repo-header">
      <InSlot slot="crumb">
        <ChevronIcon />
        <Link to={smartlogHref({ repo, at })} className="crumb" title={repository?.root}>
          {name}
        </Link>
      </InSlot>
      <InSlot slot="tabs">
        <RepoNav repo={repo} current={current} at={at} />
      </InSlot>
      {!fatal && (
        <InSlot slot="strip">
          <Scrubber
            window={win}
            at={at}
            connection={connection}
            onSessionInvalid={onSessionInvalid}
          />
        </InSlot>
      )}
    </div>
  );
}
