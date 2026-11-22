/** The page's title: "(3) Inbox · reef — Reef", or without a count at zero. */
export function inboxTitle(count: number, repoName?: string): string {
  const page = repoName ? `Inbox · ${repoName}` : "Inbox";
  return count > 0 ? `(${String(count)}) ${page} — Reef` : `${page} — Reef`;
}
