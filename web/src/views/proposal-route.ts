// `/repos/<repo>/proposals` and `/repos/<repo>/proposals/<change>` (R3.31):
// their links. Kept apart from the pages so the smartlog and the tabs link
// here without importing them.
import { href } from "../ui/router";

/** The Proposals tab: the repository's open proposals. */
export function proposalsHref(params: { repo: string; at?: string }): string {
  return href(`/repos/${encodeURIComponent(params.repo)}/proposals`, { at: params.at });
}

/** One proposal, by its change id (hex or jj's reverse hex). */
export function proposalHref(params: { repo: string; change: string }): string {
  return href(
    `/repos/${encodeURIComponent(params.repo)}/proposals/${encodeURIComponent(params.change)}`,
  );
}
