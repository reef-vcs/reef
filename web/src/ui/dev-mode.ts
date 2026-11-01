// Developer mode (the design pass): one per-origin setting, off by default.
// On, every element renders as R2.18 to R2.21 rendered it: ids, CLI
// commands, revsets, exact times and the API's words. Off, those leave the
// DOM and human forms take their place. Stored like the session token, so
// all tabs of a daemon share it, and a `storage` event updates other tabs.
import { type ReactNode, createElement, Fragment, useSyncExternalStore } from "react";
import { localStorageOrNull } from "../session";

export const DEV_MODE_KEY = "reef.devMode";

const listeners = new Set<() => void>();
let on = read();

function read(): boolean {
  try {
    return localStorageOrNull()?.getItem(DEV_MODE_KEY) === "1";
  } catch {
    return false;
  }
}

function notify() {
  for (const listener of listeners) listener();
}

function onStorage(event: StorageEvent) {
  if (event.key !== null && event.key !== DEV_MODE_KEY) return;
  const next = read();
  if (next === on) return;
  on = next;
  notify();
}

function subscribe(listener: () => void): () => void {
  if (listeners.size === 0 && typeof window !== "undefined") {
    window.addEventListener("storage", onStorage);
  }
  listeners.add(listener);
  return () => {
    listeners.delete(listener);
    if (listeners.size === 0 && typeof window !== "undefined") {
      window.removeEventListener("storage", onStorage);
    }
  };
}

export function devMode(): boolean {
  return on;
}

export function setDevMode(next: boolean) {
  try {
    const storage = localStorageOrNull();
    if (next) storage?.setItem(DEV_MODE_KEY, "1");
    else storage?.removeItem(DEV_MODE_KEY);
  } catch {
    // Storage refused: the setting lasts as long as this tab.
  }
  if (next === on) return;
  on = next;
  notify();
}

/** Re-reads storage; for tests that seed it before rendering. */
export function reloadDevMode() {
  on = read();
  notify();
}

export function useDevMode(): boolean {
  return useSyncExternalStore(subscribe, devMode, devMode);
}

/** Its children only in developer mode. */
export function Dev({ children }: { children: ReactNode }) {
  return useDevMode() ? createElement(Fragment, null, children) : null;
}

/** Its children only outside developer mode. */
export function Human({ children }: { children: ReactNode }) {
  return useDevMode() ? null : createElement(Fragment, null, children);
}
