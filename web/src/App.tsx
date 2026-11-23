// The app shell: the app bar and the routes over one connection and one live
// layer. Without a session, or once the daemon refuses it, every route is
// the no-session page. The home page is the inbox across every repository
// (R3.33) above the overview, which is in the entry chunk; every other page
// is its own (R2.21), and the repository pages share a frame keyed by the
// repository.
import { useEffect, useMemo, useState } from "react";
import { client, type Connection } from "./api/connection";
import { isSessionInvalid, REMOTE_ORIGIN_CHANGED, reefError, type ReefError } from "./api/errors";
import { device, endDevice } from "./device";
import { DaemonService, type GetDaemonStatusResponse } from "./gen/reef/api/v1/daemon_pb";
import { animationFrame, type FrameScheduler } from "./live/frame";
import { Live, type LiveEnvironment } from "./live/share";
import { endSession, SESSION_KEY, storedSession } from "./session";
import { AppBar } from "./ui/AppBar";
import { ErrorView } from "./ui/ErrorView";
import { HeaderSlots } from "./ui/header-slots";
import { lazyRoute } from "./ui/lazy-route";
import type { RepoPage } from "./ui/RepoNav";
import { match, navigate, usePath, useSearch } from "./ui/router";
import { inboxHref } from "./views/inbox-route";
import { ItemLink } from "./views/ItemLink";
import { NoSession } from "./views/NoSession";
import { NotFound } from "./views/NotFound";
import { Overview } from "./views/Overview";

const RepoFrame = lazyRoute(() => import("./views/RepoFrame").then((m) => m.RepoFrame));
const States = lazyRoute(() => import("./views/States").then((m) => m.States));
const Passkeys = lazyRoute(() => import("./views/Passkeys").then((m) => m.Passkeys));
const Approve = lazyRoute(() => import("./views/Approve").then((m) => m.Approve));

const REPO_PAGES: [string, RepoPage][] = [
  ["/repos/:repo/smartlog", "smartlog"],
  ["/repos/:repo/workspaces", "workspaces"],
  ["/repos/:repo/activity", "activity"],
  ["/repos/:repo/inbox", "inbox"],
  ["/repos/:repo/proposals", "inbox"],
  ["/repos/:repo/proposals/:change", "proposal"],
];

const PAGE_TITLES: Record<string, string> = { "/states": "States", "/passkeys": "Passkeys" };

/** Pages that set the title themselves: the home page is the inbox, with its count. */
const SELF_TITLED = new Set(["/"]);

/** The repository page `path` names, if any, and the proposal it opens. */
function repoRoute(path: string): { repo: string; page: RepoPage; change: string } | null {
  for (const [pattern, page] of REPO_PAGES) {
    const params = match(pattern, path);
    if (params?.repo) return { repo: params.repo, page, change: params.change ?? "" };
  }
  return null;
}

function useDaemonStatus(connection: Connection, onSessionInvalid: () => void) {
  const [status, setStatus] = useState<GetDaemonStatusResponse | null>(null);
  const [error, setError] = useState<ReefError | null>(null);
  useEffect(() => {
    let current = true;
    client(connection, DaemonService)
      .getDaemonStatus({})
      .then(
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
  onUnpair,
}: {
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
  onUnpair: () => void;
}) {
  const path = usePath();
  const search = useSearch();
  const { status, error } = useDaemonStatus(connection, onSessionInvalid);
  const repo = repoRoute(path);
  const approval = match("/approve/:id", path)?.id;
  const item = match("/repos/:repo/items/:key", path);
  const isItem = item !== null;
  const oldTab = match("/repos/:repo/proposals", path)?.repo;
  useEffect(() => {
    if (oldTab) navigate(inboxHref({ repo: oldTab }), { replace: true });
  }, [oldTab]);
  // The app bar's slots, for the repository frame's portals (ui/header-slots).
  const [crumb, setCrumb] = useState<HTMLElement | null>(null);
  const [tabs, setTabs] = useState<HTMLElement | null>(null);
  const [strip, setStrip] = useState<HTMLElement | null>(null);
  const slots = useMemo(() => ({ crumb, tabs, strip }), [crumb, tabs, strip]);
  // The repository frame titles its own pages, once it knows the name.
  const inRepo = repo !== null;
  useEffect(() => {
    if (inRepo || isItem || SELF_TITLED.has(path)) return;
    const page = PAGE_TITLES[path] ?? (approval ? "Approve a sign-off" : "Not found");
    document.title = `${page} — Reef`;
  }, [inRepo, isItem, path, approval]);
  return (
    <HeaderSlots.Provider value={slots}>
      <div className={repo ? "page repo-page" : "page"}>
        <AppBar
          live={live}
          status={status}
          onUnpair={onUnpair}
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
          ) : item?.repo && item.key ? (
            <ItemLink key={path} live={live} repo={item.repo} itemKey={item.key} />
          ) : repo ? (
            <RepoFrame
              key={repo.repo}
              repo={repo.repo}
              page={repo.page}
              change={repo.change}
              search={search}
              connection={connection}
              live={live}
              frame={frame}
              onSessionInvalid={onSessionInvalid}
            />
          ) : path === "/states" ? (
            <States />
          ) : path === "/passkeys" ? (
            <Passkeys connection={connection} onSessionInvalid={onSessionInvalid} />
          ) : approval ? (
            <Approve
              key={approval}
              connection={connection}
              id={approval}
              onSessionInvalid={onSessionInvalid}
            />
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
  onPaired,
}: {
  /** Null when the tab has no session. */
  session: { connection: Connection; token: string } | null;
  environment?: LiveEnvironment;
  /** This device was paired (R3.40): shows the app with its token. */
  onPaired?: (token: string) => void;
}) {
  const connection = session?.connection ?? null;
  const [ended, setEnded] = useState(session === null);
  const [moved, setMoved] = useState<ReefError | null>(null);
  const [live] = useState(() => (connection ? new Live(connection, environment) : null));
  function forget() {
    if (!session) return;
    endDevice(session.token);
    endSession(session.token);
  }
  const [end] = useState(() => () => {
    // Stop the streams too: an ended tab must not keep the lock and feed
    // other tabs on a refused token.
    live?.close();
    const refused = session?.connection.refused;
    if (refused?.reason === REMOTE_ORIGIN_CHANGED) setMoved(refused);
    else forget();
    setEnded(true);
  });
  const [unpair] = useState(() => () => {
    live?.close();
    forget();
    setEnded(true);
  });

  // A phone unpaired in one tab is unpaired in all of them.
  useEffect(() => {
    if (!device()) return;
    const onStorage = (event: StorageEvent) => {
      if (event.key === null || event.key === SESSION_KEY) {
        if (storedSession() === null) unpair();
      }
    };
    window.addEventListener("storage", onStorage);
    return () => {
      window.removeEventListener("storage", onStorage);
    };
  }, [unpair]);

  useEffect(() => {
    if (!live) return;
    const unsubscribe = live.onEnded(end);
    live.start();
    return () => {
      unsubscribe();
      live.close();
    };
  }, [live, end]);

  if (ended || !connection || !live) return <NoSession onPaired={onPaired} moved={moved} />;
  return (
    <Shell
      connection={connection}
      live={live}
      frame={environment?.frame ?? animationFrame}
      onSessionInvalid={end}
      onUnpair={unpair}
    />
  );
}
