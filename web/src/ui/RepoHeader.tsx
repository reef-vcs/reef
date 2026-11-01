// The header of the three repository pages (R2.21): the nav, and the
// scrubber below it, over the repository's operation window.
import { useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import type { OperationWindow } from "../log/window";
import { RepoNav, type RepoPage } from "./RepoNav";
import { Scrubber } from "./Scrubber";

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
  const { fatal } = useSyncExternalStore(win.subscribe, win.getSnapshot);
  return (
    <div className="repo-header">
      <RepoNav repo={repo} current={current} at={at} />
      {!fatal && (
        <Scrubber
          window={win}
          at={at}
          connection={connection}
          onSessionInvalid={onSessionInvalid}
        />
      )}
    </div>
  );
}
