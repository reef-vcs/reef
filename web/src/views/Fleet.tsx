// `/repos/<repo>/workspaces` (R2.20): who is doing what, and what is stuck.
// Agents (the fleet panel) and Workspaces (the inventory) on one page, each
// joined to the other, live from the shared streams with no stream of its
// own. The query carries `workspace`, `agent` and `at` (an operation: the
// page at that point, frozen, without presence or disk).
//
// Both sections are tables: their rows hold several links each, which a
// listbox's options cannot. A selection is a link followed without a history
// entry. Rows never move (fleet-model.ts), and the selected row, or else the
// first row in view, stays where it is on screen across an update.
import { useEffect, useMemo, useState, useSyncExternalStore, type ReactNode } from "react";
import type { Connection } from "../api/connection";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import type { Presence } from "../gen/reef/api/v1/presence_pb";
import { LeaseState, WorkspaceState } from "../gen/reef/api/v1/workspaces_pb";
import { agentsOfWorkspace, joinAgents } from "../fleet/agents";
import { expiry, type Health } from "../fleet/lease";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { changeState } from "../state/states";
import { StateBadge } from "../state/StateBadge";
import { AtBanner } from "../ui/AtBanner";
import { ErrorView } from "../ui/ErrorView";
import { reverseHex, shortId } from "../ui/ids";
import { operationLabel } from "../ui/operation";
import { RepoNav } from "../ui/RepoNav";
import { Link } from "../ui/router";
import { useScrollAnchor, type AnchorView } from "../ui/scroll-anchor";
import { size } from "../ui/size";
import { clock } from "../ui/time";
import { AgentDetail } from "./AgentDetail";
import { FleetModel, WORKSPACES_SHOWN, type WorkspaceRow } from "./fleet-model";
import { fleetHref, type FleetParams } from "./fleet-route";
import {
  agentRows,
  ATTENTION_WORDS,
  ENDING_WORDS,
  leaseIndex,
  leaseView,
  type AgentRow,
  type LeaseView,
} from "./fleet-rows";
import { smartlogHref } from "./smartlog-route";
import { WorkspaceDetail } from "./WorkspaceDetail";

export { fleetHref, fleetParams, type FleetParams } from "./fleet-route";

const NO_AGENTS: string[] = [];

/** Agents named in a workspace's row before `+n`. */
const AGENTS_SHOWN = 3;

/**
 * Its children, with the one change transition each time `word` changes: a
 * state change, never a time, a count or a figure. `-` (presence resyncing,
 * nothing known) is not a state, so going to or from it does not count.
 */
function Word({ word, children }: { word: string; children?: ReactNode }) {
  // The previous word, held as state (React's "previous props" pattern).
  const [held, setHeld] = useState({ word, changes: 0 });
  if (held.word !== word) {
    const quiet = held.word === "-" || word === "-";
    setHeld({ word, changes: quiet ? held.changes : held.changes + 1 });
  }
  return (
    // Keyed by the count, so each change replays the transition.
    <span key={held.changes} className={held.changes > 0 ? "changed" : undefined}>
      {children ?? word}
    </span>
  );
}

function HealthWord({ health }: { health: Health }) {
  return (
    <span title={health.title} className={health.stuck ? "health stuck" : "health"}>
      <Word word={health.word} />
    </span>
  );
}

function ChangeCell({ repo, at, row }: { repo: string; at: string; row: WorkspaceRow }) {
  const id = row.workspace.changeId;
  const change = row.change;
  const subject = change?.description.split("\n", 1)[0] ?? "";
  return (
    <Word word={id}>
      {change && <StateBadge {...changeState(change)} />}{" "}
      <Link to={smartlogHref({ repo, at, change: id })} title={subject || reverseHex(id)}>
        <span className="mono">{shortId(reverseHex(id))}</span>
      </Link>
    </Word>
  );
}

function LeaseCell({ view, frozen, row }: { view: LeaseView; frozen: boolean; row: WorkspaceRow }) {
  const lease = view.lease;
  if (!lease) return <span className="secondary">-</span>;
  if (frozen) {
    const issued = clock(lease.issuedAt);
    return <span title={issued.title}>issued {issued.text}</span>;
  }
  const time = expiry(lease);
  return (
    <>
      <HealthWord health={view.health} />
      <div className="secondary" title={time.title}>
        {time.text}
      </div>
      {row.workspace.lease?.state === LeaseState.ENDED && <div className="secondary">ended</div>}
    </>
  );
}

function WorkspaceTableRow({
  repo,
  at,
  row,
  agents,
  view,
  disk,
  frozen,
  selected,
  register,
}: {
  repo: string;
  /** The operation the page is frozen at, or empty: links keep it. */
  at: string;
  row: WorkspaceRow;
  agents: string[];
  view: LeaseView;
  disk: { text: string; title: string };
  frozen: boolean;
  selected: boolean;
  register: (key: string, element: HTMLTableRowElement | null) => void;
}) {
  const workspace = row.workspace;
  const attention = ATTENTION_WORDS[workspace.attention];
  const cause = row.cause;
  const title = cause
    ? `${operationLabel(cause)} at ${clock(cause.endedAt).text} by ${cause.agent || "-"}`
    : undefined;
  const key = `ws:${row.id}`;
  const ended = row.ended;
  return (
    <tr
      ref={(element) => {
        register(key, element);
      }}
      className={`${row.arrived ? "changed" : ""}${selected ? " selected" : ""}`}
      aria-current={selected ? "true" : undefined}
      title={title}
    >
      <td>
        <Link to={fleetHref({ repo, at, workspace: row.id })} replace title={row.id}>
          <span className="mono">{shortId(row.id)}</span>
        </Link>
        {attention && (
          <>
            {" "}
            <span className="label">
              <Word word={attention} />
            </span>
          </>
        )}
        {workspace.state === WorkspaceState.RELEASING && !ended && (
          <span className="secondary"> releasing</span>
        )}
        <Word word={ended?.word ?? ""}>
          {ended && (
            <div>
              {ended.word}
              {ended.operation && (
                <span className="secondary">
                  {" "}
                  {clock(ended.operation.endedAt).text} by {ended.operation.agent || "-"}
                </span>
              )}
            </div>
          )}
        </Word>
      </td>
      <td>
        {workspace.owner}
        {workspace.purpose && (
          <div className="secondary one-line" title={workspace.purpose}>
            {workspace.purpose}
          </div>
        )}
      </td>
      <td>
        {agents.slice(0, AGENTS_SHOWN).map((name, i) => (
          <span key={name}>
            {i > 0 && ", "}
            <Link to={fleetHref({ repo, at, agent: name })} replace>
              {name}
            </Link>
          </span>
        ))}
        {agents.length > AGENTS_SHOWN && ` +${String(agents.length - AGENTS_SHOWN)}`}
        {agents.length === 0 && <span className="secondary">-</span>}
      </td>
      <td>
        <ChangeCell repo={repo} at={at} row={row} />
      </td>
      <td className="number">{ended ? "" : (workspace.changedFileCount ?? "-")}</td>
      <td className="number" title={disk.title}>
        {ended || frozen ? "" : disk.text}
      </td>
      <td>{ended ? "" : <LeaseCell view={view} frozen={frozen} row={row} />}</td>
      <td>
        {workspace.lastSnapshotAt ? (
          <time title={clock(workspace.lastSnapshotAt).title}>
            {clock(workspace.lastSnapshotAt).text}
          </time>
        ) : (
          <span className="secondary">none</span>
        )}
      </td>
    </tr>
  );
}

function AgentTableRow({
  repo,
  at,
  row,
  rows,
  selected,
  register,
}: {
  repo: string;
  /** The operation the page is frozen at, or empty: links keep it. */
  at: string;
  row: AgentRow;
  /** The workspaces' rows by id, for each workspace's current change. */
  rows: ReadonlyMap<string, WorkspaceRow>;
  selected: boolean;
  register: (key: string, element: HTMLTableRowElement | null) => void;
}) {
  const key = `agent:${row.name}`;
  const agent = row.agent;
  const link = (
    <Link to={fleetHref({ repo, at, agent: row.name })} replace>
      {row.name}
    </Link>
  );
  const common = {
    ref: (element: HTMLTableRowElement | null) => {
      register(key, element);
    },
    className: selected ? "selected" : undefined,
    "aria-current": selected ? ("true" as const) : undefined,
  };
  if (!agent) {
    const ending = row.ending;
    const word = ending ? ENDING_WORDS[ending.reason] : "ended";
    // The same row element as when it was live: the class starts the one
    // transition as it turns to its ended form.
    return (
      <tr {...common} className={selected ? "selected changed" : "changed"}>
        <td>{link}</td>
        <td colSpan={5}>
          {word}
          {ending && ` ${clock(timestampOf(ending.at)).text}`}
        </td>
      </tr>
    );
  }
  // Entries with only a repository name their change themselves.
  const loose = agent.entries.filter((p) => !p.workspaceId && p.changeId);
  return (
    <tr {...common}>
      <td>{link}</td>
      <td>
        {agent.workspaceIds.map((id, i) => (
          <span key={id}>
            {i > 0 && ", "}
            <Link to={fleetHref({ repo, at, workspace: id })} replace title={id}>
              <span className="mono">{shortId(id)}</span>
            </Link>
          </span>
        ))}
        {agent.workspaceIds.length === 0 && <span className="secondary">no workspace</span>}
      </td>
      <td>
        {agent.workspaceIds.map((id) => {
          const workspaceRow = rows.get(id);
          return workspaceRow ? (
            <div key={id}>
              <ChangeCell repo={repo} at={at} row={workspaceRow} />
            </div>
          ) : null;
        })}
        {loose.map((p) => (
          <div key={p.id}>
            <Link to={smartlogHref({ repo, at, change: p.changeId })}>
              <span className="mono">{shortId(reverseHex(p.changeId))}</span>
            </Link>
          </div>
        ))}
      </td>
      <td title={row.doingTitle || undefined}>
        <Word word={row.doing} />
      </td>
      <td>
        <HealthWord health={row.health} />
      </td>
      <td>{row.since && <time title={clock(row.since).title}>{clock(row.since).text}</time>}</td>
    </tr>
  );
}

const timestampOf = timestampFromMs;

/** The rows' elements, for the scroll anchor: a stable view over the current keys. */
function useTableAnchor(keys: string[], version: number, selected: string) {
  const [elements] = useState(() => new Map<string, HTMLTableRowElement>());
  const register = useMemo(
    () => (key: string, element: HTMLTableRowElement | null) => {
      if (element) elements.set(key, element);
      else elements.delete(key);
    },
    [elements],
  );
  const view = useMemo<AnchorView>(
    () => ({
      locate: (key) => elements.get(key)?.getBoundingClientRect().top,
      firstInView: () =>
        keys.find((key) => (elements.get(key)?.getBoundingClientRect().bottom ?? 0) > 0),
      rowHeight: (key) => elements.get(key)?.getBoundingClientRect().height ?? 0,
    }),
    [elements, keys],
  );
  useScrollAnchor(view, version, selected);
  return register;
}

export function Fleet({
  connection,
  live,
  frame,
  onSessionInvalid,
  params,
}: {
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
  /** The route's parameters; the app remounts the page when `repo` or `at` change. */
  params: FleetParams;
}) {
  const [model] = useState(
    () =>
      new FleetModel(connection, live.operations, live.presence, frame, onSessionInvalid, {
        repo: params.repo,
        at: params.at,
      }),
  );
  useEffect(() => model.start(), [model]);
  useEffect(() => {
    if (model.frozen) void model.selectAt(params.workspace);
    else model.select(params.workspace);
  }, [model, params.workspace]);
  const snapshot = useSyncExternalStore(model.subscribe, model.getSnapshot);
  useSyncExternalStore(
    (listener) => live.presence.subscribe(listener),
    () => live.presence.version,
  );
  const state = useSyncExternalStore(
    (listener) => live.subscribeState(listener),
    () => live.state,
  );
  const frozen = model.frozen;

  const repoId = snapshot.repository?.id ?? params.repo;
  const presence = live.presence;
  const entries: Presence[] = frozen ? [] : [...presence.entries.values()];
  const join = joinAgents(entries, repoId);
  const endings = frozen ? [] : [...presence.endings.values()];
  const index = leaseIndex(entries, endings);
  const rows = snapshot.rows;
  const byId = new Map((rows ?? []).map((row) => [row.id, row] as const));
  const views = new Map(
    (rows ?? []).map(
      (row) => [row.id, leaseView(row, index, presence.synced, snapshot.restarting)] as const,
    ),
  );
  const agents = agentRows(
    frozen ? NO_AGENTS : snapshot.agentOrder,
    join,
    endings.filter((e) => e.presence.repoId === repoId),
    (id) => (byId.get(id)?.ended ? undefined : views.get(id)?.health),
  );

  const selectedKey = params.workspace
    ? `ws:${params.workspace}`
    : params.agent
      ? `agent:${params.agent}`
      : "";
  const order = frozen ? NO_AGENTS : snapshot.agentOrder;
  const keys = useMemo(
    () => [...order.map((name) => `agent:${name}`), ...(rows ?? []).map((row) => `ws:${row.id}`)],
    [rows, order],
  );
  // Presence changes the Agents table's height too.
  const register = useTableAnchor(keys, snapshot.version + presence.version, selectedKey);

  if (snapshot.fatal) {
    return (
      <div className="fleet">
        <RepoNav repo={params.repo} current="workspaces" at={params.at} />
        <ErrorView error={snapshot.fatal} />
      </div>
    );
  }

  const root = snapshot.repository?.root;
  const name = root ? root.replace(/\/+$/, "").split("/").pop() || root : shortId(params.repo);
  const active = (rows ?? []).filter((row) => !row.ended);
  const endedCount = (rows ?? []).length - active.length;
  const stuck = active.filter((row) => views.get(row.id)?.health.stuck).length;
  const grace = active.filter(
    (row) => views.get(row.id)?.lease?.state === LeaseState.IN_GRACE,
  ).length;
  const attention = active.filter((row) => ATTENTION_WORDS[row.workspace.attention]).length;
  const gone = agents.filter((a) => !a.agent).map((a) => a.name);
  const summary = [
    `${String(active.length)} active`,
    stuck && `${String(stuck)} missed heartbeats`,
    grace && `${String(grace)} in grace`,
    attention && `${String(attention)} need attention`,
  ].filter(Boolean);

  const disk = (row: WorkspaceRow) => {
    const result = model.disk.results.get(row.id);
    const usage = result?.response?.diskUsage;
    if (result?.notMeasurable)
      return { text: "not measurable", title: "its base is no longer cached" };
    if (result?.error && !usage) return { text: "failed", title: result.error.message };
    if (!usage || result.measuredAt === undefined) return { text: "-", title: "not measured yet" };
    return {
      text: size(usage.allocatedBytes),
      title: `${String(usage.filesChanged)} files differ from the base, measured ${clock(timestampOf(result.measuredAt)).text}`,
    };
  };

  const selectedRow = params.workspace ? byId.get(params.workspace) : undefined;
  const selectedAgent = params.agent ? agents.find((a) => a.name === params.agent) : undefined;

  return (
    <div className="fleet">
      <RepoNav repo={params.repo} current="workspaces" at={params.at} />
      <h2>
        <span title={root}>{name}</span> · Workspaces and agents
      </h2>
      {frozen && (
        <AtBanner
          operation={snapshot.frozenAt}
          at={params.at}
          live={fleetHref({ ...params, at: "" })}
        />
      )}
      {!frozen && state !== "live" && snapshot.lastRefreshAt !== undefined && (
        <p className="secondary">
          Not live: the last update was at {clock(timestampOf(snapshot.lastRefreshAt)).text}.
        </p>
      )}
      {snapshot.error && <ErrorView error={snapshot.error} />}
      <div className="smartlog-body">
        <div>
          {!frozen && (
            <section aria-labelledby="agents-heading">
              <h3 id="agents-heading">Agents</h3>
              {rows === null ? (
                <p className="secondary">Loading workspaces.</p>
              ) : !presence.synced ? (
                <p className="secondary">Waiting for presence.</p>
              ) : agents.length === 0 ? (
                <p className="secondary">No agents are reporting presence in this repository.</p>
              ) : (
                <div className="table-box">
                  <table>
                    <caption className="visually-hidden">Agents</caption>
                    <thead>
                      <tr>
                        <th scope="col">Agent</th>
                        <th scope="col">Workspaces</th>
                        <th scope="col">Current change</th>
                        <th scope="col">Doing</th>
                        <th scope="col">Health</th>
                        <th scope="col">Since</th>
                      </tr>
                    </thead>
                    <tbody>
                      {agents.map((row) => (
                        <AgentTableRow
                          key={row.name}
                          repo={repoId}
                          at={params.at}
                          row={row}
                          rows={byId}
                          selected={params.agent === row.name && !params.workspace}
                          register={register}
                        />
                      ))}
                    </tbody>
                  </table>
                </div>
              )}
              {gone.length > 0 && (
                <button
                  type="button"
                  onClick={() => {
                    model.hideGone(gone);
                  }}
                >
                  Hide {gone.length} gone
                </button>
              )}
            </section>
          )}
          <section aria-labelledby="workspaces-heading">
            <h3 id="workspaces-heading">Workspaces</h3>
            {rows === null && !snapshot.error && <p className="secondary">Loading workspaces.</p>}
            {rows?.length === 0 && (
              <p>
                No active workspaces in this repository. <code>reef ws run -- &lt;command&gt;</code>{" "}
                runs a command in a new one.
              </p>
            )}
            {rows && rows.length > 0 && (
              <>
                {!frozen && <p className="secondary summary">{summary.join(" · ")}</p>}
                <div className="table-box">
                  <table>
                    <caption className="visually-hidden">Workspaces</caption>
                    <thead>
                      <tr>
                        <th scope="col">Workspace</th>
                        <th scope="col">Owner</th>
                        <th scope="col">Agents</th>
                        <th scope="col">Change</th>
                        <th scope="col">Files</th>
                        <th scope="col">Disk</th>
                        <th scope="col">Lease</th>
                        <th scope="col">Last snapshot</th>
                      </tr>
                    </thead>
                    <tbody>
                      {rows.map((row) => (
                        <WorkspaceTableRow
                          key={row.id}
                          repo={repoId}
                          at={params.at}
                          row={row}
                          agents={row.ended ? [] : agentsOfWorkspace(join, row.id)}
                          view={
                            views.get(row.id) ?? {
                              lease: undefined,
                              health: { word: "-", title: "", stuck: false },
                            }
                          }
                          disk={disk(row)}
                          frozen={frozen}
                          selected={params.workspace === row.id}
                          register={register}
                        />
                      ))}
                    </tbody>
                  </table>
                </div>
                {snapshot.more && (
                  <p className="secondary">
                    Showing the newest {WORKSPACES_SHOWN} active workspaces.
                  </p>
                )}
                {endedCount > 0 && (
                  <button
                    type="button"
                    onClick={() => {
                      model.hideEnded();
                    }}
                  >
                    Hide {endedCount} ended
                  </button>
                )}
              </>
            )}
          </section>
        </div>
        <aside className="detail-pane" aria-label="Selection" aria-live="polite">
          {params.workspace && selectedRow && (
            <WorkspaceDetail
              repo={repoId}
              at={params.at}
              row={selectedRow}
              result={frozen ? undefined : model.disk.results.get(selectedRow.id)}
              frozenResponse={frozen ? snapshot.frozenWorkspace : undefined}
              view={views.get(selectedRow.id)}
              agents={selectedRow.ended ? [] : agentsOfWorkspace(join, selectedRow.id)}
              entries={entries.filter((p) => p.workspaceId === selectedRow.id)}
              endings={endings.filter((e) => e.presence.workspaceId === selectedRow.id)}
              byEntry={join.byEntry}
              frozen={frozen}
              remote={!isLoopback()}
              onMeasure={() => {
                model.disk.measureAgain(selectedRow.id);
              }}
            />
          )}
          {params.agent && !params.workspace && selectedAgent && (
            <AgentDetail
              repo={repoId}
              at={params.at}
              row={selectedAgent}
              rows={byId}
              endings={endings.filter(
                (e) => e.presence.repoId === repoId && e.presence.agent === params.agent,
              )}
            />
          )}
          {((params.workspace && !selectedRow) ||
            (params.agent && !params.workspace && !selectedAgent)) && (
            <div className="detail">
              <p>Not active in this repository.</p>
              {params.workspace && (
                <p>
                  <code>reef ws inspect {params.workspace}</code>
                </p>
              )}
            </div>
          )}
        </aside>
      </div>
    </div>
  );
}

function isLoopback(): boolean {
  const host = window.location.hostname;
  return host === "127.0.0.1" || host === "localhost" || host === "[::1]" || host === "::1";
}
