// What the smartlog reads (R2.19): Reef's default log set with each stack's
// parent added, the shape of jj's own default log, so stacks hang from their
// fork points on a trunk spine. The daemon's `reads::smartlog_revset` test
// reads this constant from this file: keep it on one line, in double quotes.

export const SMARTLOG_REVSET = "trunk() | working_copies() | ancestors(mutable(), 2)";

/** Entries loaded at first, and added by each "Show more". */
export const DEFAULT_LIMIT = 200;
/** `ListChanges`' page maximum: the view reads its whole limit in one page. */
export const MAX_LIMIT = 1000;

/** One argument for a POSIX shell: single-quoted, with `'` written as `'\''`. */
export function shellQuote(argument: string): string {
  return /^[A-Za-z0-9_@%+=:,./-]+$/.test(argument)
    ? argument
    : `'${argument.replaceAll("'", `'\\''`)}'`;
}

/** The `reef log` command that prints exactly the view's entries. */
export function logCommand({
  repo,
  revset,
  limit,
  at,
}: {
  repo: string;
  revset: string;
  limit: number;
  at?: string | undefined;
}): string {
  const args = ["reef", "log", "--repo", repo, "-r", revset, "--limit", String(limit)];
  if (at) args.push("--at-op", at);
  return args.map(shellQuote).join(" ");
}
