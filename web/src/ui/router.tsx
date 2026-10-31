// Client-side routes over the History API. The daemon serves index.html for
// any path without a `.` in its last segment, so a reload lands here too.
import { useSyncExternalStore, type MouseEvent, type ReactNode } from "react";

const listeners = new Set<() => void>();

function subscribe(listener: () => void): () => void {
  listeners.add(listener);
  window.addEventListener("popstate", listener);
  return () => {
    listeners.delete(listener);
    window.removeEventListener("popstate", listener);
  };
}

function pathname(): string {
  return window.location.pathname;
}

export function usePath(): string {
  return useSyncExternalStore(subscribe, pathname);
}

export function navigate(to: string) {
  if (to === window.location.pathname) return;
  window.history.pushState(null, "", to);
  for (const listener of [...listeners]) listener();
}

/** A link to a route of the app; other links are plain `<a>`. */
export function Link({ to, children }: { to: string; children: ReactNode }) {
  function onClick(event: MouseEvent<HTMLAnchorElement>) {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
      return;
    }
    event.preventDefault();
    navigate(to);
  }
  return (
    <a href={to} onClick={onClick}>
      {children}
    </a>
  );
}
