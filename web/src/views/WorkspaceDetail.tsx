// The selected workspace (R2.20): its `GetWorkspace` from the disk scheduler
// (first in its queue, again whenever an operation touches it), or its row
// until that answers; at an operation, `GetWorkspace` at it, without disk.
// All text is plain text: nothing from the API is rendered as HTML.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import type { Presence } from "../gen/reef/api/v1/presence_pb";
import {
  Backend,
  StoreMountMode,
  WatchState,
  WatcherKind,
  WorkspaceState,
  type GetWorkspaceResponse,
  type Lease,
} from "../gen/reef/api/v1/workspaces_pb";
import type { DiskResult } from "../fleet/disk";
import type { Ending } from "../live/presence";
import { changeState } from "../state/states";
import { StateBadge } from "../state/StateBadge";
import { Copy } from "../ui/Copy";
import { ErrorView } from "../ui/ErrorView";
import { reverseHex, shortId } from "../ui/ids";
import { operationLabel } from "../ui/operation";
import { Link } from "../ui/router";
import { size } from "../ui/size";
import { clock, dateTime } from "../ui/time";
import type { WorkspaceRow } from "./fleet-model";
import { fleetHref } from "./fleet-route";
import {
  ATTENTION_MEANINGS,
  ATTENTION_WORDS,
  doing,
  ENDING_WORDS,
  type LeaseView,
} from "./fleet-rows";
import { smartlogHref } from "./smartlog-route";

const PATHS_SHOWN = 20;
const ENDINGS_SHOWN = 10;
const SNAPSHOTS_SHOWN = 10;

const STATES: Partial<Record<WorkspaceState, string>> = {
  [WorkspaceState.ACTIVE]: "active",
  [WorkspaceState.RELEASING]: "releasing",
  [WorkspaceState.RELEASED]: "released",
  [WorkspaceState.RECLAIMED]: "reclaimed",
};

const BACKENDS: Partial<Record<Backend, string>> = {
  [Backend.COPY]: "copy",
  [Backend.APFS]: "APFS clone",
  [Backend.REFLINK]: "reflink",
  [Backend.OVERLAYFS]: "overlayfs",
  [Backend.FUSE_OVERLAYFS]: "fuse-overlayfs",
};

const WATCHERS: Partial<Record<WatcherKind, string>> = {
  [WatcherKind.NOTIFY]: "notify",
  [WatcherKind.WATCHMAN]: "Watchman",
};

const WATCH_STATES: Partial<Record<WatchState, string>> = {
  [WatchState.ATTACHED]: "attached",
  [WatchState.WAITING]: "waiting",
  [WatchState.ATTACHING]: "attaching",
};

const MODES: Partial<Record<StoreMountMode, string>> = {
  [StoreMountMode.SYMLINK]: "symlink",
  [StoreMountMode.BIND]: "bind mount",
};

function seconds(duration: { seconds: bigint; nanos: number } | undefined): string {
  return duration ? `${String(Number(duration.seconds) + duration.nanos / 1e9)} s` : "-";
}

function Time({ at }: { at: Parameters<typeof clock>[0] }) {
  const time = dateTime(at);
  return <time title={time.title}>{time.text}</time>;
}

function LeaseFacts({ lease, view }: { lease: Lease; view: LeaseView | undefined }) {
  return (
    <dl className="facts">
      <dt>Lease</dt>
      <dd className="mono">{lease.id}</dd>
      <dt>Holder</dt>
      <dd>{lease.owner}</dd>
      <dt>Issued</dt>
      <dd>
        <Time at={lease.issuedAt} />
      </dd>
      <dt>Expires</dt>
      <dd>
        <Time at={lease.expiresAt} />
      </dd>
      <dt>Grace</dt>
      <dd>{seconds(lease.grace)}</dd>
      <dt>Heartbeat</dt>
      <dd>every {seconds(lease.heartbeatEvery)}</dd>
      <dt>Last heartbeat</dt>
      <dd>
        <Time at={lease.lastHeartbeatAt} />
      </dd>
      {view && view.health.word !== "-" && (
        <>
          <dt>Health</dt>
          <dd>
            {view.health.word}
            {view.health.title && <span className="secondary"> ({view.health.title})</span>}
          </dd>
        </>
      )}
    </dl>
  );
}

export function WorkspaceDetail({
  repo,
  at,
  row,
  result,
  frozenResponse,
  view,
  agents,
  entries,
  endings,
  byEntry,
  frozen,
  remote,
  onMeasure,
}: {
  repo: string;
  /** The operation the page is frozen at, or empty: links keep it. */
  at: string;
  row: WorkspaceRow;
  /** Live: the disk scheduler's answer. */
  result: DiskResult | undefined;
  /** At an operation: `GetWorkspace` at it. */
  frozenResponse: GetWorkspaceResponse | undefined;
  view: LeaseView | undefined;
  agents: string[];
  /** Its live presence entries. */
  entries: Presence[];
  /** Its entries that ended while the page was open. */
  endings: Ending[];
  byEntry: ReadonlyMap<string, string[]>;
  frozen: boolean;
  /** The page is not on loopback: paths are on the daemon's machine. */
  remote: boolean;
  onMeasure: () => void;
}) {
  // The record is the row's, from the last listing: `GetWorkspace` leaves
  // `changed_file_count` unset, and the scheduler's answer may be older. The
  // answer gives only disk, snapshots and the other workspaces on the change.
  const answer = frozen ? frozenResponse : result?.response;
  const response = answer?.workspace?.id === row.id ? answer : undefined;
  const workspace = row.ended?.workspace ?? row.workspace;
  const change = row.change;
  const attention = ATTENTION_WORDS[workspace.attention];
  const usage = response?.diskUsage;
  const command = `reef ws inspect ${workspace.id}`;
  const lease = view?.lease ?? workspace.lease;
  return (
    <div className="detail">
      <h3>
        <span className="mono">{shortId(workspace.id)}</span>{" "}
        <span>{row.ended?.word ?? STATES[workspace.state] ?? "unknown"}</span>
      </h3>
      {attention && (
        <p>
          <strong>{attention}</strong>: {ATTENTION_MEANINGS[workspace.attention]}
        </p>
      )}
      <dl className="facts">
        <dt>Id</dt>
        <dd className="mono">{workspace.id}</dd>
        <dt>Owner</dt>
        <dd>{workspace.owner}</dd>
        {workspace.purpose && (
          <>
            <dt>Purpose</dt>
            <dd className="pre-wrap">{workspace.purpose}</dd>
          </>
        )}
        <dt>Created</dt>
        <dd>
          <Time at={workspace.createdAt} />
        </dd>
        <dt>Path</dt>
        <dd>
          <span className="mono">{workspace.path}</span> <Copy text={workspace.path} />
          {remote && <div className="secondary">on the daemon's machine</div>}
        </dd>
        <dt>Backend</dt>
        <dd>{BACKENDS[workspace.backend] ?? "-"}</dd>
        <dt>Watcher</dt>
        <dd>
          {WATCHERS[workspace.watcher] ?? "-"}
          {WATCH_STATES[workspace.watchState] && `, ${WATCH_STATES[workspace.watchState] ?? ""}`}
        </dd>
        {workspace.remoteTokenId && (
          <>
            <dt>Remote token</dt>
            <dd className="mono">{workspace.remoteTokenId}</dd>
          </>
        )}
        <dt>Agents</dt>
        <dd>
          {agents.length === 0 && <span className="secondary">none reporting</span>}
          {agents.map((name, i) => (
            <span key={name}>
              {i > 0 && ", "}
              <Link to={fleetHref({ repo, at, agent: name })} replace>
                {name}
              </Link>
            </span>
          ))}
        </dd>
      </dl>
      <h4>Change</h4>
      <dl className="facts">
        <dt>Change</dt>
        <dd>
          {change && <StateBadge {...changeState(change)} />}{" "}
          <span className="mono">{reverseHex(workspace.changeId)}</span>
          <div className="secondary mono">{workspace.changeId}</div>
        </dd>
        <dt>Commit</dt>
        <dd className="mono">{workspace.commitId}</dd>
        {change && (
          <>
            <dt>Description</dt>
            <dd>{change.description.split("\n", 1)[0] || "(no description)"}</dd>
          </>
        )}
        {response && response.otherWorkspacesOnChange.length > 0 && (
          <>
            <dt>Also on it</dt>
            <dd>
              {response.otherWorkspacesOnChange.map((id) => (
                <div key={id}>
                  <Link to={fleetHref({ repo, at, workspace: id })} replace>
                    <span className="mono">{shortId(id)}</span>
                  </Link>
                </div>
              ))}
            </dd>
          </>
        )}
      </dl>
      <p>
        <Link to={smartlogHref({ repo, at, change: workspace.changeId })}>
          Open in the smartlog
        </Link>
      </p>
      {!row.ended && (
        <>
          <h4>Files and disk</h4>
          <dl className="facts">
            <dt>Files changed</dt>
            <dd>{workspace.changedFileCount ?? "-"}</dd>
            {!frozen && (
              <>
                <dt>Disk</dt>
                <dd>
                  {result?.notMeasurable
                    ? "not measurable: its base is no longer cached"
                    : usage
                      ? `${size(usage.allocatedBytes)}, ${String(usage.filesChanged)} files differ from the base`
                      : "not measured yet"}
                  {result?.measuredAt !== undefined && (
                    <div className="secondary">
                      measured {clock(timestampFromMs(result.measuredAt)).text}
                    </div>
                  )}{" "}
                  <button type="button" onClick={onMeasure}>
                    Measure again
                  </button>
                </dd>
              </>
            )}
          </dl>
          {result?.error && <ErrorView error={result.error} />}
        </>
      )}
      {lease && (
        <>
          <h4>Lease</h4>
          {frozen ? (
            <p>
              Issued <Time at={lease.issuedAt} /> to {lease.owner}. Leases are replayed as issued.
            </p>
          ) : (
            <LeaseFacts lease={lease} view={view} />
          )}
        </>
      )}
      {response && (
        <>
          <h4>Snapshots</h4>
          <p>
            {String(response.snapshotCount)} in all, {String(workspace.localSnapshotCount)} local.
          </p>
          <ul className="activity">
            {response.recentSnapshots.slice(0, SNAPSHOTS_SHOWN).map((s) => (
              <li key={s.operationId}>
                <Time at={s.takenAt} /> · <span className="mono">{shortId(s.commitId)}</span> ·
                operation <span className="mono">{shortId(s.operationId)}</span>
              </li>
            ))}
          </ul>
        </>
      )}
      {workspace.stores.length > 0 && (
        <>
          <h4>Stores</h4>
          <ul className="activity">
            {workspace.stores.map((store) => (
              <li key={store.name}>
                {store.name} at <span className="mono">{store.mount}</span>,{" "}
                {MODES[store.mode] ?? "-"}
                {store.verified ? ", verified" : ", not verified"}
              </li>
            ))}
          </ul>
        </>
      )}
      {!frozen && entries.length > 0 && (
        <>
          <h4>Presence now</h4>
          <ul className="activity">
            {entries.map((p) => (
              <li key={p.id}>
                {(byEntry.get(p.id) ?? [p.agent]).join(", ")} · {doing(p)}
                {p.paths.length > 0 && (
                  <div className="secondary">
                    {p.paths.slice(0, PATHS_SHOWN).join(", ")}
                    {p.paths.length > PATHS_SHOWN &&
                      ` and ${String(p.paths.length - PATHS_SHOWN)} more`}
                  </div>
                )}
                <span className="secondary"> · {clock(p.updatedAt).text}</span>
              </li>
            ))}
          </ul>
        </>
      )}
      {!frozen && endings.length > 0 && (
        <>
          <h4>Ended since you opened</h4>
          <ul className="activity">
            {endings
              .slice(-ENDINGS_SHOWN)
              .reverse()
              .map((e) => (
                <li key={e.presence.id}>
                  {e.presence.agent} · {doing(e.presence)} · {ENDING_WORDS[e.reason]}{" "}
                  {clock(timestampFromMs(e.at)).text}
                </li>
              ))}
          </ul>
        </>
      )}
      {row.cause && (
        <p className="secondary">
          Last: {operationLabel(row.cause)} by {row.cause.agent || "-"} at{" "}
          {clock(row.cause.endedAt).text}, operation{" "}
          <span className="mono">{shortId(row.cause.id)}</span>
        </p>
      )}
      <p className="command">
        <code>{command}</code> <Copy text={command} />
      </p>
    </div>
  );
}
