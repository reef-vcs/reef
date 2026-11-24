// The header of the repository pages (R2.21, in the app bar since the design
// pass): the repository's name and the tabs in the bar, and, on a page that
// reads at an operation, the scrubber in the strip below it, over the
// repository's operation window.
import { useEffect, useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import type { Live } from "../live/share";
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
  inbox: "Inbox",
  proposal: "Proposal",
};

/** The pages that read at an operation; the inbox and a proposal are live (R3.40). */
const AT_OPERATION: ReadonlySet<RepoPage> = new Set(["smartlog", "workspaces", "activity"]);

/** A repository's name: the last component of its root. */
export function repoName(root: string | undefined, repo: string): string {
  return root ? root.replace(/\/+$/, "").split("/").pop() || root : shortId(repo);
}

/** How many items of `repo`, an id, need the person now. */
function useNeeds(live: Live, repo: string): number {
  return useSyncExternalStore(
    (listener) => live.inbox.subscribe(listener),
    () => live.inbox.sections.get(repo)?.needsYouNowTotal ?? 0,
  );
}

export function RepoHeader({
  repo,
  current,
  at,
  window: win,
  connection,
  live,
  onSessionInvalid,
}: {
  repo: string;
  current: RepoPage;
  at: string;
  window: OperationWindow;
  connection: Connection;
  live: Live;
  onSessionInvalid: () => void;
}) {
  const { fatal, repository } = useSyncExternalStore(win.subscribe, win.getSnapshot);
  const needs = useNeeds(live, repository?.id ?? repo);
  const name = repoName(repository?.root, repo);
  useEffect(() => {
    // The inbox titles itself, with its count (inbox/needs.ts).
    if (current === "inbox") return;
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
        <RepoNav repo={repo} current={current} at={at} needs={needs} />
      </InSlot>
      {!fatal && AT_OPERATION.has(current) && (
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
