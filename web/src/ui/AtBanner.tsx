// The banner of a view read at an operation (R2.19, shared since R2.20): what
// the point is, that it is not live, and a link back to live.
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import { shortId } from "./ids";
import { operationLabel } from "./operation";
import { Link } from "./router";
import { clock } from "./time";

export function AtBanner({
  operation,
  at,
  live,
}: {
  /** Once looked up. */
  operation: Operation | undefined;
  /** As the route gave it, until then. */
  at: string;
  /** The same view, live. */
  live: string;
}) {
  return (
    <div className="banner" role="status">
      At operation <span className="mono">{shortId(operation?.id ?? at)}</span>
      {operation && (
        <>
          , ended {clock(operation.endedAt).text} ({operation.agent || "-"},{" "}
          {operationLabel(operation)})
        </>
      )}
      . Not live; presence is not replayed. <Link to={live}>Back to live</Link>
    </div>
  );
}
