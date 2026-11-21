// The inbox's links (R3.33): `/` for every repository, and
// `/repos/<repo>/inbox` for one. Kept apart from the page so the tabs, the
// proposal page and the overview link here without importing it.
import { href } from "../ui/router";

/** The inbox: every repository's, or one's. */
export function inboxHref(params: { repo?: string } = {}): string {
  return params.repo ? href(`/repos/${encodeURIComponent(params.repo)}/inbox`) : "/";
}
