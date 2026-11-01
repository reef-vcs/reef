// `/repos/<repo>/activity` (R2.21): its parameters and links, apart from
// Activity.tsx so other pages link here without loading it.
import { href } from "../ui/router";

export interface ActivityParams {
  repo: string;
  /** A selected operation's id or prefix, or empty. */
  op: string;
  /** An operation: the feed at that point, or empty for live. */
  at: string;
}

export function activityParams(repo: string, search: string): ActivityParams {
  const query = new URLSearchParams(search);
  return { repo, op: query.get("op") ?? "", at: query.get("at") ?? "" };
}

export function activityHref(params: Partial<ActivityParams> & { repo: string }): string {
  return href(`/repos/${encodeURIComponent(params.repo)}/activity`, {
    op: params.op,
    at: params.at,
  });
}
