export function NoSession() {
  // Over the remote listener (R2.08) the page is https and its token came from
  // `reef remote token create`; on loopback it came from `reef ui`.
  if (window.location.protocol === "https:") {
    return (
      <main className="page">
        <h1>Reef</h1>
        <p>
          This page&apos;s token is not valid for this daemon (revoked, expired or mistyped). Ask
          the daemon&apos;s owner for a new link.
        </p>
      </main>
    );
  }
  return (
    <main className="page">
      <h1>Reef</h1>
      <p>
        This page has no session with the daemon. Run <code>reef ui</code> in a terminal.
      </p>
    </main>
  );
}
