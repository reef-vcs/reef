// The app shell: header, routes and footer over one connection and one live
// layer. Without a session, or once the daemon refuses it, every route is
// the no-session page.
import { useEffect, useState } from "react";
import type { Connection } from "./api/connection";
import { isSessionInvalid, reefError, type ReefError } from "./api/errors";
import type { GetDaemonStatusResponse } from "./gen/reef/api/v1/daemon_pb";
import { animationFrame, type FrameScheduler } from "./live/frame";
import { Live, type LiveEnvironment } from "./live/share";
import { endSession } from "./session";
import { ConnectionIndicator } from "./ui/ConnectionIndicator";
import { ErrorView } from "./ui/ErrorView";
import { Link, match, usePath, useSearch } from "./ui/router";
import { Fleet } from "./views/Fleet";
import { fleetParams } from "./views/fleet-route";
import { NoSession } from "./views/NoSession";
import { NotFound } from "./views/NotFound";
import { Overview } from "./views/Overview";
import { Smartlog, smartlogParams } from "./views/Smartlog";
import { States } from "./views/States";

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
  const smartlog = match("/repos/:repo/smartlog", path);
  const smartlogQuery = smartlog && smartlogParams(smartlog.repo ?? "", search);
  const fleet = match("/repos/:repo/workspaces", path);
  const fleetQuery = fleet && fleetParams(fleet.repo ?? "", search);
  return (
    <div className="page">
      <header className="header">
        <h1>
          <Link to="/">Reef</Link>
        </h1>
        {status && (
          <span className="daemon">
            reef-daemon {status.version} · API revision {status.apiRevision}
          </span>
        )}
        <ConnectionIndicator live={live} />
      </header>
      <main>
        {error && <ErrorView error={error} />}
        {path === "/" ? (
          <Overview
            connection={connection}
            live={live}
            frame={frame}
            onSessionInvalid={onSessionInvalid}
          />
        ) : smartlogQuery ? (
          // A new operation or repository is a new view; a new revset, limit
          // or selection is not, so a bad revset keeps the last good graph.
          <Smartlog
            key={`${smartlogQuery.repo}\n${smartlogQuery.at}`}
            connection={connection}
            live={live}
            frame={frame}
            onSessionInvalid={onSessionInvalid}
            params={smartlogQuery}
          />
        ) : fleetQuery ? (
          // A new operation or repository is a new page; a new selection is not.
          <Fleet
            key={`${fleetQuery.repo}\n${fleetQuery.at}`}
            connection={connection}
            live={live}
            frame={frame}
            onSessionInvalid={onSessionInvalid}
            params={fleetQuery}
          />
        ) : path === "/states" ? (
          <States />
        ) : (
          <NotFound />
        )}
      </main>
      <footer className="footer">
        <Link to="/states">States</Link>
        <a href="/THIRD_PARTY_LICENSES.txt">Third-party licenses</a>
      </footer>
    </div>
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
