import { useCallback, useEffect, useSyncExternalStore } from "react";
import type { Live } from "../live/share";
import { Link, navigate } from "../ui/router";
import { inboxHref, openedAt } from "./inbox-route";

export function ItemLink({ live, repo, itemKey }: { live: Live; repo: string; itemKey: string }) {
  const store = live.inbox;
  const subscribe = useCallback((listener: () => void) => store.subscribe(listener), [store]);
  useSyncExternalStore(subscribe, () => store.version);
  const state = useSyncExternalStore(
    (listener) => live.subscribeState(listener),
    () => live.state,
  );
  const item = store.sections.get(repo)?.items.get(itemKey);
  // What a lost stream's inbox does not hold may have arrived since.
  const gone = !item && state === "live" && store.synced && !store.resyncing;
  const to = item ? openedAt(item) : null;
  useEffect(() => {
    if (to) navigate(to, { replace: true });
  }, [to]);
  useEffect(() => {
    document.title = "Inbox item — Reef";
  }, []);
  if (gone) {
    return (
      <section className="card" aria-label="Inbox item">
        <p>That no longer needs you.</p>
        <p>
          <Link to={inboxHref({ repo })}>Open the inbox</Link>
        </p>
      </section>
    );
  }
  return <p className="secondary">Finding it in the inbox.</p>;
}
