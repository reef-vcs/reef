// The one-minute clock behind relative times (the design pass). While a
// component reads it, it ticks at each minute boundary through `after`, so
// every "4 min ago" on the page changes together and nothing else moves. It
// reads nothing over the network and schedules nothing when unread.
import { useSyncExternalStore } from "react";
import { after, type Cancel } from "../live/backoff";

const MINUTE = 60_000;

const listeners = new Set<() => void>();
let now = Date.now();
let cancel: Cancel | null = null;

function tick() {
  now = Date.now();
  for (const listener of listeners) listener();
  arm();
}

function arm() {
  cancel?.();
  cancel = listeners.size > 0 ? after(MINUTE - (Date.now() % MINUTE) + 50, tick) : null;
}

function onVisibility() {
  if (document.visibilityState === "visible") tick();
}

function subscribe(listener: () => void): () => void {
  if (listeners.size === 0) {
    now = Date.now();
    document.addEventListener("visibilitychange", onVisibility);
  }
  listeners.add(listener);
  if (listeners.size === 1) arm();
  return () => {
    listeners.delete(listener);
    if (listeners.size > 0) return;
    document.removeEventListener("visibilitychange", onVisibility);
    cancel?.();
    cancel = null;
  };
}

function snapshot(): number {
  return now;
}

function idle(): () => void {
  return () => undefined;
}

/**
 * The time, updated once a minute while read. Inactive (developer mode, which
 * shows exact times), it schedules nothing and returns the last tick.
 */
export function useNow(active = true): number {
  return useSyncExternalStore(active ? subscribe : idle, snapshot, snapshot);
}
