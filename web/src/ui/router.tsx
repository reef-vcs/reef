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

function search(): string {
  return window.location.search;
}

export function usePath(): string {
  return useSyncExternalStore(subscribe, pathname);
}

/** The query string; re-renders when it changes. Parse it with `URLSearchParams`. */
export function useSearch(): string {
  return useSyncExternalStore(subscribe, search);
}

/**
 * The parameters of `pattern` (segments like `:repo`) in `path`, decoded, or
 * null when it does not match. Segment counts must agree, so a trailing slash
 * or an extra segment does not match.
 */
export function match(pattern: string, path: string): Record<string, string> | null {
  const want = pattern.split("/");
  const got = path.split("/");
  if (want.length !== got.length) return null;
  const params: Record<string, string> = {};
  for (const [i, segment] of want.entries()) {
    const value = got[i] ?? "";
    if (segment.startsWith(":")) {
      if (!value) return null;
      try {
        params[segment.slice(1)] = decodeURIComponent(value);
      } catch {
        return null;
      }
    } else if (segment !== value) {
      return null;
    }
  }
  return params;
}

/** A path with a query; empty values are left out. */
export function href(path: string, query: Record<string, string | undefined> = {}): string {
  const params = new URLSearchParams();
  for (const [key, value] of Object.entries(query)) if (value) params.set(key, value);
  const text = params.toString();
  return text ? `${path}?${text}` : path;
}

/** Goes to `to`, a path and an optional query: a new history entry, or with `replace`, none. */
export function navigate(to: string, { replace = false }: { replace?: boolean } = {}) {
  if (to === window.location.pathname + window.location.search) return;
  if (replace) window.history.replaceState(null, "", to);
  else window.history.pushState(null, "", to);
  for (const listener of [...listeners]) listener();
}

/**
 * A link to a route of the app; other links are plain `<a>`. With `replace`,
 * following it adds no history entry (a selection); `current` marks the
 * page it names (`aria-current`).
 */
export function Link({
  to,
  title,
  replace = false,
  current,
  children,
}: {
  to: string;
  title?: string | undefined;
  replace?: boolean;
  current?: "page" | "true" | undefined;
  children: ReactNode;
}) {
  function onClick(event: MouseEvent<HTMLAnchorElement>) {
    if (event.button !== 0 || event.metaKey || event.ctrlKey || event.shiftKey || event.altKey) {
      return;
    }
    event.preventDefault();
    navigate(to, { replace });
  }
  return (
    <a href={to} title={title} aria-current={current} onClick={onClick}>
      {children}
    </a>
  );
}
