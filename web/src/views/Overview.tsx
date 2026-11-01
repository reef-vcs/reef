// `/`: every registered repository with its latest operation and the number
// of agents active in it now, live from both streams. The repository links to
// its smartlog (R2.19); the agent count, and the latest operation's workspace
// and agent, link to its workspaces page (R2.20). R2.21 makes the operation
// itself a link.
import { useEffect, useState, useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import type { Repository } from "../gen/reef/api/v1/repositories_pb";
import type { FrameScheduler } from "../live/frame";
import { agentCounts } from "../fleet/agents";
import type { Live } from "../live/share";
import { ErrorView } from "../ui/ErrorView";
import { operationLabel } from "../ui/operation";
import { Link } from "../ui/router";
import { clock } from "../ui/time";
import { fleetHref } from "./fleet-route";
import { OverviewModel } from "./overview-model";
import { smartlogHref } from "./Smartlog";

function lastComponent(root: string): string {
  return root.replace(/\/+$/, "").split("/").pop() || root;
}

function LatestOperation({ operation }: { operation: Operation | undefined }) {
  // The first operation shown is not a change; every later one is.
  const [first] = useState(operation?.id);
  if (!operation) return <span className="secondary">none yet</span>;
  const time = clock(operation.endedAt);
  return (
    // Keyed by id, so each new operation replays the one change transition.
    <span
      key={operation.id}
      className={operation.id === first ? undefined : "changed"}
      title={operation.description}
    >
      {operationLabel(operation)} ·{" "}
      {operation.agent ? (
        <Link to={fleetHref({ repo: operation.repoId, agent: operation.agent })}>
          {operation.agent}
        </Link>
      ) : (
        "-"
      )}{" "}
      ·{" "}
      {operation.workspaceId ? (
        <Link
          to={fleetHref({ repo: operation.repoId, workspace: operation.workspaceId })}
          title={operation.workspaceId}
        >
          <span className="mono">{operation.workspaceId.slice(0, 8)}</span>
        </Link>
      ) : (
        <span className="mono">-</span>
      )}{" "}
      · <time title={time.title}>{time.text}</time>
    </span>
  );
}

function Row({
  repository,
  operation,
  agents,
}: {
  repository: Repository;
  operation: Operation | undefined;
  agents: number;
}) {
  return (
    <tr>
      <td>
        <Link to={smartlogHref({ repo: repository.id })} title={repository.root}>
          {lastComponent(repository.root)}
        </Link>
      </td>
      <td className="mono">{repository.id.slice(0, 8)}</td>
      <td>
        <LatestOperation operation={operation} />
      </td>
      <td>
        <Link to={fleetHref({ repo: repository.id })} title="Workspaces and agents">
          {agents}
        </Link>
      </td>
    </tr>
  );
}

/** Agents among live presence entries, per repository, counted once each (fleet/agents.ts). */
function useActiveAgents(live: Live): Map<string, number> {
  // Re-renders when the set changes; joining a few hundred entries is cheap.
  useSyncExternalStore(
    (listener) => live.presence.subscribe(listener),
    () => live.presence.version,
  );
  return agentCounts(live.presence.entries.values());
}

export function Overview({
  connection,
  live,
  frame,
  onSessionInvalid,
}: {
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
}) {
  const [model] = useState(
    () => new OverviewModel(connection, live.operations, frame, onSessionInvalid),
  );
  useEffect(() => model.start(), [model]);
  const { repositories, latest, error } = useSyncExternalStore(model.subscribe, model.getSnapshot);
  const agents = useActiveAgents(live);

  return (
    <>
      <h2>Repositories</h2>
      {error && <ErrorView error={error} />}
      {repositories === null && !error && <p className="secondary">Loading repositories.</p>}
      {repositories?.length === 0 && (
        <p>
          No repositories are registered. Run <code>reef init</code> in one.
        </p>
      )}
      {repositories && repositories.length > 0 && (
        <div className="table-box">
          <table>
            <thead>
              <tr>
                <th scope="col">Repository</th>
                <th scope="col">Id</th>
                <th scope="col">Latest operation</th>
                <th scope="col">Active agents</th>
              </tr>
            </thead>
            <tbody>
              {repositories.map((repository) => (
                <Row
                  key={repository.id}
                  repository={repository}
                  operation={latest.get(repository.id)}
                  agents={agents.get(repository.id) ?? 0}
                />
              ))}
            </tbody>
          </table>
        </div>
      )}
    </>
  );
}
