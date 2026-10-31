// Every error the app shows carries the daemon's hint and its `next`
// commands, which can be copied.
import { useState } from "react";
import type { ReefError } from "../api/errors";

function Copy({ text }: { text: string }) {
  const [copied, setCopied] = useState(false);
  return (
    <button
      type="button"
      onClick={() => {
        navigator.clipboard.writeText(text).then(
          () => {
            setCopied(true);
          },
          () => {
            setCopied(false);
          },
        );
      }}
    >
      {copied ? "Copied" : "Copy"}
    </button>
  );
}

export function ErrorView({ error }: { error: ReefError }) {
  return (
    <div className="error" role="alert">
      <p>{error.message || "The daemon did not answer."}</p>
      {error.hint && <p className="secondary">{error.hint}</p>}
      {error.next.map((command) => (
        <p className="next" key={command}>
          <code>{command}</code>
          <Copy text={command} />
        </p>
      ))}
    </div>
  );
}
