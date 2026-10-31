// `/`: every registered repository with its latest operation and the number
// of agents active in it now, live from both streams. R2.19 to R2.21 make
// its cells links into their views; until then ids are shown as text.
import { useEffect, useState, useSyncExternalStore } from "react";
import type { Connection } from "../api/connection";
import { OperationKind, SnapshotKind, type Operation } from "../gen/reef/api/v1/operations_pb";
import type { Repository } from "../gen/reef/api/v1/repositories_pb";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { ErrorView } from "../ui/ErrorView";
import { clock } from "../ui/time";
import { OverviewModel } from "./overview-model";

const KINDS: Partial<Record<OperationKind, string>> = {
  [OperationKind.SNAPSHOT]: "snapshot",
  [OperationKind.CREATE_WORKSPACE]: "create workspace",
  [OperationKind.RELEASE_WORKSPACE]: "release workspace",
  [OperationKind.MOVE_WORKSPACE]: "move workspace",
  [OperationKind.DESCRIBE]: "describe",
  [OperationKind.ABANDON]: "abandon",
  [OperationKind.UNDO]: "undo",
  [OperationKind.EDIT]: "edit",
  [OperationKind.IMPORT_REFS]: "import refs",
  [OperationKind.EXTERNAL]: "external",
  [OperationKind.OTHER]: "other",
};

const SNAPSHOTS: Partial<Record<SnapshotKind, string>> = {
  [SnapshotKind.WATCHER]: "watcher",
  [SnapshotKind.EXPLICIT]: "explicit",
  [SnapshotKind.FINAL]: "final",
  [SnapshotKind.MOVE]: "move",
  [SnapshotKind.SHUTDOWN]: "shutdown",
};

export function operationLabel(operation: Operation): string {
  const kind = KINDS[operation.kind] ?? "unknown";
  const snapshot = SNAPSHOTS[operation.snapshotKind];
  return operation.kind === OperationKind.SNAPSHOT && snapshot ? `${kind} (${snapshot})` : kind;
}

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
      {operationLabel(operation)} · {operation.agent || "-"} ·{" "}
      <span className="mono">
        {operation.workspaceId ? operation.workspaceId.slice(0, 8) : "-"}
      </span>{" "}
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
      <td title={repository.root}>{lastComponent(repository.root)}</td>
      <td className="mono">{repository.id.slice(0, 8)}</td>
      <td>
        <LatestOperation operation={operation} />
      </td>
      <td>{agents}</td>
    </tr>
  );
}

/** Distinct agents among live presence entries, per repository. */
function useActiveAgents(live: Live): Map<string, number> {
  // Re-renders when the set changes; counting a few hundred entries is cheap.
  useSyncExternalStore(
    (listener) => live.presence.subscribe(listener),
    () => live.presence.version,
  );
  const agents = new Map<string, Set<string>>();
  for (const presence of live.presence.entries.values()) {
    if (!presence.repoId) continue;
    let set = agents.get(presence.repoId);
    if (!set) agents.set(presence.repoId, (set = new Set()));
    set.add(presence.agent);
  }
  return new Map([...agents].map(([repo, set]) => [repo, set.size]));
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
