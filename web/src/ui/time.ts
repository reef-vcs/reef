// Times are absolute local times, never "12 s ago": a relative time needs a
// ticking clock, which is motion without a state change.
import { timestampDate, type Timestamp } from "@bufbuild/protobuf/wkt";

const pad = (n: number) => String(n).padStart(2, "0");

export function clock(timestamp: Timestamp | undefined): { text: string; title: string } {
  if (!timestamp) return { text: "-", title: "" };
  const date = timestampDate(timestamp);
  return {
    text: `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`,
    title: date.toISOString(),
  };
}

/** `2026-10-31 14:03`, local, with RFC 3339 for a `title`. */
export function dateTime(timestamp: Timestamp | undefined): { text: string; title: string } {
  if (!timestamp) return { text: "-", title: "" };
  const date = timestampDate(timestamp);
  return {
    text: `${String(date.getFullYear())}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ${pad(date.getHours())}:${pad(date.getMinutes())}`,
    title: date.toISOString(),
  };
}
