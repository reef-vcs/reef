// Presence kinds the daemon reports about state, not about an agent's
// doing: executor jobs (R3.05), proposals' automatic statuses and workspaces'
// attention (R3.33). The fleet panel, the smartlog and the overview's agent
// counts leave them out through `isAgentEntry`; a test fails until every
// kind is in one list or the other.
import { PresenceKind, type Presence } from "../gen/reef/api/v1/presence_pb";

export const STATE_KINDS: ReadonlySet<PresenceKind> = new Set([
  PresenceKind.JOB,
  PresenceKind.AUTOMATIC,
  PresenceKind.ATTENTION,
]);

export const AGENT_KINDS: ReadonlySet<PresenceKind> = new Set([
  PresenceKind.ACTIVITY,
  PresenceKind.EDITING,
  PresenceKind.LEASE,
  PresenceKind.PROGRESS,
]);

/** Whether an entry is an agent's doing (or the daemon's report of one). */
export function isAgentEntry(p: Presence): boolean {
  return !STATE_KINDS.has(p.kind);
}

/**
 * The live entries of `kind` in repository `repoId` (every one when empty),
 * as one string that changes whenever one is added, replaced or ended: a
 * page reads again when it does.
 */
export function stateSignature(
  entries: Iterable<Presence>,
  kind: PresenceKind,
  repoId: string,
): string {
  const parts: string[] = [];
  for (const p of entries) {
    if (p.kind !== kind || (repoId !== "" && p.repoId !== repoId)) continue;
    const at = p.updatedAt;
    parts.push(`${p.id}@${at ? `${String(at.seconds)}.${String(at.nanos)}` : ""}`);
  }
  return parts.sort().join(",");
}
