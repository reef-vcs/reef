// The selected change (R2.19): what the loaded entry says, with no read; its
// files from `GetDiff`; who wrote it from `QueryChanges` (live only: the
// history index is not replayed); and the agents present on it now. Its
// workspaces and agents link to the workspaces page (R2.20): the rows are
// listbox options, which cannot hold links, so the links are here. All text
// is plain text: nothing from the API is rendered as HTML.
import { useEffect, useRef, useState } from "react";
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import type { Change } from "../gen/reef/api/v1/changes_pb";
import { DiffFormat, type GetDiffResponse } from "../gen/reef/api/v1/diff_pb";
import { FileChangeKind } from "../gen/reef/api/v1/files_pb";
import type { HistoryChange } from "../gen/reef/api/v1/history_pb";
import { PresenceKind, type Presence } from "../gen/reef/api/v1/presence_pb";
import type { Workspace } from "../gen/reef/api/v1/workspaces_pb";
import type { EdgeKind } from "../graph/node";
import { shellQuote } from "../graph/revset";
import { after } from "../live/backoff";
import { outcomeState } from "../state/states";
import { StateBadge } from "../state/StateBadge";
import { Copy } from "../ui/Copy";
import { ErrorView } from "../ui/ErrorView";
import { operationLabel } from "../ui/operation";
import { reverseHex, shortId } from "../ui/ids";
import { Link } from "../ui/router";
import { clock, dateTime } from "../ui/time";
import { fleetHref } from "./fleet-route";
import { MIN_SPACING_MS, type SmartlogRow } from "./smartlog-model";

export const HISTORY_DISABLED = "HISTORY_DISABLED";
const FILES_SHOWN = 100;

const FILE_KINDS: Partial<Record<FileChangeKind, string>> = {
  [FileChangeKind.ADDED]: "A",
  [FileChangeKind.MODIFIED]: "M",
  [FileChangeKind.DELETED]: "D",
  [FileChangeKind.RENAMED]: "R",
  [FileChangeKind.COPIED]: "C",
};

export const EDGE_WORDS: Record<EdgeKind, string> = {
  direct: "direct",
  indirect: "through commits this view leaves out",
  missing: "outside this view",
};

const PRESENCE_KINDS: Partial<Record<PresenceKind, string>> = {
  [PresenceKind.ACTIVITY]: "active",
  [PresenceKind.EDITING]: "editing",
  [PresenceKind.LEASE]: "holds a lease",
  [PresenceKind.PROGRESS]: "working",
};

/** The diff's first page of files at the selected commit, read when the commit changes. */
function useFiles(
  connection: Connection,
  repo: string,
  commit: string,
  at: string,
  onSessionInvalid: () => void,
) {
  // Each answer is kept with its commit, so one for another selection is never shown.
  const [files, setFiles] = useState<{ commit: string; response: GetDiffResponse } | null>(null);
  const [error, setError] = useState<{ commit: string; error: ReefError } | null>(null);
  useEffect(() => {
    let current = true;
    connection.clients.diff
      .getDiff({
        repo,
        toRevision: commit,
        format: DiffFormat.STAT,
        pageSize: FILES_SHOWN,
        atOperation: at,
      })
      .then(
        (response) => {
          if (current) setFiles({ commit, response });
        },
        (thrown: unknown) => {
          if (!current) return;
          const error = reefError(thrown);
          if (isSessionInvalid(error)) onSessionInvalid();
          else setError({ commit, error });
        },
      );
    return () => {
      current = false;
    };
  }, [connection, repo, commit, at, onSessionInvalid]);
  return {
    files: files?.commit === commit ? files.response : null,
    error: error?.commit === commit ? error.error : null,
  };
}

/**
 * Who wrote the change, read when the selection changes, and again when its
 * cause does, spaced like a refresh. Null while frozen at an operation.
 */
function useHistory(
  connection: Connection,
  repo: string,
  changeId: string,
  cause: string,
  frozen: boolean,
  onSessionInvalid: () => void,
) {
  const [history, setHistory] = useState<{ changeId: string; change: HistoryChange | null } | null>(
    null,
  );
  // Answers and errors are kept with their change, so neither shows under another.
  const [error, setError] = useState<{ changeId: string; error: ReefError } | null>(null);
  const last = useRef({ changeId: "", at: Number.NEGATIVE_INFINITY, duration: 0 });
  useEffect(() => {
    if (frozen) return;
    let current = true;
    const read = () => {
      const started = Date.now();
      last.current = { changeId, at: started, duration: last.current.duration };
      const done = () => {
        last.current.duration = Date.now() - started;
      };
      connection.clients.history
        .queryChanges({ repo, changes: [changeId], includeEmpty: true, pageSize: 1 })
        .then(
          (response) => {
            done();
            if (!current) return;
            setError(null);
            setHistory({ changeId, change: response.changes[0] ?? null });
          },
          (thrown: unknown) => {
            done();
            if (!current) return;
            const error = reefError(thrown);
            if (isSessionInvalid(error)) onSessionInvalid();
            else setError({ changeId, error });
          },
        );
    };
    // Spaced like a refresh: max(250 ms, 4 × the last read) after the last start.
    const spacing = Math.max(MIN_SPACING_MS, 4 * last.current.duration);
    const wait = last.current.changeId === changeId ? last.current.at + spacing - Date.now() : 0;
    const cancel = wait > 0 ? after(wait, read) : (read(), undefined);
    return () => {
      current = false;
      cancel?.();
    };
  }, [connection, repo, changeId, cause, frozen, onSessionInvalid]);
  return {
    history: history?.changeId === changeId ? history.change : undefined,
    error: error?.changeId === changeId ? error.error : null,
  };
}

function Ids({ change }: { change: Change }) {
  return (
    <dl className="facts">
      <dt>Change</dt>
      <dd className="mono">
        {reverseHex(change.changeId)}
        <br />
        <span className="secondary">{change.changeId}</span>
      </dd>
      <dt>Commit</dt>
      <dd className="mono">{change.commitId}</dd>
    </dl>
  );
}

function Signature({ name, who }: { name: string; who: Change["author"] }) {
  if (!who) return null;
  const time = dateTime(who.at);
  return (
    <>
      <dt>{name}</dt>
      <dd>
        {who.name} &lt;{who.email}&gt; · <time title={time.title}>{time.text}</time>
      </dd>
    </>
  );
}

function Files({
  files,
  error,
  repo,
  commit,
  at,
}: {
  files: GetDiffResponse | null;
  error: ReefError | null;
  repo: string;
  commit: string;
  /** At an operation: the frozen view's, for the command. */
  at: string | undefined;
}) {
  const command = `reef diff --repo ${shellQuote(repo)} -r ${commit} --stat${at ? ` --at-op ${at}` : ""}`;
  return (
    <section>
      <h4>Files</h4>
      {error && <ErrorView error={error} />}
      {!files && !error && <p className="secondary">Reading the files.</p>}
      {files && files.files.length === 0 && <p className="secondary">No files changed.</p>}
      {files && files.files.length > 0 && (
        <ul className="files">
          {files.files.map((file) => {
            const change = file.change;
            const path = change?.path ?? "";
            return (
              <li key={path}>
                <span className="mono kind">
                  {FILE_KINDS[change?.kind ?? FileChangeKind.UNSPECIFIED] ?? "?"}
                </span>{" "}
                <span className="mono">
                  {change?.oldPath ? `${change.oldPath} → ${path}` : path}
                </span>{" "}
                <span className="secondary">
                  {file.binary
                    ? "binary"
                    : `+${String(file.linesAdded)} -${String(file.linesRemoved)}`}
                </span>
              </li>
            );
          })}
        </ul>
      )}
      {files && files.totalFiles > files.files.length && (
        <p className="secondary">and {files.totalFiles - files.files.length} more files</p>
      )}
      <p className="command">
        <code>{command}</code> <Copy text={command} />
      </p>
    </section>
  );
}

function History({
  history,
  error,
}: {
  history: HistoryChange | null | undefined;
  error: ReefError | null;
}) {
  if (error?.reason === HISTORY_DISABLED) {
    return (
      <section>
        <h4>Who wrote it</h4>
        <p className="secondary">History is off for this daemon.</p>
      </section>
    );
  }
  return (
    <section>
      <h4>Who wrote it</h4>
      {error && <ErrorView error={error} />}
      {history === undefined && !error && <p className="secondary">Reading the history.</p>}
      {history === null && <p className="secondary">The history index has no record of it.</p>}
      {history && (
        <>
          <p>
            <StateBadge {...outcomeState(history.outcome)} />
          </p>
          {history.activity.length === 0 ? (
            <p className="secondary">No agent activity recorded.</p>
          ) : (
            <ul className="activity">
              {history.activity.map((row) => {
                const first = dateTime(row.firstAt);
                const last = dateTime(row.lastAt);
                return (
                  <li key={`${row.agent}\n${row.workspaceId}`}>
                    {row.agent || "-"} in{" "}
                    <span className="mono">{shortId(row.workspaceId) || "-"}</span>
                    {" · "}
                    <time title={first.title}>{first.text}</time> to{" "}
                    <time title={last.title}>{last.text}</time> · {row.operations} operations
                  </li>
                );
              })}
            </ul>
          )}
          {history.activityCount > history.activity.length && (
            <p className="secondary">and {history.activityCount - history.activity.length} more</p>
          )}
        </>
      )}
    </section>
  );
}

export function ChangeDetail({
  connection,
  repo,
  rows,
  workspaces,
  presence,
  agentsOf,
  at,
  frozen,
  onSelect,
  onSessionInvalid,
}: {
  connection: Connection;
  repo: string;
  /** The selected change's rows: two or more for a divergent change. */
  rows: SmartlogRow[];
  workspaces: ReadonlyMap<string, Workspace>;
  presence: Presence[];
  /** Each entry id's agents (fleet/agents.ts). */
  agentsOf: ReadonlyMap<string, string[]>;
  /** The operation the rows were read at. */
  at: string;
  frozen: boolean;
  onSelect: (row: SmartlogRow) => void;
  onSessionInvalid: () => void;
}) {
  const first = rows[0];
  const change = first?.change;
  const commit = change?.commitId ?? "";
  // A commit's files never change, so a live view reads them once per commit,
  // at the head; a frozen one reads them at its operation.
  const { files, error: filesError } = useFiles(
    connection,
    repo,
    commit,
    frozen ? at : "",
    onSessionInvalid,
  );
  const cause = first?.cause;
  const { history, error: historyError } = useHistory(
    connection,
    repo,
    change?.changeId ?? "",
    cause?.id ?? "",
    frozen,
    onSessionInvalid,
  );
  if (!first || !change) return null;
  const description = change.description.trimEnd();

  return (
    <div className="detail">
      <h3>
        <span className="mono">{shortId(reverseHex(change.changeId))}</span>{" "}
        <StateBadge {...first.state} />
      </h3>
      {rows.length > 1 && (
        <section>
          <p>This change is divergent: it has {rows.length} commits in this view.</p>
          <ul>
            {rows.map((row) => (
              <li key={row.change.commitId}>
                <button
                  type="button"
                  onClick={() => {
                    onSelect(row);
                  }}
                >
                  <span className="mono">{shortId(row.change.commitId)}</span>
                </button>
              </li>
            ))}
          </ul>
        </section>
      )}
      <pre className="description">{description || "(no description)"}</pre>
      <Ids change={change} />
      <dl className="facts">
        <Signature name="Author" who={change.author} />
        <Signature name="Committer" who={change.committer} />
        <dt>Parents</dt>
        <dd>
          {first.graph.node.edges.length === 0 && (
            <span className="secondary">none (the root)</span>
          )}
          <ul>
            {first.graph.node.edges.map((edge, i) => (
              <li key={i}>
                <span className="mono">{shortId(edge.parent)}</span>{" "}
                <span className="secondary">{EDGE_WORDS[edge.kind]}</span>
              </li>
            ))}
          </ul>
        </dd>
        {change.bookmarks.length > 0 && (
          <>
            <dt>Bookmarks</dt>
            <dd>{change.bookmarks.join(", ")}</dd>
          </>
        )}
        {change.tags.length > 0 && (
          <>
            <dt>Tags</dt>
            <dd>{change.tags.join(", ")}</dd>
          </>
        )}
        {change.workspaceIds.length > 0 && (
          <>
            <dt>Workspaces</dt>
            <dd>
              {change.workspaceIds.map((id) => {
                const workspace = workspaces.get(id);
                return (
                  <div key={id}>
                    <Link
                      to={fleetHref({ repo, at: frozen ? at : "", workspace: id })}
                      title="On the workspaces page"
                    >
                      <span className="mono">{id}</span>
                    </Link>
                    {workspace && (
                      <span className="secondary">
                        {" "}
                        {workspace.owner}
                        {workspace.purpose ? ` · ${workspace.purpose}` : ""}
                      </span>
                    )}
                  </div>
                );
              })}
            </dd>
          </>
        )}
        {change.jjWorkspaces.length > 0 && (
          <>
            <dt>jj workspaces</dt>
            <dd>{change.jjWorkspaces.join(", ")}</dd>
          </>
        )}
        {cause && (
          <>
            <dt>Last changed</dt>
            <dd>
              {operationLabel(cause)} by {cause.agent || "-"} at{" "}
              <time title={clock(cause.endedAt).title}>{clock(cause.endedAt).text}</time>, operation{" "}
              <span className="mono">{shortId(cause.id)}</span>
            </dd>
          </>
        )}
      </dl>
      <Files
        files={files}
        error={filesError}
        repo={repo}
        commit={commit}
        at={frozen ? at : undefined}
      />
      {!frozen && <History history={history} error={historyError} />}
      {!frozen && presence.length > 0 && (
        <section>
          <h4>Agents present now</h4>
          <ul className="activity">
            {presence.map((p) => (
              <li key={p.id}>
                {(agentsOf.get(p.id) ?? [p.agent]).map((agent, i) => (
                  <span key={agent}>
                    {i > 0 && ", "}
                    <Link to={fleetHref({ repo, agent })}>{agent}</Link>
                  </span>
                ))}{" "}
                · {PRESENCE_KINDS[p.kind] ?? "present"}
                {p.activity ? ` · ${p.activity}` : ""}
                {p.paths.length > 0 ? ` · editing ${String(p.paths.length)} paths` : ""}
              </li>
            ))}
          </ul>
        </section>
      )}
    </div>
  );
}
