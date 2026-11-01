// Relative times (the design pass), pure: the clock is the caller's. English
// and locale independent. Exact times stay in `title` and developer mode.
const MINUTE = 60_000;
const HOUR = 60 * MINUTE;
const SKEW = 2 * MINUTE;
const DAYS = ["Sun", "Mon", "Tue", "Wed", "Thu", "Fri", "Sat"];
const MONTHS = ["Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec"];

const pad = (n: number) => String(n).padStart(2, "0");

function hhmm(date: Date): string {
  return `${pad(date.getHours())}:${pad(date.getMinutes())}`;
}

function startOfDay(ms: number): number {
  const date = new Date(ms);
  date.setHours(0, 0, 0, 0);
  return date.getTime();
}

function calendar(then: Date, now: Date): string {
  if (then.getFullYear() === now.getFullYear()) {
    return `${MONTHS[then.getMonth()] ?? ""} ${String(then.getDate())}`;
  }
  return `${String(then.getFullYear())}-${pad(then.getMonth() + 1)}-${pad(then.getDate())}`;
}

/** "just now", "4 min ago", "3 h ago", "yesterday 14:03", "Mon 14:03", "Oct 24". */
export function ago(thenMs: number, nowMs: number): string {
  const distance = nowMs - thenMs;
  const then = new Date(thenMs);
  if (distance < -SKEW) return hhmm(then);
  if (distance < MINUTE) return "just now";
  if (distance < HOUR) return `${String(Math.floor(distance / MINUTE))} min ago`;
  const today = startOfDay(nowMs);
  if (thenMs >= today) return `${String(Math.floor(distance / HOUR))} h ago`;
  if (thenMs >= startOfDay(today - 1)) return `yesterday ${hhmm(then)}`;
  // The weekday only within the last six calendar days, so it never names today's.
  const sixDaysBack = new Date(today);
  sixDaysBack.setDate(sixDaysBack.getDate() - 6);
  if (thenMs >= sixDaysBack.getTime()) return `${DAYS[then.getDay()] ?? ""} ${hhmm(then)}`;
  return calendar(then, new Date(nowMs));
}

/** "in under a minute", "in 4 min", "in 3 h", "Oct 24". */
export function until(thenMs: number, nowMs: number): string {
  const distance = thenMs - nowMs;
  if (distance < MINUTE) return "in under a minute";
  if (distance < HOUR) return `in ${String(Math.ceil(distance / MINUTE))} min`;
  if (distance < 24 * HOUR) return `in ${String(Math.ceil(distance / HOUR))} h`;
  return calendar(new Date(thenMs), new Date(nowMs));
}

/** "47 s", "3 min", "2 h": how long a run of operations took. */
export function span(ms: number): string {
  if (ms < MINUTE) return `${String(Math.max(1, Math.floor(ms / 1000)))} s`;
  if (ms < HOUR) return `${String(Math.floor(ms / MINUTE))} min`;
  return `${String(Math.floor(ms / HOUR))} h`;
}
