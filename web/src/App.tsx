// The app shell: the app bar and the routes over one connection and one live
// layer. Without a session, or once the daemon refuses it, every route is
// the no-session page. The overview is in the entry chunk; every other page
// is its own (R2.21), and the three repository pages share a frame keyed by
// the repository.
import { useEffect, useMemo, useState } from "react";
import type { Connection } from "./api/connection";
import { isSessionInvalid, reefError, type ReefError } from "./api/errors";
import type { GetDaemonStatusResponse } from "./gen/reef/api/v1/daemon_pb";
import { animationFrame, type FrameScheduler } from "./live/frame";
import { Live, type LiveEnvironment } from "./live/share";
import { endSession } from "./session";
import { AppBar } from "./ui/AppBar";
import { ErrorView } from "./ui/ErrorView";
import { HeaderSlots } from "./ui/header-slots";
import { lazyRoute } from "./ui/lazy-route";
import type { RepoPage } from "./ui/RepoNav";
import { match, usePath, useSearch } from "./ui/router";
import { NoSession } from "./views/NoSession";
import { NotFound } from "./views/NotFound";
import { Overview } from "./views/Overview";

const RepoFrame = lazyRoute(() => import("./views/RepoFrame").then((m) => m.RepoFrame));
const States = lazyRoute(() => import("./views/States").then((m) => m.States));

const REPO_PAGES: [string, RepoPage][] = [
  ["/repos/:repo/smartlog", "smartlog"],
  ["/repos/:repo/workspaces", "workspaces"],
  ["/repos/:repo/activity", "activity"],
];

/** The repository page `path` names, if any. */
function repoRoute(path: string): { repo: string; page: RepoPage } | null {
  for (const [pattern, page] of REPO_PAGES) {
    const params = match(pattern, path);
    if (params?.repo) return { repo: params.repo, page };
  }
  return null;
}

function useDaemonStatus(connection: Connection, onSessionInvalid: () => void) {
  const [status, setStatus] = useState<GetDaemonStatusResponse | null>(null);
  const [error, setError] = useState<ReefError | null>(null);
  useEffect(() => {
    let current = true;
    connection.clients.daemon.getDaemonStatus({}).then(
      (response) => {
        if (current) setStatus(response);
      },
      (thrown: unknown) => {
        if (!current) return;
        const error = reefError(thrown);
        if (isSessionInvalid(error)) onSessionInvalid();
        else setError(error);
      },
    );
    return () => {
      current = false;
    };
  }, [connection, onSessionInvalid]);
  return { status, error };
}

function Shell({
  connection,
  live,
  frame,
  onSessionInvalid,
}: {
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
}) {
  const path = usePath();
  const search = useSearch();
  const { status, error } = useDaemonStatus(connection, onSessionInvalid);
  const repo = repoRoute(path);
  // The app bar's slots, for the repository frame's portals (ui/header-slots).
  const [crumb, setCrumb] = useState<HTMLElement | null>(null);
  const [tabs, setTabs] = useState<HTMLElement | null>(null);
  const [strip, setStrip] = useState<HTMLElement | null>(null);
  const slots = useMemo(() => ({ crumb, tabs, strip }), [crumb, tabs, strip]);
  // The repository frame titles its own pages, once it knows the name.
  const inRepo = repo !== null;
  useEffect(() => {
    if (inRepo) return;
    document.title =
      path === "/" ? "Reef" : path === "/states" ? "States — Reef" : "Not found — Reef";
  }, [inRepo, path]);
  return (
    <HeaderSlots.Provider value={slots}>
      <div className={repo ? "page repo-page" : "page"}>
        <AppBar
          live={live}
          status={status}
          onCrumb={setCrumb}
          onTabs={setTabs}
          onStrip={setStrip}
        />
        <main>
          {error && <ErrorView error={error} />}
          {path === "/" ? (
            <Overview
              connection={connection}
              live={live}
              frame={frame}
              onSessionInvalid={onSessionInvalid}
            />
          ) : repo ? (
            <RepoFrame
              key={repo.repo}
              repo={repo.repo}
              page={repo.page}
              search={search}
              connection={connection}
              live={live}
              frame={frame}
              onSessionInvalid={onSessionInvalid}
            />
          ) : path === "/states" ? (
            <States />
          ) : (
            <NotFound />
          )}
        </main>
      </div>
    </HeaderSlots.Provider>
  );
}

export function App({
  session,
  environment,
}: {
  /** Null when the tab has no session. */
  session: { connection: Connection; token: string } | null;
  environment?: LiveEnvironment;
}) {
  const connection = session?.connection ?? null;
  const [ended, setEnded] = useState(session === null);
  const [live] = useState(() => (connection ? new Live(connection, environment) : null));
  const [end] = useState(() => () => {
    // Stop the streams too: an ended tab must not keep the lock and feed
    // other tabs on a refused token.
    live?.close();
    if (session) endSession(session.token);
    setEnded(true);
  });

  useEffect(() => {
    if (!live) return;
    const unsubscribe = live.onEnded(end);
    live.start();
    return () => {
      unsubscribe();
      live.close();
    };
  }, [live, end]);

  if (ended || !connection || !live) return <NoSession />;
  return (
    <Shell
      connection={connection}
      live={live}
      frame={environment?.frame ?? animationFrame}
      onSessionInvalid={end}
    />
  );
}
