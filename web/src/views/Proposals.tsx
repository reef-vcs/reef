// The Proposals tab (R3.31, decision 4): the repository's open proposals,
// newest update first, held in place while the person reads. R3.33's inbox
// replaces this list; each row opens the proposal page.
import { useEffect, useState, useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import type { ProposalEntry } from "../gen/reef/api/v1/changes_pb";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { changeState } from "../state/states";
import { StateBadge } from "../state/StateBadge";
import { ErrorView } from "../ui/ErrorView";
import { Link } from "../ui/router";
import { Time } from "../ui/TimeLabel";
import { plural } from "../ui/words";
import { PROPOSALS_SHOWN, ProposalsModel } from "./proposal-model";
import { proposalHref, proposalsHref } from "./proposal-route";
import { automaticWords, changeLabel } from "./proposal-words";

function Row({ repo, entry }: { repo: string; entry: ProposalEntry }) {
  const subject = entry.change?.description.split("\n")[0]?.trim() ?? "";
  const below = entry.dependencies.length;
  const status = automaticWords(entry.automatic);
  const state = entry.change
    ? changeState(entry.change)
    : { state: "none" as const, label: "gone" };
  return (
    <li className="proposal-row">
      <StateBadge {...state} />
      <span className="proposal-row-text">
        <Link to={proposalHref({ repo, change: entry.changeId })} className="name-link">
          {subject || "(no description)"}
        </Link>
        <span className="secondary">
          {" "}
          <span className="mono">{changeLabel(entry.changeId)}</span>
          {entry.proposedBy && ` · by ${entry.proposedBy}`}
          {below > 0 && ` · on ${plural(below, "change")} in review`}
          {" · "}
          <Time at={entry.lifecycle?.last?.recordedAt} />
        </span>
        {status && <span className="proposal-row-status">{status}</span>}
      </span>
    </li>
  );
}

export function Proposals({
  repo,
  at,
  connection,
  live,
  frame,
  onSessionInvalid,
}: {
  repo: string;
  at: string;
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
}) {
  const [model] = useState(
    () => new ProposalsModel(connection, live.operations, frame, onSessionInvalid, repo),
  );
  useEffect(() => model.start(), [model]);
  const { rows, fresh, left, more, error } = useSyncExternalStore(
    model.subscribe,
    model.getSnapshot,
  );
  return (
    <div className="proposals">
      <h2>Proposals</h2>
      {at && (
        <p className="notice">
          Proposals are read live, not at an operation.{" "}
          <Link to={proposalsHref({ repo })}>Show them now</Link>
        </p>
      )}
      {error && <ErrorView error={error} />}
      {(fresh > 0 || left > 0) && (
        <p className="notice" role="status">
          {fresh > 0 && `${plural(fresh, "new proposal")}. `}
          {left > 0 && `${plural(left, "proposal")} left review. `}
          <button
            type="button"
            onClick={() => {
              model.showNew();
            }}
          >
            {fresh > 0 ? `Show ${plural(fresh, "new proposal")}` : "Hide those that left"}
          </button>
        </p>
      )}
      {rows === null && !error && <p className="secondary">Reading the proposals.</p>}
      {rows?.length === 0 && (
        <div className="empty card">
          <p>No open proposals.</p>
          <p className="secondary">
            An agent proposes a change with <code>reef propose</code>; it shows here for review.
          </p>
        </div>
      )}
      {rows && rows.length > 0 && (
        <ul className="card proposal-list" aria-label="Open proposals">
          {rows.map((entry) => (
            <Row key={entry.changeId} repo={repo} entry={entry} />
          ))}
        </ul>
      )}
      {more && (
        <p className="secondary">
          Showing the newest {PROPOSALS_SHOWN.toLocaleString("en-US")}; run{" "}
          <code>reef proposal list</code> for the rest.
        </p>
      )}
    </div>
  );
}
