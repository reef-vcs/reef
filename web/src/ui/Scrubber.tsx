// The scrubber (R2.21): one slider over the window's operations, in the
// header of the three repository pages (the app bar's strip since the design
// pass, labelled "History"). A stop is an operation, and "Live"
// is a stop after the newest; committing one moves the open page to
// `?at=<operation>`, which the smartlog and the workspaces page read at,
// remounted, and the feed shows. The track is a time axis from the window's
// oldest operation to its newest, never "now": no clock is read, so an idle
// repository's track does not creep.
//
// A burst of steps is one navigation, `COMMIT_DELAY_MS` after the last, or
// at once on release, End or Enter. The first move away from Live pushes a
// history entry and later moves replace it, so Back returns to live.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import {
  useEffect,
  useMemo,
  useRef,
  useState,
  useSyncExternalStore,
  type SyntheticEvent,
  type KeyboardEvent,
  type PointerEvent,
} from "react";
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { after, type Cancel } from "../live/backoff";
import { userOf } from "../log/coalesce";
import { bucketWidth, buckets } from "../log/counts";
import type { OpRecord } from "../log/record";
import type { OperationWindow } from "../log/window";
import { useNow } from "./clock";
import { count } from "./count";
import { useDevMode } from "./dev-mode";
import { ErrorView } from "./ErrorView";
import { HistoryIcon } from "./icons";
import { shortId } from "./ids";
import { operationLabel } from "./operation";
import { navigate, usePath, useSearch } from "./router";
import { ago } from "./relative";
import { clock } from "./time";
import { operationSentence } from "./words";

/** The pause after the last step before a burst commits. */
export const COMMIT_DELAY_MS = 250;

/** The current page with `at` set, or removed for live; the rest of its query kept. */
export function withAt(at: string): string {
  const params = new URLSearchParams(window.location.search);
  if (at) params.set("at", at);
  else params.delete("at");
  const query = params.toString();
  return window.location.pathname + (query ? `?${query}` : "");
}

function describe(record: OpRecord): string {
  const who = record.agent || userOf(record) || "-";
  return `${shortId(record.id)} · ${operationLabel(record)} · ${who} · ${clock(timestampFromMs(record.endedAt)).text}`;
}

/** "4 min ago · ann saved a snapshot", outside developer mode. */
function sentence(record: OpRecord, now: number): string {
  const who = record.agent || userOf(record);
  const what = operationSentence(record);
  return `${ago(record.endedAt, now)} · ${who ? `${who} ${what}` : what}`;
}

export function Scrubber({
  window: win,
  at,
  connection,
  onSessionInvalid,
}: {
  window: OperationWindow;
  /** The route's point, or empty for live. */
  at: string;
  connection: Connection;
  onSessionInvalid: () => void;
}) {
  const snap = useSyncExternalStore(win.subscribe, win.getSnapshot);
  const dev = useDevMode();
  const now = useNow(!dev);
  const records = useMemo(() => snap.records ?? [], [snap.records]);
  const n = records.length;
  useEffect(() => {
    if (at) win.locatePoint(at);
  }, [win, at, snap.repository, snap.version]);
  const point = at ? win.point(at) : undefined;
  const detached = point?.state === "detached";

  const committed =
    !at || point?.state !== "inside"
      ? n
      : Math.max(0, n - 1 - records.findIndex((r) => r.id === point.id));
  const [preview, setPreview] = useState<number | null>(null);
  const value = preview ?? committed;
  const pending = useRef<Cancel | undefined>(undefined);
  const dragging = useRef(false);
  const track = useRef<HTMLDivElement>(null);
  const [error, setError] = useState<ReefError | null>(null);
  const [time, setTime] = useState("");

  // Navigating anywhere (a link, Back, a commit) drops a burst in progress:
  // its preview now, and its pending commit, which would go to a stale point.
  const location = usePath() + useSearch();
  const [seen, setSeen] = useState(location);
  if (seen !== location) {
    setSeen(location);
    setPreview(null);
  }
  useEffect(
    () => () => {
      pending.current?.();
      pending.current = undefined;
    },
    [location],
  );

  const recordAt = (v: number) => (v >= 0 && v < n ? records[n - 1 - v] : undefined);

  function go(target: string) {
    pending.current?.();
    pending.current = undefined;
    setPreview(null);
    if (target === at) return;
    navigate(withAt(target), { replace: at !== "" });
  }

  function commit(v: number) {
    go(v >= n ? "" : (recordAt(v)?.id ?? ""));
  }

  function step(v: number) {
    const next = Math.max(0, Math.min(n, v));
    setPreview(next);
    pending.current?.();
    pending.current = after(COMMIT_DELAY_MS, () => {
      pending.current = undefined;
      commit(next);
    });
  }

  // Buckets on a time axis from the oldest operation to the newest.
  const oldest = records.at(-1)?.endedAt ?? 0;
  const newest = records[0]?.endedAt ?? 0;
  const width = bucketWidth(oldest, newest);
  const bars = useMemo(() => buckets(records, width), [records, width]);
  const axisStart = bars[0]?.start ?? oldest;
  const axisEnd = (bars.at(-1)?.start ?? newest) + width;
  const tallest = Math.max(1, ...bars.map((b) => b.count));
  const position = (t: number) => ((t - axisStart) / (axisEnd - axisStart)) * 100;

  /** The stop of the newest operation that ended at or before `t`. */
  function stopAtTime(t: number): number {
    const i = records.findIndex((r) => r.endedAt <= t);
    return i < 0 ? 0 : n - 1 - i;
  }

  /** One bucket older (negative) or newer (positive) than the stop. */
  function bucketStep(direction: -1 | 1): number {
    const t = recordAt(value)?.endedAt ?? newest;
    if (direction < 0) return stopAtTime(t - width);
    const i = [...records].reverse().findIndex((r) => r.endedAt >= t + width);
    return i < 0 ? n : i;
  }

  function onKeyDown(event: KeyboardEvent<HTMLDivElement>) {
    let handled = true;
    if (point?.state === "detached") {
      // Older than the window: step to the first parent, or back to live.
      if (event.key === "ArrowLeft") {
        const parent = point.operation.parentIds[0];
        if (parent) go(parent);
      } else if (event.key === "End") go("");
      else handled = event.key === "ArrowRight";
    } else if (event.key === "ArrowLeft") step(value - 1);
    else if (event.key === "ArrowRight") step(value + 1);
    else if (event.key === "PageDown") step(bucketStep(-1));
    else if (event.key === "PageUp") step(bucketStep(1));
    else if (event.key === "Home") step(0);
    else if (event.key === "End") commit(n);
    else if (event.key === "Enter") commit(value);
    else handled = false;
    if (handled) event.preventDefault();
  }

  function stopAtPointer(event: PointerEvent<HTMLDivElement>): number {
    const box = track.current?.getBoundingClientRect();
    if (!box || box.width === 0 || n === 0) return value;
    const fraction = Math.max(0, Math.min(1, (event.clientX - box.left) / box.width));
    return stopAtTime(axisStart + fraction * (axisEnd - axisStart));
  }

  async function goToTime(event: SyntheticEvent) {
    event.preventDefault();
    setError(null);
    const t = new Date(time).getTime();
    if (!Number.isFinite(t)) return;
    if (n > 0 && t >= oldest) {
      go(recordAt(stopAtTime(t))?.id ?? "");
      return;
    }
    try {
      const response = await connection.clients.operations.getOperation({
        repo: win.repositoryId ?? "",
        endedAtOrBefore: timestampFromMs(t),
      });
      if (response.operation) go(response.operation.id);
    } catch (thrown) {
      const failure = reefError(thrown);
      if (isSessionInvalid(failure)) onSessionInvalid();
      else setError(failure);
    }
  }

  const current = recordAt(value);
  const label = dev
    ? point?.state === "detached"
      ? `${shortId(point.operation.id)} · older than the loaded period`
      : point?.state === "loading" && preview === null
        ? `Looking up ${shortId(at)}`
        : value >= n || !current
          ? "Live"
          : `${describe(current)} · ${count(value + 1)} of ${count(n)}`
    : point?.state === "detached"
      ? "Older than the loaded period"
      : point?.state === "loading" && preview === null
        ? "Looking up the point"
        : value >= n || !current
          ? "Now"
          : `${sentence(current, now)} · ${count(value + 1)} of ${count(n)}`;
  // Bars after the point are what the view does not include yet.
  const pointEnd = value >= n || !current ? Infinity : current.endedAt;
  const handleAt = detached ? 0 : current ? position(current.endedAt) : 100;

  return (
    <div className="scrubber">
      <div className="scrubber-row">
        <span className="strip-title">
          <HistoryIcon />
          History
        </span>
        <div
          ref={track}
          className="track"
          role="slider"
          tabIndex={0}
          aria-label="Point in the operation log"
          aria-valuemin={0}
          aria-valuemax={n}
          aria-valuenow={detached ? 0 : value}
          aria-valuetext={label}
          onKeyDown={onKeyDown}
          onPointerDown={(event) => {
            if (n === 0) return;
            dragging.current = true;
            event.currentTarget.setPointerCapture(event.pointerId);
            setPreview(stopAtPointer(event));
          }}
          onPointerMove={(event) => {
            if (dragging.current) setPreview(stopAtPointer(event));
          }}
          onPointerUp={(event) => {
            if (!dragging.current) return;
            dragging.current = false;
            commit(stopAtPointer(event));
          }}
        >
          <div className="bars" aria-hidden="true">
            {bars.map((bar) => (
              <div
                key={bar.start}
                className={bar.start > pointEnd ? "bar future" : "bar"}
                style={{ height: `${String((bar.count / tallest) * 100)}%` }}
                title={`${clock(timestampFromMs(bar.start)).text}: ${count(bar.count)}`}
              />
            ))}
          </div>
          <div
            className={detached ? "handle detached" : "handle"}
            style={{ left: `${String(handleAt)}%` }}
            aria-hidden="true"
          />
        </div>
        <span className="scrubber-label">{label}</span>
        <form
          className="go-to-time"
          onSubmit={(event) => {
            void goToTime(event);
          }}
        >
          <label>
            Go to time{" "}
            <input
              type="datetime-local"
              step={1}
              value={time}
              onChange={(event) => {
                setTime(event.target.value);
              }}
            />
          </label>{" "}
          <button type="submit">Go</button>
        </form>
      </div>
      {dev && (
        <div className="scrubber-row secondary">
          <span>
            {bars.length > 0 && `Tallest bar: ${count(tallest)} · `}
            {n > 0 &&
              `${clock(timestampFromMs(oldest)).text} to ${clock(timestampFromMs(newest)).text}`}
          </span>
        </div>
      )}
      {error && <ErrorView error={error} />}
    </div>
  );
}
