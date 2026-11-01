import { useEffect } from "react";
import { AppBar } from "../ui/AppBar";

export function NoSession() {
  // Over the remote listener (R2.08) the page is https and its token came from
  // `reef remote token create`; on loopback it came from `reef ui`.
  const remote = window.location.protocol === "https:";
  useEffect(() => {
    document.title = "Not connected — Reef";
  }, []);
  return (
    <div className="page">
      <AppBar live={null} status={null} />
      <main>
        <div className="card center-card">
          <h1>Not connected</h1>
          {remote ? (
            <p>
              This page&apos;s token is not valid for this daemon (revoked, expired or mistyped).
              Ask the daemon&apos;s owner for a new link.
            </p>
          ) : (
            <p>
              This page has no session with the daemon. Run <code>reef ui</code> in a terminal.
            </p>
          )}
        </div>
      </main>
    </div>
  );
}
