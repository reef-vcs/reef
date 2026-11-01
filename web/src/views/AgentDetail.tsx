// The selected agent (R2.20): its entries, its workspaces with their
// changes, and its endings since the page opened. It reads nothing.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import type { Ending } from "../live/presence";
import { changeState } from "../state/states";
import { StateBadge } from "../state/StateBadge";
import { reverseHex, shortId } from "../ui/ids";
import { Link } from "../ui/router";
import { clock } from "../ui/time";
import type { WorkspaceRow } from "./fleet-model";
import { fleetHref } from "./fleet-route";
import { doing, ENDING_WORDS, type AgentRow } from "./fleet-rows";
import { smartlogHref } from "./smartlog-route";

const ENDINGS_SHOWN = 10;

export function AgentDetail({
  repo,
  at,
  row,
  rows,
  endings,
}: {
  repo: string;
  /** The operation the page is frozen at, or empty: links keep it. */
  at: string;
  row: AgentRow;
  rows: ReadonlyMap<string, WorkspaceRow>;
  endings: Ending[];
}) {
  const agent = row.agent;
  return (
    <div className="detail">
      <h3>{row.name}</h3>
      {!agent && (
        <p>
          {row.ending ? ENDING_WORDS[row.ending.reason] : "ended"}
          {row.ending && ` ${clock(timestampFromMs(row.ending.at)).text}`}
        </p>
      )}
      {agent && (
        <>
          <h4>Workspaces</h4>
          {agent.workspaceIds.length === 0 && <p className="secondary">no workspace</p>}
          <ul className="activity">
            {agent.workspaceIds.map((id) => {
              const workspace = rows.get(id);
              const change = workspace?.change;
              return (
                <li key={id}>
                  <Link to={fleetHref({ repo, at, workspace: id })} replace>
                    <span className="mono">{shortId(id)}</span>
                  </Link>
                  {workspace && (
                    <>
                      {" on "}
                      {change && <StateBadge {...changeState(change)} />}{" "}
                      <Link to={smartlogHref({ repo, at, change: workspace.workspace.changeId })}>
                        <span className="mono">
                          {shortId(reverseHex(workspace.workspace.changeId))}
                        </span>
                      </Link>
                    </>
                  )}
                </li>
              );
            })}
          </ul>
          <h4>Presence now</h4>
          <ul className="activity">
            {agent.entries.map((p) => (
              <li key={p.id}>
                {doing(p)}
                {p.workspaceId && (
                  <span className="secondary">
                    {" "}
                    on <span className="mono">{shortId(p.workspaceId)}</span>
                  </span>
                )}
                {p.byDaemon && <span className="secondary"> (reported by the daemon)</span>}
                <span className="secondary"> · {clock(p.updatedAt).text}</span>
              </li>
            ))}
          </ul>
        </>
      )}
      {endings.length > 0 && (
        <>
          <h4>Ended since you opened</h4>
          <ul className="activity">
            {endings
              .slice(-ENDINGS_SHOWN)
              .reverse()
              .map((e) => (
                <li key={e.presence.id}>
                  {doing(e.presence)} · {ENDING_WORDS[e.reason]} {clock(timestampFromMs(e.at)).text}
                </li>
              ))}
          </ul>
        </>
      )}
    </div>
  );
}
