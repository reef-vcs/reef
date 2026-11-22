import type { InboxItem } from "../gen/reef/api/v1/inbox_pb";
import { href } from "../ui/router";
import { fleetHref } from "./fleet-route";
import { proposalHref } from "./proposal-route";
import { smartlogHref } from "./smartlog-route";

/** The inbox: every repository's, or one's. */
export function inboxHref(params: { repo?: string } = {}): string {
  return params.repo ? href(`/repos/${encodeURIComponent(params.repo)}/inbox`) : "/";
}

export function targetOf(item: InboxItem): string {
  const repo = item.repoId;
  const record = item.record;
  if (record.case === "proposal") return proposalHref({ repo, change: item.changeId });
  if (record.case === "workspace") return fleetHref({ repo, workspace: record.value.id });
  if (item.changeId) return smartlogHref({ repo, change: item.changeId });
  return smartlogHref({ repo });
}

export function openedAt(item: InboxItem): string {
  return item.record.case === "comment" ? inboxHref({ repo: item.repoId }) : targetOf(item);
}

export function itemHref(params: { repo: string; key: string }): string {
  return href(`/repos/${encodeURIComponent(params.repo)}/items/${encodeURIComponent(params.key)}`);
}
