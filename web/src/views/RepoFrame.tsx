// The repository pages' frame (R2.21): one operation window per repository,
// the shared header over it, and the page. Keyed by the repository alone, so
// the window survives moving between the pages and moving `at`. Its own
// chunk, as each page is: the overview never loads it.
import { useEffect, useState } from "react";
import type { Connection } from "../api/connection";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { OperationWindow } from "../log/window";
import { lazyRoute } from "../ui/lazy-route";
import { RepoHeader } from "../ui/RepoHeader";
import type { RepoPage } from "../ui/RepoNav";
import { activityParams } from "./activity-route";
import { fleetParams } from "./fleet-route";
import { smartlogParams } from "./smartlog-route";

const Smartlog = lazyRoute(() => import("./Smartlog").then((m) => m.Smartlog));
const Fleet = lazyRoute(() => import("./Fleet").then((m) => m.Fleet));
const Activity = lazyRoute(() => import("./Activity").then((m) => m.Activity));
const Proposals = lazyRoute(() => import("./Proposals").then((m) => m.Proposals));
const Proposal = lazyRoute(() => import("./Proposal").then((m) => m.Proposal));

export function RepoFrame({
  repo,
  page,
  change,
  search,
  connection,
  live,
  frame,
  onSessionInvalid,
}: {
  repo: string;
  page: RepoPage;
  /** On the proposals page, the proposal it opens; empty for the list. */
  change: string;
  search: string;
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
}) {
  const [win] = useState(
    () => new OperationWindow(connection, live.operations, frame, onSessionInvalid, repo),
  );
  useEffect(() => win.start(), [win]);
  const at = new URLSearchParams(search).get("at") ?? "";
  const shared = { connection, live, frame, onSessionInvalid };
  let body;
  if (page === "smartlog") {
    const params = smartlogParams(repo, search);
    // A new operation is a new view; a new revset, limit or selection is
    // not, so a bad revset keeps the last good graph.
    body = <Smartlog key={params.at} {...shared} params={params} />;
  } else if (page === "workspaces") {
    const params = fleetParams(repo, search);
    // A new operation is a new page; a new selection is not.
    body = <Fleet key={params.at} {...shared} params={params} />;
  } else if (page === "proposals") {
    // Proposals are read live: `ListProposals` has no `at_operation`.
    body = change ? (
      <Proposal key={change} {...shared} repo={repo} change={change} />
    ) : (
      <Proposals {...shared} repo={repo} at={at} />
    );
  } else {
    // The feed follows `at` itself, from the window it shares with the scrubber.
    body = <Activity {...shared} window={win} params={activityParams(repo, search)} />;
  }
  return (
    <>
      <RepoHeader
        repo={repo}
        current={page}
        at={at}
        window={win}
        connection={connection}
        onSessionInvalid={onSessionInvalid}
      />
      {body}
    </>
  );
}
