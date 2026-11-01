// `/`: every registered repository as a card, with who is working in it now
// and its latest operation, live from both streams. The repository links to
// its smartlog (R2.19); the agents, and the latest operation's workspace and
// agent, link to its workspaces page (R2.20); the operation itself, to the
// activity feed with it selected (R2.21). Developer mode adds the
// repository's id and shows the operation in the API's words.
import { timestampDate } from "@bufbuild/protobuf/wkt";
import { useEffect, useState, useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import type { Operation } from "../gen/reef/api/v1/operations_pb";
import type { Repository } from "../gen/reef/api/v1/repositories_pb";
import type { FrameScheduler } from "../live/frame";
import { agentNames } from "../fleet/agents";
import type { Live } from "../live/share";
import { Avatar, AvatarStack } from "../ui/Avatar";
import { useDevMode } from "../ui/dev-mode";
import { ErrorView } from "../ui/ErrorView";
import { operationLabel } from "../ui/operation";
import { Link } from "../ui/router";
import { clock } from "../ui/time";
import { Time } from "../ui/TimeLabel";
import { capitalize, operationSentence, plural } from "../ui/words";
import { activityHref } from "./activity-route";
import { fleetHref } from "./fleet-route";
import { OverviewModel } from "./overview-model";
import { smartlogHref } from "./smartlog-route";

function lastComponent(root: string): string {
  return root.replace(/\/+$/, "").split("/").pop() || root;
}

/** Today's form: `describe · agent · ws123456 · 14:03:22`. */
function DevOperation({ operation }: { operation: Operation }) {
  const time = clock(operation.endedAt);
  return (
    <>
      <Link to={activityHref({ repo: operation.repoId, op: operation.id })} title={operation.id}>
        {operationLabel(operation)}
      </Link>{" "}
      ·{" "}
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
    </>
  );
}

/** "ann saved a snapshot in ws123456 · 4 min ago". */
function HumanOperation({ operation }: { operation: Operation }) {
  const sentence = operationSentence(operation);
  return (
    <>
      {operation.agent ? (
        <>
          <Avatar name={operation.agent} size="sm" />{" "}
          <Link
            to={fleetHref({ repo: operation.repoId, agent: operation.agent })}
            className="name-link"
          >
            {operation.agent}
          </Link>{" "}
        </>
      ) : null}
      <Link to={activityHref({ repo: operation.repoId, op: operation.id })}>
        {operation.agent ? sentence : capitalize(sentence)}
      </Link>
      {operation.workspaceId && (
        <>
          {" in "}
          <Link
            to={fleetHref({ repo: operation.repoId, workspace: operation.workspaceId })}
            className="mono"
          >
            {operation.workspaceId.slice(0, 8)}
          </Link>
        </>
      )}
      <span className="secondary">
        {" · "}
        <Time at={operation.endedAt} />
      </span>
    </>
  );
}

function LatestOperation({ operation }: { operation: Operation | undefined }) {
  // The first operation shown is not a change; every later one is.
  const [first] = useState(operation?.id);
  const dev = useDevMode();
  if (!operation) return <p className="latest secondary">No activity yet</p>;
  const subject = operation.description.split("\n")[0];
  return (
    // Keyed by id, so each new operation replays the one change transition.
    <div
      key={operation.id}
      className={operation.id === first ? "latest" : "latest changed"}
      title={dev ? operation.description : undefined}
    >
      <p>
        {dev ? <DevOperation operation={operation} /> : <HumanOperation operation={operation} />}
      </p>
      {!dev && subject && <p className="latest-subject secondary">{subject}</p>}
    </div>
  );
}

function Card({
  repository,
  operation,
  agents,
}: {
  repository: Repository;
  operation: Operation | undefined;
  agents: string[];
}) {
  const dev = useDevMode();
  const count = agents.length;
  return (
    <li className="card repo-card">
      <div className="repo-card-head">
        <h2>
          <Link
            to={smartlogHref({ repo: repository.id })}
            title={repository.root}
            className="name-link"
          >
            {lastComponent(repository.root)}
          </Link>
        </h2>
        <span className="repo-agents">
          <AvatarStack names={agents} max={4} size="md" />
          <Link to={fleetHref({ repo: repository.id })} title="Workspaces and agents">
            {count === 0 ? "No agents working" : `${plural(count, "agent")} working`}
          </Link>
        </span>
      </div>
      <p className="repo-root mono" title={repository.root}>
        <bdi>{repository.root}</bdi>
      </p>
      {dev && (
        <p className="repo-id secondary">
          Id <span className="mono">{repository.id.slice(0, 8)}</span>
        </p>
      )}
      <LatestOperation operation={operation} />
      <p className="repo-links">
        <Link to={smartlogHref({ repo: repository.id })}>Smartlog</Link>
        <Link to={fleetHref({ repo: repository.id })}>Workspaces</Link>
        <Link to={activityHref({ repo: repository.id })}>Activity</Link>
      </p>
    </li>
  );
}

/** Agents among live presence entries, per repository, counted once each (fleet/agents.ts). */
function useActiveAgents(live: Live): Map<string, string[]> {
  // Re-renders when the set changes; joining a few hundred entries is cheap.
  useSyncExternalStore(
    (listener) => live.presence.subscribe(listener),
    () => live.presence.version,
  );
  return agentNames(live.presence.entries.values());
}

function Summary({
  repositories,
  agents,
  latest,
}: {
  repositories: Repository[];
  agents: Map<string, string[]>;
  latest: ReadonlyMap<string, Operation>;
}) {
  // Each agent once, in the listed repositories only.
  const working = new Set(repositories.flatMap((r) => agents.get(r.id) ?? [])).size;
  let newest: Operation | undefined;
  for (const operation of latest.values()) {
    if (!operation.endedAt) continue;
    if (
      !newest?.endedAt ||
      timestampDate(operation.endedAt).getTime() > timestampDate(newest.endedAt).getTime()
    ) {
      newest = operation;
    }
  }
  return (
    <p className="page-summary secondary">
      {plural(repositories.length, "repository", "repositories")}
      {working > 0 && ` · ${plural(working, "agent")} working now`}
      {newest && (
        <>
          {" · last activity "}
          <Time at={newest.endedAt} />
        </>
      )}
    </p>
  );
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
      <h1>Repositories</h1>
      {repositories && repositories.length > 0 && (
        <Summary repositories={repositories} agents={agents} latest={latest} />
      )}
      {error && <ErrorView error={error} />}
      {repositories === null && !error && <p className="secondary">Loading repositories.</p>}
      {repositories?.length === 0 && (
        <p className="empty">
          No repositories are registered. Run <code>reef init</code> in one.
        </p>
      )}
      {repositories && repositories.length > 0 && (
        <ul className="repo-cards" aria-label="Repositories">
          {repositories.map((repository) => (
            <Card
              key={repository.id}
              repository={repository}
              operation={latest.get(repository.id)}
              agents={agents.get(repository.id) ?? []}
            />
          ))}
        </ul>
      )}
    </>
  );
}
