// Every error the app shows carries the daemon's hint and its `next`
// commands, which can be copied.
import type { ReefError } from "../api/errors";
import { Copy } from "./Copy";

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
