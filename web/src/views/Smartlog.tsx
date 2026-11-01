// `/repos/<repo>/smartlog` (R2.19): a revset's changes as a live graph,
// stacks off a trunk spine, with the selected change's details beside it.
// The query carries `revset`, `limit`, `change` (reverse hex) and `at` (an
// operation: the view at that point, frozen).
//
// Rows are one listbox of options, with a roving tabindex; the graph is
// decoration and each row's accessible name says what it shows. Stacks keep
// their places (order.ts), and the row in view stays where it is on screen
// across a refresh: nothing reorders under the cursor.
import {
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type KeyboardEvent,
  type RefObject,
} from "react";
import type { Connection } from "../api/connection";
import type { ReefError } from "../api/errors";
import type { Presence } from "../gen/reef/api/v1/presence_pb";
import type { Workspace } from "../gen/reef/api/v1/workspaces_pb";
import { DEFAULT_LIMIT, logCommand, MAX_LIMIT, SMARTLOG_REVSET } from "../graph/revset";
import { joinAgents, type AgentJoin } from "../fleet/agents";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { AtBanner } from "../ui/AtBanner";
import { Copy } from "../ui/Copy";
import { ErrorView } from "../ui/ErrorView";
import { RepoNav } from "../ui/RepoNav";
import { useScrollAnchor, type AnchorView } from "../ui/scroll-anchor";
import { operationLabel } from "../ui/operation";
import { reverseHex, shortId } from "../ui/ids";
import { Link, navigate } from "../ui/router";
import { clock, dateTime } from "../ui/time";
import { ChangeDetail } from "./ChangeDetail";
import { fleetHref } from "./fleet-route";
import { GraphCell, LANE_WIDTH, OpenLanes, ROW_HEIGHT } from "./GraphCell";
import { INVALID_REVSET, SmartlogModel, type SmartlogRow } from "./smartlog-model";
import { smartlogHref, smartlogParams, type SmartlogParams } from "./smartlog-route";

/** Agents named on a row before `+n`. */
const AGENTS_SHOWN = 3;

export { smartlogHref, smartlogParams, type SmartlogParams };

/** Distinct agents present on each change: on it, or on one of its workspaces. */
function presentAgents(
  presence: Iterable<Presence>,
  repo: string,
  rows: SmartlogRow[],
): Map<string, Presence[]> {
  const byWorkspace = new Map<string, string>();
  for (const row of rows) {
    for (const id of row.change.workspaceIds) byWorkspace.set(id, row.change.changeId);
  }
  const out = new Map<string, Presence[]>();
  for (const entry of presence) {
    if (entry.repoId !== repo) continue;
    const change = entry.changeId || byWorkspace.get(entry.workspaceId);
    if (!change) continue;
    let list = out.get(change);
    if (!list) out.set(change, (list = []));
    list.push(entry);
  }
  return out;
}

/** The agents of a change's entries, each once (fleet/agents.ts). */
function agentNames(entries: Presence[] | undefined, join: AgentJoin): string[] {
  return [...new Set((entries ?? []).flatMap((p) => join.byEntry.get(p.id) ?? [p.agent]))].sort();
}

/** What the row shows, in words, for its accessible name. */
function accessibleName(row: SmartlogRow, agents: string[]): string {
  const change = row.change;
  const parts = [row.state.label, `change ${shortId(reverseHex(change.changeId))}`];
  if (change.bookmarks.length) parts.push(`bookmarks ${change.bookmarks.join(", ")}`);
  if (change.tags.length) parts.push(`tags ${change.tags.join(", ")}`);
  if (change.workspaceIds.length) {
    parts.push(`workspaces ${change.workspaceIds.map((id) => shortId(id)).join(", ")}`);
  }
  if (agents.length) parts.push(`agents ${agents.join(", ")}`);
  if (change.empty) parts.push("empty");
  parts.push(change.description.split("\n", 1)[0] || "no description");
  const kinds = new Set(row.graph.node.edges.map((e) => e.kind));
  if (kinds.has("indirect")) parts.push("joins its parent through left-out commits");
  if (kinds.has("missing")) parts.push("parent outside this view");
  return parts.join(", ");
}

function Row({
  row,
  width,
  agents,
  workspaces,
  selected,
  focusable,
  onSelect,
  register,
}: {
  row: SmartlogRow;
  width: number;
  agents: string[];
  workspaces: ReadonlyMap<string, Workspace>;
  selected: boolean;
  focusable: boolean;
  onSelect: (row: SmartlogRow) => void;
  register: (commit: string, element: HTMLLIElement | null) => void;
}) {
  const change = row.change;
  const time = dateTime(change.committer?.at);
  const draft = row.state.state === "draft";
  const cause = row.cause;
  // The state's word is always in the title, for draft rows too (decision 3).
  const title = cause
    ? `${row.state.label}; changed at ${clock(cause.endedAt).text} by ${cause.agent || "-"}, ${operationLabel(cause)}`
    : row.state.label;
  return (
    <li
      role="option"
      aria-selected={selected}
      aria-label={accessibleName(row, agents)}
      tabIndex={focusable ? 0 : -1}
      className={`row${row.changed ? " changed" : ""}${selected ? " selected" : ""}`}
      data-commit={change.commitId}
      ref={(element) => {
        register(change.commitId, element);
      }}
      onClick={() => {
        onSelect(row);
      }}
      title={title}
    >
      <span className="graph-cell" style={{ width: Math.max(width, 1) * LANE_WIDTH }}>
        <GraphCell row={row.graph} width={width} state={row.state.state} />
      </span>
      <span className="row-text">
        {!draft && (
          <span className="state-word" style={{ color: `var(--state-${row.state.state})` }}>
            {row.state.label}
          </span>
        )}
        <span className="mono" title={reverseHex(change.changeId)}>
          {shortId(reverseHex(change.changeId))}
        </span>
        <span className="mono secondary">{shortId(change.commitId)}</span>
        {change.bookmarks.map((name) => (
          <span key={name} className={`label${name.includes("@") ? " secondary" : ""}`}>
            {name}
          </span>
        ))}
        {change.tags.map((name) => (
          <span key={name} className="label">
            tag:{name}
          </span>
        ))}
        {change.workspaceIds.map((id) => {
          const owner = workspaces.get(id)?.owner;
          return (
            <span key={id} className="label">
              ws <span className="mono">{shortId(id)}</span>
              {owner ? ` ${owner}` : ""}
            </span>
          );
        })}
        {change.jjWorkspaces.map((name) => (
          <span key={name} className="label">
            jj {name}
          </span>
        ))}
        {agents.slice(0, AGENTS_SHOWN).map((agent) => (
          <span key={agent} className="label agent">
            {agent}
          </span>
        ))}
        {agents.length > AGENTS_SHOWN && (
          <span className="label agent">+{agents.length - AGENTS_SHOWN}</span>
        )}
        {change.empty && <span className="secondary">empty</span>}
        <span className="secondary subject">
          {change.description.split("\n", 1)[0] || "(no description)"}
        </span>
        <time className="secondary" title={time.title}>
          {time.text}
        </time>
      </span>
    </li>
  );
}

/**
 * The smartlog's rows for the scroll anchor (ui/scroll-anchor.ts). Rows are
 * all `ROW_HEIGHT` high, so a row's place is the list's top plus its index:
 * one measurement per scroll event, whatever the number of rows.
 */
function useRowAnchor(
  list: RefObject<HTMLUListElement | null>,
  changes: string[],
  version: number,
  selectedChange: string,
) {
  const view = useMemo<AnchorView>(
    () => ({
      locate: (change) => {
        const index = changes.indexOf(change);
        const element = list.current;
        if (index < 0 || !element) return undefined;
        return element.getBoundingClientRect().top + index * ROW_HEIGHT;
      },
      firstInView: () => {
        const element = list.current;
        if (!element || changes.length === 0) return undefined;
        const top = element.getBoundingClientRect().top;
        return changes[Math.min(changes.length - 1, Math.max(0, Math.ceil(-top / ROW_HEIGHT)))];
      },
      rowHeight: () => ROW_HEIGHT,
    }),
    [list, changes],
  );
  useScrollAnchor(view, version, selectedChange);
}

function RevsetField({ params, error }: { params: SmartlogParams; error: ReefError | null }) {
  const [text, setText] = useState(params.revset);
  return (
    <form
      className="revset"
      onSubmit={(event) => {
        event.preventDefault();
        navigate(smartlogHref({ ...params, revset: text.trim() || SMARTLOG_REVSET, change: "" }));
      }}
    >
      <label htmlFor="revset">Revset</label>
      <input
        id="revset"
        className="mono"
        value={text}
        spellCheck={false}
        aria-invalid={error !== null}
        aria-describedby={error ? "revset-error" : undefined}
        onChange={(event) => {
          setText(event.target.value);
        }}
      />
      <button type="submit">Show</button>
      {error && (
        <div id="revset-error" className="field-error">
          <ErrorView error={error} />
        </div>
      )}
    </form>
  );
}

export function Smartlog({
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
  /** The route's parameters; the app remounts the view when `repo` or `at` change. */
  params: SmartlogParams;
}) {
  const [model] = useState(
    () =>
      new SmartlogModel(connection, live.operations, frame, onSessionInvalid, {
        repo: params.repo,
        revset: params.revset,
        limit: params.limit,
        at: params.at,
      }),
  );
  useEffect(() => model.start(), [model]);
  useEffect(() => {
    model.setRevset(params.revset);
  }, [model, params.revset]);
  const snapshot = useSyncExternalStore(model.subscribe, model.getSnapshot);
  const frozen = model.frozen;
  useSyncExternalStore(
    (listener) => live.presence.subscribe(listener),
    () => live.presence.version,
  );

  const rows = snapshot.rows;
  const [elements] = useState(() => new Map<string, HTMLLIElement>());
  const register = useCallback(
    (commit: string, element: HTMLLIElement | null) => {
      if (element) elements.set(commit, element);
      else elements.delete(commit);
    },
    [elements],
  );
  const list = useRef<HTMLUListElement>(null);
  const changes = useMemo(() => (rows ?? []).map((r) => r.change.changeId), [rows]);
  useRowAnchor(list, changes, snapshot.version, params.change);

  // The commit with focus: the selected change's first row unless the
  // keyboard moved to another of its commits.
  const [focusCommit, setFocusCommit] = useState("");
  const selectedRows = rows?.filter((r) => r.change.changeId === params.change) ?? [];
  const focused =
    selectedRows.find((r) => r.change.commitId === focusCommit) ?? selectedRows[0] ?? rows?.[0];
  const moveFocus = useRef(false);

  const select = useCallback(
    (row: SmartlogRow | undefined) => {
      setFocusCommit(row?.change.commitId ?? "");
      // Clearing the selection leaves focus where it is.
      moveFocus.current = row !== undefined;
      navigate(smartlogHref({ ...params, change: row?.change.changeId ?? "" }), { replace: true });
    },
    [params],
  );

  // Whether focus is in the list. A rewrite gives a row a new key, and the
  // browser drops focus to the body when its element goes: it comes back to
  // the focused row, without scrolling.
  const listHasFocus = useRef(false);
  useEffect(() => {
    if (!focused) return;
    const element = elements.get(focused.change.commitId);
    if (moveFocus.current) {
      moveFocus.current = false;
      element?.focus();
    } else if (listHasFocus.current && document.activeElement === document.body) {
      element?.focus({ preventScroll: true });
    }
  });

  function onKeyDown(event: KeyboardEvent<HTMLUListElement>) {
    if (!rows?.length) return;
    // Without a selection in view, ArrowDown starts at the first row.
    const index = selectedRows.length && focused ? rows.indexOf(focused) : -1;
    const go = (i: number) => {
      event.preventDefault();
      select(rows[Math.max(0, Math.min(rows.length - 1, i))]);
    };
    switch (event.key) {
      case "ArrowDown":
        go(index + 1);
        break;
      case "ArrowUp":
        go(Math.max(index - 1, 0));
        break;
      case "Home":
        go(0);
        break;
      case "End":
        go(rows.length - 1);
        break;
      case "Escape":
        event.preventDefault();
        select(undefined);
        break;
    }
  }

  const workspaces = new Map(
    (snapshot.status?.activeWorkspaces ?? []).map((w) => [w.id, w] as const),
  );
  const repoId = snapshot.repository?.id ?? params.repo;
  const join = frozen ? joinAgents([], repoId) : joinAgents(live.presence.entries.values(), repoId);
  const presence = frozen
    ? new Map<string, Presence[]>()
    : presentAgents(live.presence.entries.values(), repoId, rows ?? []);
  const error = snapshot.error;
  const revsetError = error?.reason === INVALID_REVSET ? error : null;
  const command = logCommand({
    repo: params.repo,
    revset: params.revset,
    limit: snapshot.limit,
    at: frozen ? snapshot.atOperation || params.at : undefined,
  });
  const root = snapshot.repository?.root;
  const name = root ? root.replace(/\/+$/, "").split("/").pop() || root : shortId(params.repo);
  const status = snapshot.status;

  return (
    <div className="smartlog">
      <RepoNav repo={params.repo} current="smartlog" at={params.at} />
      <h2>
        <span title={root}>{name}</span> · Smartlog
      </h2>
      {status && (
        <p className="secondary summary">
          {status.trunkBookmark ? `trunk is ${status.trunkBookmark}` : "no trunk bookmark"}
          {status.conflictedChangeIds.length > 0 &&
            ` · ${String(status.conflictedChangeIds.length)} conflicted`}
          {status.divergentChangeIds.length > 0 &&
            ` · ${String(status.divergentChangeIds.length)} divergent`}
          {" · "}
          <Link to={fleetHref({ repo: snapshot.repository?.id ?? params.repo, at: params.at })}>
            {status.activeWorkspaceCount} active workspaces
          </Link>
        </p>
      )}
      {frozen && (
        <AtBanner
          operation={snapshot.frozenAt}
          at={params.at}
          live={smartlogHref({ ...params, at: "" })}
        />
      )}
      <RevsetField key={params.revset} params={params} error={revsetError} />
      <p className="command">
        <code>{command}</code> <Copy text={command} />
      </p>
      {!frozen && (
        <p className="secondary note">
          Stacks keep their place while you watch; <code>reef log</code> lists the most recently
          changed first.
        </p>
      )}
      {error && !revsetError && <ErrorView error={error} />}
      {error && rows && snapshot.atOperation && (
        <p className="secondary">
          Showing the last graph read
          {snapshot.revset !== params.revset && (
            <>
              , of <code>{snapshot.revset}</code>
            </>
          )}
          , at operation <span className="mono">{shortId(snapshot.atOperation)}</span>.
        </p>
      )}
      {rows === null && !error && <p className="secondary">Reading the changes.</p>}
      {rows?.length === 0 && <p>No changes match this revset.</p>}
      {rows && rows.length > 0 && (
        <div className="smartlog-body">
          <div className="graph-box">
            <ul
              ref={list}
              className="rows"
              role="listbox"
              aria-label="Changes"
              onKeyDown={onKeyDown}
              onFocus={() => {
                listHasFocus.current = true;
              }}
              onBlur={(event) => {
                if (!event.currentTarget.contains(event.relatedTarget)) {
                  listHasFocus.current = false;
                }
              }}
            >
              {rows.map((row) => {
                const isSelected = row.change.changeId === params.change;
                return (
                  <Row
                    key={row.change.commitId}
                    row={row}
                    width={snapshot.width}
                    agents={agentNames(presence.get(row.change.changeId), join)}
                    workspaces={workspaces}
                    selected={isSelected}
                    focusable={row === focused}
                    onSelect={select}
                    register={register}
                  />
                );
              })}
            </ul>
            {snapshot.more && (
              <div className="more">
                {snapshot.open.length > 0 && (
                  <OpenLanes open={snapshot.open} width={snapshot.width} />
                )}
                <span className="secondary">More changes than shown.</span>{" "}
                {snapshot.limit < MAX_LIMIT && !frozen ? (
                  <button
                    type="button"
                    onClick={() => {
                      const limit = model.showMore();
                      navigate(smartlogHref({ ...params, limit }), { replace: true });
                    }}
                  >
                    Show {DEFAULT_LIMIT} more
                  </button>
                ) : (
                  <span className="secondary">Narrow the revset to see more.</span>
                )}
              </div>
            )}
          </div>
          <p className="visually-hidden" aria-live="polite">
            {selectedRows[0]
              ? `Selected ${shortId(reverseHex(params.change))}, ${selectedRows[0].state.label}`
              : ""}
          </p>
          <aside className="detail-pane" aria-label="Selected change">
            {params.change && selectedRows.length > 0 && (
              <ChangeDetail
                connection={connection}
                repo={snapshot.repository?.id ?? params.repo}
                rows={selectedRows}
                workspaces={workspaces}
                presence={presence.get(params.change) ?? []}
                agentsOf={join.byEntry}
                at={snapshot.atOperation}
                frozen={frozen}
                onSelect={select}
                onSessionInvalid={onSessionInvalid}
              />
            )}
            {params.change && selectedRows.length === 0 && (
              <p className="secondary">
                This change is not in this view. <code>{command}</code> lists what it shows.
              </p>
            )}
          </aside>
        </div>
      )}
    </div>
  );
}
