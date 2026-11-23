import { useEffect, useState } from "react";
import type { Connection } from "../api/connection";
import { AppBar } from "../ui/AppBar";
import { PairForm } from "./PairForm";

/** Whether the page runs as a Home Screen app, whose storage is its own. */
function standalone(): boolean {
  return (
    (navigator as { standalone?: boolean }).standalone === true ||
    (typeof matchMedia === "function" && matchMedia("(display-mode: standalone)").matches)
  );
}

/**
 * Over the remote listener (R2.08) the page is https: a phone pairs here
 * (R3.40). From a browser's tab the Home Screen steps come first, since
 * what a tab stores never reaches the app.
 */
function RemoteSession({
  onPaired,
  connect,
}: {
  onPaired: (token: string) => void;
  connect?: ((code: string) => Connection) | undefined;
}) {
  const [here, setHere] = useState(standalone);
  return (
    <div className="card center-card">
      <h1>Not connected</h1>
      {here ? (
        <PairForm onPaired={onPaired} connect={connect} />
      ) : (
        <>
          <p>To use Reef on a phone, put it on the Home Screen and pair it there:</p>
          <ol>
            <li>
              Tap <strong>Share</strong>, or the browser&apos;s menu on Android.
            </li>
            <li>
              Tap <strong>Add to Home Screen</strong>.
            </li>
            <li>Open Reef from the Home Screen, and pair it there.</li>
          </ol>
          <p>
            <button
              type="button"
              className="link-button"
              onClick={() => {
                setHere(true);
              }}
            >
              Pair this browser instead
            </button>
          </p>
        </>
      )}
      <p className="secondary">
        If you came here by a link, its token is revoked, expired or mistyped: ask the daemon&apos;s
        owner for a new one.
      </p>
    </div>
  );
}

export function NoSession({
  onPaired = () => undefined,
  connect,
  remote = window.location.protocol === "https:",
}: {
  onPaired?: ((token: string) => void) | undefined;
  connect?: ((code: string) => Connection) | undefined;
  remote?: boolean;
}) {
  useEffect(() => {
    document.title = "Not connected — Reef";
  }, []);
  return (
    <div className="page">
      <AppBar live={null} status={null} />
      <main>
        {remote ? (
          <RemoteSession onPaired={onPaired} connect={connect} />
        ) : (
          <div className="card center-card">
            <h1>Not connected</h1>
            <p>
              This page has no session with the daemon. Run <code>reef ui</code> in a terminal.
            </p>
          </div>
        )}
      </main>
    </div>
  );
}
