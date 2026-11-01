// A time as people read it (the design pass): "4 min ago", with the exact
// local time in `title`. Developer mode shows the exact time instead, in the
// format the call site used before. Only the human form reads the clock.
import { timestampDate, timestampFromMs, type Timestamp } from "@bufbuild/protobuf/wkt";
import { useNow } from "./clock";
import { useDevMode } from "./dev-mode";
import { ago } from "./relative";
import { clock, dateTime, fullTime } from "./time";

export type DevFormat = "clock" | "date" | "full";

const FORMATS = { clock, date: dateTime, full: fullTime };

function stamp(at: Timestamp | number): Timestamp {
  return typeof at === "number" ? timestampFromMs(at) : at;
}

function Relative({ at }: { at: Timestamp }) {
  const now = useNow();
  const date = timestampDate(at);
  const text = ago(date.getTime(), now);
  return (
    <time dateTime={date.toISOString()} title={fullTime(at).text}>
      {text}
    </time>
  );
}

function Exact({ at, dev }: { at: Timestamp; dev: DevFormat }) {
  const { text, title } = FORMATS[dev](at);
  return (
    <time dateTime={title} title={title}>
      {text}
    </time>
  );
}

/** A past time: relative, or exact in developer mode. `-` when missing. */
export function Time({
  at,
  dev = "clock",
}: {
  at: Timestamp | number | undefined;
  dev?: DevFormat;
}) {
  const isDev = useDevMode();
  if (at === undefined) return <>-</>;
  return isDev ? <Exact at={stamp(at)} dev={dev} /> : <Relative at={stamp(at)} />;
}

/**
 * A time as text for a `title` or a sentence: relative against `now`, or
 * `clock` text in developer mode. The caller reads `useNow()` and
 * `useDevMode()` once.
 */
export function timeText(at: Timestamp | number | undefined, now: number, dev: boolean): string {
  if (at === undefined) return "-";
  const ts = stamp(at);
  return dev ? clock(ts).text : ago(timestampDate(ts).getTime(), now);
}
