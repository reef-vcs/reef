// What the workspaces page's rows show (R2.20), computed at render from the
// model's messages and the presence store: each workspace's lease and
// health, and each agent's workspaces, activity and health. Pure functions,
// with no clock.
import type { Timestamp } from "@bufbuild/protobuf/wkt";
import { timestampMs } from "@bufbuild/protobuf/wkt";
import { PresenceEndReason, PresenceKind, type Presence } from "../gen/reef/api/v1/presence_pb";
import { Attention, type Lease } from "../gen/reef/api/v1/workspaces_pb";
import type { AgentJoin, FleetAgent } from "../fleet/agents";
import { foldLease, leaseHealth, type Health } from "../fleet/lease";
import type { Ending } from "../live/presence";
import type { WorkspaceRow } from "./fleet-model";

export const ATTENTION_WORDS: Partial<Record<Attention, string>> = {
  [Attention.DIVERGENT]: "divergent",
  [Attention.STALE_RECOVERED]: "stale recovered",
  [Attention.FINAL_SNAPSHOT_FAILED]: "final snapshot failed",
  [Attention.STORE_HASH_MISMATCH]: "store mismatch",
  [Attention.STORE_MISSING]: "store missing",
  [Attention.WATCH_FAILED]: "watch failed",
  [Attention.UPPER_MISSING]: "upper missing",
  [Attention.MOUNT_FAILED]: "mount failed",
};

export const ATTENTION_MEANINGS: Partial<Record<Attention, string>> = {
  [Attention.DIVERGENT]: "its change has more than one visible commit",
  [Attention.STALE_RECOVERED]: "its working copy was stale and was recovered",
  [Attention.FINAL_SNAPSHOT_FAILED]: "the final snapshot before a release failed; it was kept",
  [Attention.STORE_HASH_MISMATCH]: "its lockfile no longer matches a mounted shared store",
  [Attention.STORE_MISSING]: "a shared store it declares had no sealed build",
  [Attention.WATCH_FAILED]: "its file watch could not be attached",
  [Attention.UPPER_MISSING]: "its overlay's upper directory is gone",
  [Attention.MOUNT_FAILED]: "its overlay could not be mounted",
};

export const ENDING_WORDS: Record<PresenceEndReason, string> = {
  [PresenceEndReason.CLEARED]: "finished",
  [PresenceEndReason.EXPIRED]: "went silent",
  [PresenceEndReason.GONE]: "workspace gone",
  [PresenceEndReason.UNSPECIFIED]: "ended",
};

/** Each workspace's lease entries, live and ended, indexed once per render. */
export interface LeaseIndex {
  /** Workspaces with a live `LEASE` entry. */
  live: Set<string>;
  /** The leases each workspace's `LEASE` entries carried, live or ended. */
  carried: Map<string, Lease[]>;
}

export function leaseIndex(entries: Iterable<Presence>, endings: Iterable<Ending>): LeaseIndex {
  const live = new Set<string>();
  const carried = new Map<string, Lease[]>();
  const carry = (p: Presence) => {
    if (p.kind !== PresenceKind.LEASE || !p.workspaceId || !p.lease) return;
    let list = carried.get(p.workspaceId);
    if (!list) carried.set(p.workspaceId, (list = []));
    list.push(p.lease);
  };
  for (const p of entries) {
    if (p.kind === PresenceKind.LEASE && p.workspaceId) live.add(p.workspaceId);
    carry(p);
  }
  for (const ending of endings) carry(ending.presence);
  return { live, carried };
}

export interface LeaseView {
  lease: Lease | undefined;
  health: Health;
}

export function leaseView(
  row: WorkspaceRow,
  index: LeaseIndex,
  synced: boolean,
  restarting: boolean,
): LeaseView {
  const lease = foldLease(row.workspace.lease, index.carried.get(row.id) ?? []);
  return {
    lease,
    health: leaseHealth({ lease, heartbeating: index.live.has(row.id), synced, restarting }),
  };
}

const RANK: Record<Health["word"], number> = {
  "-": -1,
  heartbeating: 0,
  "not heartbeating": 1,
  "waiting for a heartbeat": 2,
  "missed heartbeats": 3,
};

/** The worst of several healths. */
export function worst(healths: Health[]): Health {
  let out: Health = { word: "-", title: "", stuck: false };
  for (const health of healths) if (RANK[health.word] > RANK[out.word]) out = health;
  return out;
}

/** What an entry says it is doing, in words. */
export function doing(p: Presence): string {
  switch (p.kind) {
    case PresenceKind.ACTIVITY:
      return p.activity;
    case PresenceKind.PROGRESS: {
      const progress = p.progress;
      if (!progress) return "working";
      const count = progress.total
        ? `${String(progress.done)}/${String(progress.total)}`
        : String(progress.done);
      return [progress.phase, count, progress.unit].filter(Boolean).join(" ");
    }
    case PresenceKind.EDITING:
      return `editing ${String(p.paths.length)} ${p.paths.length === 1 ? "path" : "paths"}`;
    case PresenceKind.LEASE:
      return "holds a lease";
    default:
      return "present";
  }
}

function ms(timestamp: Timestamp | undefined): number {
  return timestamp ? timestampMs(timestamp) : Number.NEGATIVE_INFINITY;
}

/** The newest entry: what the Doing column shows. A lease entry only when nothing else is. */
export function newest(entries: Presence[]): Presence | undefined {
  let best: Presence | undefined;
  for (const p of entries) {
    if (!best) {
      best = p;
      continue;
    }
    const leaseOnly = (e: Presence) => e.kind === PresenceKind.LEASE;
    if (leaseOnly(best) !== leaseOnly(p)) {
      if (leaseOnly(best)) best = p;
      continue;
    }
    if (ms(p.updatedAt) > ms(best.updatedAt)) best = p;
  }
  return best;
}

export interface AgentRow {
  name: string;
  /** Undefined once all its entries ended. */
  agent: FleetAgent | undefined;
  /** For a gone agent, its last ending. */
  ending: Ending | undefined;
  doing: string;
  /** The `title` for the Doing cell: an editing entry's first paths. */
  doingTitle: string;
  since: Timestamp | undefined;
  health: Health;
}

/** The last ending of an agent with no live entry: its own entries' first, else the daemon's. */
export function lastEnding(name: string, endings: Iterable<Ending>): Ending | undefined {
  let own: Ending | undefined;
  let daemon: Ending | undefined;
  for (const ending of endings) {
    if (ending.presence.agent !== name) continue;
    if (ending.presence.byDaemon) daemon = ending;
    else own = ending;
  }
  return own ?? daemon;
}

export function agentRows(
  order: string[],
  join: AgentJoin,
  endings: Iterable<Ending>,
  healthOf: (workspaceId: string) => Health | undefined,
): AgentRow[] {
  const ended = [...endings];
  return order.map((name) => {
    const agent = join.agents.get(name);
    if (!agent) {
      const ending = lastEnding(name, ended);
      return {
        name,
        agent,
        ending,
        doing: "",
        doingTitle: "",
        since: undefined,
        health: { word: "-", title: "", stuck: false },
      };
    }
    const top = newest(agent.entries);
    let since: Timestamp | undefined;
    for (const p of agent.entries) if (ms(p.updatedAt) > ms(since)) since = p.updatedAt;
    const healths = agent.workspaceIds.flatMap((id) => {
      const health = healthOf(id);
      return health ? [health] : [];
    });
    return {
      name,
      agent,
      ending: undefined,
      doing: top ? doing(top) : "",
      doingTitle: top?.kind === PresenceKind.EDITING ? top.paths.slice(0, 3).join(", ") : "",
      since,
      health: worst(healths),
    };
  });
}
