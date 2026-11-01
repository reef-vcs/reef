// `/repos/<repo>/workspaces` (R2.20): its parameters and links. Kept apart
// from Fleet.tsx so the overview and the smartlog link here without
// importing the page, which imports them.
import { href } from "../ui/router";

export interface FleetParams {
  repo: string;
  /** A selected workspace's id, or empty. Wins over `agent`. */
  workspace: string;
  /** A selected agent's name, or empty. */
  agent: string;
  /** An operation: the page at that point, frozen. */
  at: string;
}

export function fleetParams(repo: string, search: string): FleetParams {
  const query = new URLSearchParams(search);
  const workspace = query.get("workspace") ?? "";
  return {
    repo,
    workspace,
    agent: workspace ? "" : (query.get("agent") ?? ""),
    at: query.get("at") ?? "",
  };
}

export function fleetHref(params: Partial<FleetParams> & { repo: string }): string {
  return href(`/repos/${encodeURIComponent(params.repo)}/workspaces`, {
    workspace: params.workspace,
    agent: params.workspace ? undefined : params.agent,
    at: params.at,
  });
}
