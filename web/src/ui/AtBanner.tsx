// The banner of a view read at an operation (R2.19, shared since R2.20): what
// the point is, that it is not live, and a link back to live. The
// operation's id links to the activity feed with it selected (R2.21).
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import { activityHref } from "../views/activity-route";
import { shortId } from "./ids";
import { operationLabel } from "./operation";
import { Link } from "./router";
import { clock } from "./time";

export function AtBanner({
  repo,
  operation,
  at,
  live,
}: {
  repo: string;
  /** Once looked up. */
  operation: Operation | undefined;
  /** As the route gave it, until then. */
  at: string;
  /** The same view, live. */
  live: string;
}) {
  const id = operation?.id ?? at;
  return (
    <div className="banner" role="status">
      At operation{" "}
      <Link to={activityHref({ repo, op: id, at: id })} title={id}>
        <span className="mono">{shortId(id)}</span>
      </Link>
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
