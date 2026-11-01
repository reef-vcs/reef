// `/repos/<repo>/activity` (R2.21): the operation log as a live, coalesced
// feed, from the repository's window (log/window.ts), which it shares with
// the scrubber. The query carries `op` (a selected operation) and `at` (the
// feed at that point: the point and its ancestors in the window, or one page
// ending at it when it is older than the window holds).
//
// Every count on the page is over operations the window holds once each:
// the rows', the summary's, the held bar's and the scrubber's bars. While
// the first row is out of view, or focus is in the table, new operations
// wait behind a count, so nothing moves under the reader; while following,
// new rows push the table down, so no scroll anchor is kept.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import {
  useEffect,
  useLayoutEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type RefObject,
} from "react";
import type { Connection } from "../api/connection";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { coalesce, userOf, type Row } from "../log/coalesce";
import { summarize } from "../log/counts";
import { newerFirst, type OpRecord } from "../log/record";
import { PAGE_SIZE, WINDOW_CAP, type OperationWindow } from "../log/window";
import { OperationKind, SnapshotKind } from "../gen/reef/api/v1/operations_pb";
import { AtBanner } from "../ui/AtBanner";
import { Identity } from "../ui/Avatar";
import { Copy } from "../ui/Copy";
import { useDevMode } from "../ui/dev-mode";
import { count } from "../ui/count";
import { ErrorView } from "../ui/ErrorView";
import { shortId } from "../ui/ids";
import { operationLabel } from "../ui/operation";
import { Link } from "../ui/router";
import { span } from "../ui/relative";
import { clock, dateTime, fullTime } from "../ui/time";
import { Time } from "../ui/TimeLabel";
import { capitalize, operationSentence } from "../ui/words";
import { activityHref, type ActivityParams } from "./activity-route";
import { fleetHref } from "./fleet-route";
import { OperationCache, OperationDetail } from "./OperationDetail";

export { activityHref, activityParams, type ActivityParams } from "./activity-route";

/**
 * An operation id in a printed command: a prefix the CLI resolves, 64 bits,
 * so the command fits a line.
 */
const commandId = (id: string) => id.slice(0, 16);

/** Rows rendered at first, and per "Show more". */
export const ROWS_SHOWN = 200;

const SNAPSHOT_WORDS: Partial<Record<SnapshotKind, string>> = {
  [SnapshotKind.WATCHER]: "watcher",
  [SnapshotKind.EXPLICIT]: "explicit",
  [SnapshotKind.FINAL]: "final",
  [SnapshotKind.MOVE]: "move",
  [SnapshotKind.SHUTDOWN]: "shutdown",
};

const at = (ms: number) => timestampFromMs(ms);

function plural(n: number, one: string, many: string): string {
  return `${count(n)} ${n === 1 ? one : many}`;
}

/** A run's "What": its counts, and the detail for `title`. */
function runWhat(row: Row): { text: string; title: string } {
  if (row.class === "outside") {
    return {
      text: plural(row.operations.length, "jj operation", "jj operations"),
      title: row.operations
        .slice(0, 3)
        .map((o) => o.description.split("\n", 1)[0])
        .join("\n"),
    };
  }
  let snapshots = 0;
  let edits = 0;
  const kinds = new Map<string, number>();
  for (const o of row.operations) {
    if (o.kind === OperationKind.EDIT) edits += 1;
    else {
      snapshots += 1;
      const word = SNAPSHOT_WORDS[o.snapshotKind] ?? "other";
      kinds.set(word, (kinds.get(word) ?? 0) + 1);
    }
  }
  const parts = [];
  if (snapshots) parts.push(plural(snapshots, "snapshot", "snapshots"));
  if (edits) parts.push(plural(edits, "edit", "edits"));
  return {
    text: parts.join(", "),
    title: [...kinds].map(([word, n]) => `${count(n)} ${word}`).join(", "),
  };
}

/** `14:03:22`, with the date when it is not the newest operation's. */
function When({ ms, day }: { ms: number; day: string }) {
  const date = dateTime(at(ms));
  const time = clock(at(ms));
  const sameDay = date.text.slice(0, 10) === day;
  return (
    <time title={time.title}>{sameDay ? time.text : `${date.text.slice(0, 10)} ${time.text}`}</time>
  );
}

function FeedRow({
  repo,
  row,
  point,
  day,
  selected,
  arrived,
}: {
  repo: string;
  row: Row;
  point: string;
  day: string;
  selected: boolean;
  arrived: boolean;
}) {
  const dev = useDevMode();
  const newest = row.operations[0] as OpRecord;
  const oldest = row.operations.at(-1) as OpRecord;
  const what =
    row.class === "event"
      ? {
          text: dev ? operationLabel(newest) : capitalize(operationSentence(newest)),
          title: newest.description,
        }
      : runWhat(row);
  const user = userOf(newest);
  return (
    <tr
      className={
        [selected && "selected", arrived && "changed"].filter(Boolean).join(" ") || undefined
      }
      aria-current={selected ? "true" : undefined}
    >
      <td className="when-cell">
        {dev ? (
          row.operations.length > 1 ? (
            <>
              <When ms={oldest.endedAt} day={day} /> to <When ms={newest.endedAt} day={day} />
            </>
          ) : (
            <When ms={newest.endedAt} day={day} />
          )
        ) : (
          <>
            <Time at={newest.endedAt} />
            {row.operations.length > 1 && (
              <div
                className="secondary run-span"
                title={`${fullTime(at(oldest.endedAt)).text} to ${fullTime(at(newest.endedAt)).text}`}
              >
                over {span(newest.endedAt - oldest.endedAt)}
              </div>
            )}
          </>
        )}
      </td>
      <td>
        <Link
          to={activityHref({ repo, op: newest.id, at: point })}
          replace
          title={what.title || undefined}
        >
          {what.text}
        </Link>
        {row.class === "event" && newest.description && (
          <div className="secondary one-line">{newest.description.split("\n", 1)[0]}</div>
        )}
      </td>
      <td>
        {newest.agent ? (
          <Link to={fleetHref({ repo, agent: newest.agent, at: point })}>
            {dev ? newest.agent : <Identity identity={newest.agent} size="sm" />}
          </Link>
        ) : dev || !user ? (
          user || "-"
        ) : (
          <Identity identity={user} size="sm" />
        )}
      </td>
      <td>
        {newest.workspaceId ? (
          <Link
            to={fleetHref({ repo, workspace: newest.workspaceId, at: point })}
            title={dev ? newest.workspaceId : undefined}
          >
            <span className="mono">{shortId(newest.workspaceId)}</span>
          </Link>
        ) : (
          <span className={newest.jjWorkspace && !dev ? "label" : "mono"}>
            {newest.jjWorkspace || "-"}
          </span>
        )}
      </td>
      <td className="number">{count(row.operations.length)}</td>
    </tr>
  );
}

/**
 * Rows that arrived while live, by key: rows newer than the newest one shown
 * before. A new load of the window (the first, a reload after a reset or a
 * GC) is not news, nor are older pages, nor the bottom run, whose key moves
 * as the cap drops its oldest operations.
 */
class Arrivals {
  private loads = -1;
  private known = new Set<string>();
  private top: OpRecord | undefined;
  private readonly arrived = new Set<string>();

  see(rows: Row[], loads: number) {
    const newest = rows[0]?.operations[0];
    if (loads !== this.loads) {
      this.loads = loads;
      this.known = new Set(rows.map((row) => row.key));
      this.top = newest;
      return;
    }
    for (const row of rows) {
      if (this.known.has(row.key)) continue;
      this.known.add(row.key);
      const first = row.operations[0];
      if (first && this.top && newerFirst(first, this.top) < 0) this.arrived.add(row.key);
    }
    if (newest && (!this.top || newerFirst(newest, this.top) < 0)) this.top = newest;
  }

  has(key: string): boolean {
    return this.arrived.has(key);
  }
}

/** Where the sticky header ends, or 0 when the header scrolls away with the page. */
export function headerBottom(): number {
  const header = document.querySelector("header");
  if (!header || getComputedStyle(header).position !== "sticky") return 0;
  return header.getBoundingClientRect().bottom;
}

/** Reading: the first row is out of view (under the sticky header), or focus is in the table. */
function useReading(box: RefObject<HTMLDivElement | null>, rows: number): boolean {
  const [scrolled, setScrolled] = useState(false);
  const [focused, setFocused] = useState(false);
  useEffect(() => {
    const element = box.current;
    const measure = () => {
      const box = element?.querySelector("tbody tr")?.getBoundingClientRect();
      // A row with no height has not been laid out: not out of view.
      setScrolled(box ? box.height > 0 && box.bottom <= headerBottom() : false);
    };
    const onFocusIn = () => {
      setFocused(true);
    };
    const onFocusOut = (event: FocusEvent) => {
      if (!(event.relatedTarget instanceof Node && element?.contains(event.relatedTarget))) {
        setFocused(false);
      }
    };
    measure();
    window.addEventListener("scroll", measure, { passive: true });
    element?.addEventListener("focusin", onFocusIn);
    element?.addEventListener("focusout", onFocusOut);
    return () => {
      window.removeEventListener("scroll", measure);
      element?.removeEventListener("focusin", onFocusIn);
      element?.removeEventListener("focusout", onFocusOut);
    };
  }, [box, rows]);
  return scrolled || focused;
}

export function Activity({
  connection,
  live,
  onSessionInvalid,
  window: win,
  params,
}: {
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
  window: OperationWindow;
  params: ActivityParams;
}) {
  const snap = useSyncExternalStore(win.subscribe, win.getSnapshot);
  const dev = useDevMode();
  const state = useSyncExternalStore(
    (listener) => live.subscribeState(listener),
    () => live.state,
  );
  const [cache] = useState(() => new OperationCache());
  const [limit, setLimit] = useState(ROWS_SHOWN);
  const box = useRef<HTMLDivElement>(null);
  const repo = win.repositoryId ?? params.repo;

  useEffect(() => {
    if (params.at) win.locatePoint(params.at);
  }, [win, params.at, snap.repository, snap.version]);
  const point = params.at ? win.point(params.at) : undefined;

  const records = snap.records;
  const shown = useMemo((): OpRecord[] | null => {
    if (!records) return null;
    if (!point) return records.filter((r) => !snap.held.has(r.id));
    if (point.state === "inside") return win.ancestors(point.id);
    if (point.state === "detached") return point.records;
    return null;
  }, [records, snap.held, point, win]);

  // Rows that arrive while live take the one change transition, once.
  const [arrivals] = useState(() => new Arrivals());
  const rows = useMemo(() => {
    const out = shown ? coalesce(shown) : [];
    if (shown && !point) arrivals.see(out, snap.loads);
    return out;
  }, [shown, point, snap.loads, arrivals]);

  const reading = useReading(box, rows.length);
  useEffect(() => {
    win.setHolding(!params.at && reading);
  }, [win, params.at, reading]);
  useEffect(
    () => () => {
      win.setHolding(false);
    },
    [win],
  );
  // A note lasts until the next selection or scroll.
  useEffect(() => {
    win.clearNote();
  }, [win, params.op]);
  useEffect(() => {
    if (!snap.note) return;
    const clear = () => {
      win.clearNote();
    };
    window.addEventListener("scroll", clear, { once: true, passive: true });
    return () => {
      window.removeEventListener("scroll", clear);
    };
  }, [win, snap.note]);

  const selectedRow = params.op
    ? rows.find((row) =>
        row.operations.some((o) => o.id === params.op || o.id.startsWith(params.op)),
      )
    : undefined;
  const visible = rows.slice(0, limit);
  // "Show" applies the held rows, then goes to the top once they are drawn.
  const [toTop, setToTop] = useState(0);
  useLayoutEffect(() => {
    if (toTop > 0) box.current?.scrollIntoView({ block: "start" });
  }, [toTop]);

  if (snap.fatal) return <ErrorView error={snap.fatal} />;

  const root = snap.repository?.root;
  const name = root ? root.replace(/\/+$/, "").split("/").pop() || root : shortId(params.repo);
  const summary = shown ? summarize(shown) : undefined;
  const newest = shown?.[0];
  const oldest = shown?.at(-1);
  const day = newest ? dateTime(at(newest.endedAt)).text.slice(0, 10) : "";
  const detached = point?.state === "detached";
  const reachedRoot = snap.reachedRoot && !detached;
  const limitCount = (shown?.length ?? 0) + (reachedRoot ? 1 : 0);
  const command = newest
    ? `reef op log --repo ${repo} --at-op ${commandId(newest.id)} --limit ${String(limitCount)}`
    : "";
  const pointOperation =
    point?.state === "inside" || point?.state === "detached" ? point.operation : undefined;

  return (
    <div className="feed">
      <h1 className="visually-hidden">
        <span title={root}>{name}</span> · Activity
      </h1>
      {params.at && (
        <AtBanner
          repo={repo}
          operation={pointOperation}
          at={params.at}
          live={activityHref({ repo: params.repo, op: params.op })}
        />
      )}
      {!params.at && state !== "live" && newest && (
        <p className="notice">
          Not live: the last operation arrived{" "}
          {dev ? `at ${clock(at(newest.endedAt)).text}` : <Time at={newest.endedAt} />}.
        </p>
      )}
      {snap.note && (
        <p className="secondary note">
          {snap.note.kind === "gc"
            ? "Garbage collection rewrote the operation log; the feed was reloaded."
            : `The operation stream started over; the feed was reloaded at ${clock(at(snap.note.at)).text}.`}
        </p>
      )}
      {snap.error && <ErrorView error={snap.error} />}
      {point?.state === "error" && <ErrorView error={point.error} />}
      <div className="smartlog-body">
        <div>
          {shown === null && point?.state !== "error" && (
            <p className="secondary">Loading the operation log.</p>
          )}
          {shown?.length === 0 && <p>No operations yet besides the root.</p>}
          {shown && summary && shown.length > 0 && (
            <>
              <p className="secondary summary">
                {[
                  `${plural(summary.total, "operation", "operations")} from ${dateTime(at(summary.oldest ?? 0)).text} to ${clock(at(summary.newest ?? 0)).text} in ${plural(rows.length, "row", "rows")}`,
                  ...summary.kinds.map(([word, n]) => `${count(n)} ${word}`),
                  plural(summary.agents, "agent", "agents"),
                  plural(summary.workspaces, "workspace", "workspaces"),
                ].join(" · ")}
              </p>
              {dev && (
                <p className="command">
                  <code>{command}</code> <Copy text={command} />
                </p>
              )}
              {snap.held.size > 0 && !params.at && (
                <div className="held">
                  <button
                    type="button"
                    className="primary"
                    onClick={() => {
                      win.showHeld();
                      setToTop(toTop + 1);
                    }}
                  >
                    <span aria-hidden="true">↑ </span>
                    {plural(snap.held.size, "new operation", "new operations")} · Show
                  </button>
                </div>
              )}
              <div className="table-box" ref={box}>
                <table>
                  <caption className="visually-hidden">Activity</caption>
                  <thead>
                    <tr>
                      <th scope="col">When</th>
                      <th scope="col">What</th>
                      <th scope="col">Who</th>
                      <th scope="col">Where</th>
                      <th scope="col" className="number">
                        Ops
                      </th>
                    </tr>
                  </thead>
                  <tbody>
                    {visible.map((row) => (
                      <FeedRow
                        key={row.key}
                        repo={repo}
                        row={row}
                        point={params.at}
                        day={day}
                        selected={row === selectedRow}
                        arrived={arrivals.has(row.key)}
                      />
                    ))}
                  </tbody>
                </table>
              </div>
              {rows.length > limit && (
                <button
                  type="button"
                  onClick={() => {
                    setLimit(limit + ROWS_SHOWN);
                  }}
                >
                  Show {count(Math.min(ROWS_SHOWN, rows.length - limit))} more rows
                </button>
              )}
              {detached ? (
                <p className="secondary">
                  {plural(shown.length, "operation", "operations")} ending at this point, not joined
                  to the loaded period.
                </p>
              ) : reachedRoot ? (
                <p className="secondary">
                  Start of the retained log: garbage collection keeps the undo window, 7 days by
                  default.
                </p>
              ) : snap.capped ? (
                <p className="secondary">
                  Loaded back to {dateTime(at(oldest?.endedAt ?? 0)).text} · {count(WINDOW_CAP)}{" "}
                  operations, the most this page holds.
                  {dev && (
                    <>
                      {" "}
                      For everything older:{" "}
                      <code>
                        reef op log --repo {repo} --at-op {commandId(oldest?.id ?? "")} --limit 0
                      </code>
                    </>
                  )}
                </p>
              ) : (
                <p className="secondary">
                  Loaded back to {dateTime(at(oldest?.endedAt ?? 0)).text}.{" "}
                  {snap.more && !params.at && (
                    <button
                      type="button"
                      disabled={snap.loadingOlder}
                      onClick={() => {
                        void win.loadOlder();
                      }}
                    >
                      Load {count(PAGE_SIZE)} older
                    </button>
                  )}
                </p>
              )}
            </>
          )}
        </div>
        <aside className="detail-pane" aria-label="Selection" aria-live="polite">
          {params.op && (
            <OperationDetail
              key={params.op}
              repo={repo}
              op={params.op}
              at={params.at}
              row={selectedRow}
              inWindow={(records ?? []).some(
                (r) => r.id === params.op || r.id.startsWith(params.op),
              )}
              connection={connection}
              cache={cache}
              onSessionInvalid={onSessionInvalid}
            />
          )}
        </aside>
      </div>
    </div>
  );
}
