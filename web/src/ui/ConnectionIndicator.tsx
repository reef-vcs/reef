// The streams' state as a word and a shape in neutral colors: state colors
// are for object states only, so green always means landed.
import { useSyncExternalStore } from "react";
import type { ConnectionState, Live } from "../live/share";

const WORDS: Record<ConnectionState, string> = {
  connecting: "Connecting",
  live: "Live",
  reconnecting: "Reconnecting",
  ended: "Session ended",
};

function Shape({ state }: { state: ConnectionState }) {
  switch (state) {
    case "live":
      return <circle cx="5" cy="5" r="4" fill="currentColor" />;
    case "connecting":
    case "reconnecting":
      return <circle cx="5" cy="5" r="3.5" fill="none" stroke="currentColor" strokeWidth={1.5} />;
    case "ended":
      return <path d="M2 2 8 8M8 2 2 8" stroke="currentColor" strokeWidth={1.5} />;
  }
}

export function ConnectionIndicator({ live }: { live: Live }) {
  const state = useSyncExternalStore(
    (listener) => live.subscribeState(listener),
    () => live.state,
  );
  return (
    <span className="connection" aria-live="polite" data-connection={state}>
      <svg width="10" height="10" viewBox="0 0 10 10" aria-hidden="true" focusable="false">
        <Shape state={state} />
      </svg>
      {WORDS[state]}
    </span>
  );
}
