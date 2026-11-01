// The activity feed's pane (R2.21): a run's operations, and the selected
// operation as `GetOperation` returns it, with its effects. All text is
// rendered as text.
import { useEffect, useState } from "react";
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import {
  ChangeEffectKind,
  type ChangeEffect,
  type Operation,
} from "../gen/reef/api/v1/operations_pb";
import type { Row } from "../log/coalesce";
import { Copy } from "../ui/Copy";
import { ErrorView } from "../ui/ErrorView";
import { reverseHex, shortId } from "../ui/ids";
import { operationLabel } from "../ui/operation";
import { Link } from "../ui/router";
import { clock, fullTime } from "../ui/time";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { activityHref } from "./activity-route";
import { fleetHref } from "./fleet-route";
import { smartlogHref } from "./smartlog-route";

/** A run's operations listed in the pane at most. */
export const RUN_LISTED = 50;
/** `GetOperation` answers kept, most recent first. */
export const CACHED = 64;

const EFFECT_WORDS: Partial<Record<ChangeEffectKind, string>> = {
  [ChangeEffectKind.CREATED]: "created",
  [ChangeEffectKind.REWRITTEN]: "rewritten",
  [ChangeEffectKind.ABANDONED]: "abandoned",
  [ChangeEffectKind.HIDDEN]: "hidden",
};

/** The pane's answers, by operation id or prefix, the most recent `CACHED`. */
export class OperationCache {
  private readonly answers = new Map<string, Operation>();

  get(id: string): Operation | undefined {
    const found = this.answers.get(id);
    if (found) {
      this.answers.delete(id);
      this.answers.set(id, found);
    }
    return found;
  }

  set(id: string, operation: Operation) {
    this.answers.delete(id);
    this.answers.set(id, operation);
    while (this.answers.size > CACHED) {
      const oldest = this.answers.keys().next().value;
      if (oldest === undefined) break;
      this.answers.delete(oldest);
    }
  }
}

function ChangeItem({
  repo,
  operation,
  effect,
}: {
  repo: string;
  operation: Operation;
  effect: ChangeEffect;
}) {
  // An abandoned change is not visible at the operation; it is at its parent.
  const at =
    effect.kind === ChangeEffectKind.ABANDONED
      ? (operation.parentIds[0] ?? operation.id)
      : operation.id;
  return (
    <li>
      {EFFECT_WORDS[effect.kind] ?? "changed"}{" "}
      <Link to={smartlogHref({ repo, change: effect.changeId, at })} title={effect.commitId}>
        <span className="mono">{shortId(reverseHex(effect.changeId))}</span>
      </Link>
    </li>
  );
}

function Effects({ repo, operation }: { repo: string; operation: Operation }) {
  const effects = operation.effects;
  if (!effects) return null;
  const changesMore = effects.changeCount - effects.changes.length;
  const bookmarksMore = effects.bookmarkCount - effects.bookmarks.length;
  if (
    effects.changes.length === 0 &&
    effects.bookmarks.length === 0 &&
    effects.workingCopies.length === 0
  ) {
    return <p className="secondary">It changed no change, bookmark or working copy.</p>;
  }
  return (
    <>
      {effects.changes.length > 0 && (
        <>
          <h4>Changes</h4>
          <ul className="activity">
            {effects.changes.map((effect) => (
              <ChangeItem
                key={`${effect.changeId}\n${effect.commitId}`}
                repo={repo}
                operation={operation}
                effect={effect}
              />
            ))}
            {changesMore > 0 && <li className="secondary">and {changesMore} more</li>}
          </ul>
        </>
      )}
      {effects.bookmarks.length > 0 && (
        <>
          <h4>Bookmarks</h4>
          <ul className="activity">
            {effects.bookmarks.map((b) => (
              <li key={`${b.name}@${b.remote}`}>
                <span className="mono">{b.remote ? `${b.name}@${b.remote}` : b.name}</span>{" "}
                {!b.previousCommitId
                  ? `set to ${shortId(b.commitId)}`
                  : !b.commitId
                    ? "deleted"
                    : `moved ${shortId(b.previousCommitId)} to ${shortId(b.commitId)}`}
                {b.conflicted && " (conflicted)"}
              </li>
            ))}
            {bookmarksMore > 0 && <li className="secondary">and {bookmarksMore} more</li>}
          </ul>
        </>
      )}
      {effects.workingCopies.length > 0 && (
        <>
          <h4>Working copies</h4>
          <ul className="activity">
            {effects.workingCopies.map((w) => (
              <li key={w.workspaceId || w.jjWorkspace}>
                {w.workspaceId ? (
                  <Link to={fleetHref({ repo, workspace: w.workspaceId })} title={w.workspaceId}>
                    <span className="mono">{shortId(w.workspaceId)}</span>
                  </Link>
                ) : (
                  <span className="mono">{w.jjWorkspace}</span>
                )}{" "}
                {!w.previousCommitId
                  ? "added"
                  : !w.commitId
                    ? "forgotten"
                    : `moved ${shortId(w.previousCommitId)} to ${shortId(w.commitId)}`}
              </li>
            ))}
          </ul>
        </>
      )}
    </>
  );
}

export function OperationDetail({
  repo,
  op,
  at,
  row,
  inWindow,
  connection,
  cache,
  onSessionInvalid,
}: {
  /** The repository's full id. */
  repo: string;
  /** The selected operation, as the route gave it. */
  op: string;
  /** The feed's point, or empty. */
  at: string;
  /** The row holding it, when the feed shows it. */
  row: Row | undefined;
  /** The window holds it. */
  inWindow: boolean;
  connection: Connection;
  cache: OperationCache;
  onSessionInvalid: () => void;
}) {
  const [answer, setAnswer] = useState<{ op: string; operation?: Operation; error?: ReefError }>(
    () => {
      const cached = cache.get(op);
      return cached ? { op, operation: cached } : { op };
    },
  );
  useEffect(() => {
    // Keyed by `op`, so a cached answer is already the initial state.
    if (cache.get(op)) return;
    let current = true;
    connection.clients.operations.getOperation({ repo, operationId: op }).then(
      (response) => {
        if (!current || !response.operation) return;
        cache.set(op, response.operation);
        setAnswer({ op, operation: response.operation });
      },
      (thrown: unknown) => {
        if (!current) return;
        const error = reefError(thrown);
        if (isSessionInvalid(error)) onSessionInvalid();
        else setAnswer({ op, error });
      },
    );
    return () => {
      current = false;
    };
  }, [repo, op, connection, cache, onSessionInvalid]);

  const operation = answer.op === op ? answer.operation : undefined;
  const error = answer.op === op ? answer.error : undefined;
  const select = (id: string) => activityHref({ repo, op: id, at });
  const command = `reef op show ${operation?.id ?? op} --repo ${repo}`;

  return (
    <div className="detail">
      {row && row.operations.length > 1 && (
        <>
          <h3>{row.operations.length} operations in this row</h3>
          <ul className="activity">
            {row.operations.slice(0, RUN_LISTED).map((o) => (
              <li key={o.id}>
                <Link
                  to={select(o.id)}
                  replace
                  current={o.id === operation?.id ? "true" : undefined}
                >
                  {clock(timestampFromMs(o.endedAt)).text} · {operationLabel(o)} ·{" "}
                  <span className="mono">{shortId(o.id)}</span>
                </Link>
              </li>
            ))}
            {row.operations.length > RUN_LISTED && (
              <li className="secondary">and {row.operations.length - RUN_LISTED} more</li>
            )}
          </ul>
        </>
      )}
      {error && <ErrorView error={error} />}
      {!error && !operation && <p className="secondary">Loading the operation.</p>}
      {operation && (
        <>
          <h3>{operationLabel(operation)}</h3>
          {!inWindow && (
            <p>
              Not in the loaded period.{" "}
              <Link to={activityHref({ repo, op: operation.id, at: operation.id })}>
                Show the feed at this operation
              </Link>
            </p>
          )}
          <dl className="facts">
            <dt>Id</dt>
            <dd>
              <span className="mono">{operation.id}</span> <Copy text={operation.id} />
            </dd>
            <dt>Started</dt>
            <dd title={fullTime(operation.startedAt).title}>
              {fullTime(operation.startedAt).text}
            </dd>
            <dt>Ended</dt>
            <dd title={fullTime(operation.endedAt).title}>{fullTime(operation.endedAt).text}</dd>
            <dt>Agent</dt>
            <dd>
              {operation.agent ? (
                <Link to={fleetHref({ repo, agent: operation.agent, at })}>{operation.agent}</Link>
              ) : (
                "-"
              )}
            </dd>
            <dt>Workspace</dt>
            <dd>
              {operation.workspaceId ? (
                <Link
                  to={fleetHref({ repo, workspace: operation.workspaceId, at })}
                  title={operation.workspaceId}
                >
                  <span className="mono">{shortId(operation.workspaceId)}</span>
                </Link>
              ) : (
                <span className="mono">{operation.jjWorkspace || "-"}</span>
              )}
            </dd>
            <dt>By</dt>
            <dd>
              {operation.username || "-"}@{operation.hostname || "-"}
            </dd>
            {operation.remote?.tokenName && (
              <>
                <dt>Remote token</dt>
                <dd>{operation.remote.tokenName}</dd>
              </>
            )}
            <dt>Parents</dt>
            <dd>
              {operation.parentIds.length === 0
                ? "none (the root)"
                : operation.parentIds.map((id, i) => (
                    <span key={id}>
                      {i > 0 && ", "}
                      <Link to={select(id)} replace title={id}>
                        <span className="mono">{shortId(id)}</span>
                      </Link>
                    </span>
                  ))}
            </dd>
          </dl>
          {operation.description && <p className="description pre-wrap">{operation.description}</p>}
          <Effects repo={repo} operation={operation} />
          {Object.keys(operation.attributes).length > 0 && (
            <>
              <h4>Attributes</h4>
              <ul className="activity mono">
                {Object.entries(operation.attributes)
                  .sort(([a], [b]) => (a < b ? -1 : a > b ? 1 : 0))
                  .map(([key, value]) => (
                    <li key={key}>
                      {key} = {value}
                    </li>
                  ))}
              </ul>
            </>
          )}
          <p>
            Open at this operation:{" "}
            <Link to={smartlogHref({ repo, at: operation.id })}>smartlog</Link> ·{" "}
            <Link to={fleetHref({ repo, at: operation.id })}>workspaces</Link> ·{" "}
            <Link to={activityHref({ repo, op: operation.id, at: operation.id })}>activity</Link>
          </p>
        </>
      )}
      <p className="command">
        <code>{command}</code> <Copy text={command} />
      </p>
    </div>
  );
}
