// Who an agent is (R2.20): one join of presence entries, used by the
// workspaces page, the overview's count and the smartlog's labels.
//
// A client entry belongs to its `agent` (R2.11's rule 7 names it: the
// `reef-agent` name, else the workspace's owner). The daemon's own entries
// (leases, snapshots' edits, progress) always name the workspace's owner, so
// a `ws run` with an agent name would otherwise count twice: once under the
// name, once under its owner. A daemon entry therefore belongs to every
// agent with a client entry on the same workspace, and to its own `agent`
// (the owner) only when no client entry names that workspace.
import type { Presence } from "../gen/reef/api/v1/presence_pb";

export interface FleetAgent {
  name: string;
  /** Its entries, client and daemon, in the order given. */
  entries: Presence[];
  /** Workspaces its entries name, in first-seen order. */
  workspaceIds: string[];
  /** Changes its entries name (hex), in first-seen order. */
  changeIds: string[];
}

export interface AgentJoin {
  /** By name, in first-seen order. */
  agents: Map<string, FleetAgent>;
  /** Each entry id's agents. */
  byEntry: Map<string, string[]>;
}

function add(list: string[], value: string) {
  if (value && !list.includes(value)) list.push(value);
}

/** The agents of `entries` in repository `repoId` (a full id). Other entries are left out. */
export function joinAgents(entries: Iterable<Presence>, repoId: string): AgentJoin {
  const mine = [...entries].filter((p) => p.repoId !== "" && p.repoId === repoId);
  // The client agents on each workspace.
  const clients = new Map<string, string[]>();
  for (const p of mine) {
    if (p.byDaemon || !p.workspaceId) continue;
    let names = clients.get(p.workspaceId);
    if (!names) clients.set(p.workspaceId, (names = []));
    add(names, p.agent);
  }
  const agents = new Map<string, FleetAgent>();
  const byEntry = new Map<string, string[]>();
  for (const p of mine) {
    const names = p.byDaemon ? (clients.get(p.workspaceId) ?? [p.agent]) : [p.agent];
    byEntry.set(p.id, names);
    for (const name of names) {
      let agent = agents.get(name);
      if (!agent)
        agents.set(name, (agent = { name, entries: [], workspaceIds: [], changeIds: [] }));
      agent.entries.push(p);
      add(agent.workspaceIds, p.workspaceId);
      add(agent.changeIds, p.changeId);
    }
  }
  return { agents, byEntry };
}

/** The names of the agents on workspace `id`, sorted. */
export function agentsOfWorkspace(join: AgentJoin, id: string): string[] {
  return [...join.agents.values()]
    .filter((agent) => agent.workspaceIds.includes(id))
    .map((agent) => agent.name)
    .sort();
}

/** The agents `agentCounts` counts, by name, per repository: the overview's cards. */
export function agentNames(entries: Iterable<Presence>): Map<string, string[]> {
  const byRepo = new Map<string, Presence[]>();
  for (const p of entries) {
    if (!p.repoId) continue;
    let list = byRepo.get(p.repoId);
    if (!list) byRepo.set(p.repoId, (list = []));
    list.push(p);
  }
  return new Map(
    [...byRepo].map(([repo, list]) => [repo, [...joinAgents(list, repo).agents.keys()]] as const),
  );
}

/** Each repository's number of agents. */
export function agentCounts(entries: Iterable<Presence>): Map<string, number> {
  return new Map([...agentNames(entries)].map(([repo, names]) => [repo, names.length] as const));
}
