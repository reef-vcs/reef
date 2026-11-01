// The app bar (the design pass): the one sticky header. The mark and "Reef"
// at the left, then the slots a repository frame fills (its name, its tabs),
// then the connection and settings at the right; the scrubber's strip sits
// below the bar. In developer mode a pill says so, with the daemon's version.
import type { GetDaemonStatusResponse } from "../gen/reef/api/v1/daemon_pb";
import type { Live } from "../live/share";
import { ConnectionIndicator } from "./ConnectionIndicator";
import { useDevMode } from "./dev-mode";
import { ReefMark } from "./ReefMark";
import { Link } from "./router";
import { Settings } from "./Settings";

export function AppBar({
  live,
  status,
  onCrumb,
  onTabs,
  onStrip,
}: {
  /** Null on the no-session page, which shows the brand alone. */
  live: Live | null;
  status: GetDaemonStatusResponse | null;
  /** The slots' elements, for the repository frame's portals; stable setters. */
  onCrumb?: (element: HTMLElement | null) => void;
  onTabs?: (element: HTMLElement | null) => void;
  onStrip?: (element: HTMLElement | null) => void;
}) {
  const dev = useDevMode();
  return (
    <header className="app-header">
      <div className="appbar">
        <Link to="/" className="brand">
          <ReefMark />
          Reef
        </Link>
        <div className="appbar-crumb" ref={onCrumb} />
        <div className="appbar-tabs" ref={onTabs} />
        {live && (
          <div className="appbar-end">
            {dev && <span className="dev-pill">Developer mode</span>}
            {dev && status && (
              <span className="appbar-version">
                reef-daemon {status.version} · API revision {status.apiRevision}
              </span>
            )}
            <ConnectionIndicator live={live} />
            <Settings status={status} />
          </div>
        )}
      </div>
      <div className="appbar-strip" ref={onStrip} />
    </header>
  );
}
