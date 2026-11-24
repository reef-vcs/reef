import { useSyncExternalStore } from "react";

const QUERY = "(max-width: 599.98px)";

function list(): MediaQueryList | null {
  return typeof matchMedia === "function" ? matchMedia(QUERY) : null;
}

function subscribe(listener: () => void): () => void {
  const query = list();
  query?.addEventListener("change", listener);
  return () => {
    query?.removeEventListener("change", listener);
  };
}

/** Whether the page is phone width: under 600 px, as base.css's narrowest layout. */
export function useNarrow(): boolean {
  return useSyncExternalStore(subscribe, () => list()?.matches ?? false);
}
