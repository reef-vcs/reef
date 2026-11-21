// What needs the person now, for the page's title and for the desktop
// shell's notifications (R3.39's seam): the needs-you-now keys at each read,
// and the items whose keys are new since the one before. Opening the page
// adds nothing, and a key reported once is not reported again in the same
// page (a daemon restart re-judges every status, and an item may leave and
// come back).
import type { InboxItem } from "../gen/reef/api/v1/inbox_pb";

export class NeedsWatcher {
  private readonly reported = new Set<string>();
  private first = true;

  /** The items of `items` not seen before; none on the first call. */
  next(items: readonly InboxItem[]): InboxItem[] {
    const added = this.first ? [] : items.filter((item) => !this.reported.has(item.key));
    this.first = false;
    for (const item of items) this.reported.add(item.key);
    return added;
  }
}

/** The page's title: "(3) Inbox · reef — Reef", or without a count at zero. */
export function inboxTitle(count: number, repoName?: string): string {
  const page = repoName ? `Inbox · ${repoName}` : "Inbox";
  return count > 0 ? `(${String(count)}) ${page} — Reef` : `${page} — Reef`;
}
