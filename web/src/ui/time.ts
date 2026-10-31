// Times are absolute local times, never "12 s ago": a relative time needs a
// ticking clock, which is motion without a state change.
import { timestampDate, type Timestamp } from "@bufbuild/protobuf/wkt";

export function clock(timestamp: Timestamp | undefined): { text: string; title: string } {
  if (!timestamp) return { text: "-", title: "" };
  const date = timestampDate(timestamp);
  const pad = (n: number) => String(n).padStart(2, "0");
  return {
    text: `${pad(date.getHours())}:${pad(date.getMinutes())}:${pad(date.getSeconds())}`,
    title: date.toISOString(),
  };
}
