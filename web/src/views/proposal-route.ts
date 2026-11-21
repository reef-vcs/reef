// `/repos/<repo>/proposals/<change>` (R3.31): its link. Kept apart from the
// page so the smartlog and the inbox link here without importing it.
// `/repos/<repo>/proposals`, R3.31's interim list, now opens the inbox
// (R3.33).
import { href } from "../ui/router";

/** One proposal, by its change id (hex or jj's reverse hex). */
export function proposalHref(params: { repo: string; change: string }): string {
  return href(
    `/repos/${encodeURIComponent(params.repo)}/proposals/${encodeURIComponent(params.change)}`,
  );
}
