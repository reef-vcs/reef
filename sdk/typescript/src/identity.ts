// Who is calling: the agent name, and a workspace's default owner, as reef-client has them.

import { hostname, userInfo } from "node:os";

import { InvalidArgumentError } from "./errors.js";

export const AGENT_ENV = "REEF_AGENT";
export const AGENT_METADATA = "reef-agent";
export const AGENT_MAX_BYTES = 256;

/**
 * `agent` trimmed, or undefined when blank. Throws InvalidArgumentError (`INVALID_AGENT`) unless
 * it is visible ASCII (spaces allowed) of at most 256 bytes, which the daemon would refuse.
 */
export function checkAgent(agent: string): string | undefined {
  // Python's str.strip and Rust's trim: Unicode whitespace at both ends.
  const text = agent.trim();
  if (!text) {
    return undefined;
  }
  const visible = /^[\x20-\x7e]*$/.test(text);
  if (Buffer.byteLength(text) > AGENT_MAX_BYTES || !visible) {
    throw new InvalidArgumentError(
      `an agent must be visible ASCII of at most ${AGENT_MAX_BYTES} bytes: ${JSON.stringify(agent)}`,
      { reason: "INVALID_AGENT" },
    );
  }
  return text;
}

/** The agent a client sends: `given`, else REEF_AGENT, checked. */
export function agentFrom(given: string | undefined): string | undefined {
  if (given !== undefined) {
    return checkAgent(given);
  }
  return checkAgent(process.env[AGENT_ENV] ?? "");
}

/**
 * The agent a presence entry names (R2.11's rule 7): the client's agent, else the workspace's
 * owner, else `fallback`; a name the daemon would refuse falls through.
 */
export function presenceAgent(
  given: string | undefined,
  owner: string | undefined,
  fallback: string,
): string {
  for (const name of [given, owner]) {
    if (name === undefined) {
      continue;
    }
    try {
      const checked = checkAgent(name);
      if (checked) {
        return checked;
      }
    } catch {
      // The daemon would refuse it; try the next.
    }
  }
  return fallback;
}

/** The user the process runs as: the password database entry, else $USER, else the uid. */
export function osUsername(): string {
  try {
    const name = userInfo().username;
    if (name) {
      return name;
    }
  } catch {
    // No password database entry.
  }
  return process.env["USER"] || String(process.getuid?.() ?? "");
}

/** The machine's hostname, or `localhost` if it has none. */
export function machineHostname(): string {
  return hostname() || "localhost";
}

/** `<user>@<host>`, as `reef ws create` sends when not given an owner. */
export function defaultOwner(): string {
  return `${osUsername()}@${machineHostname()}`;
}

/** `<user>@<host>:<pid>`, as `reef ws run` owns its workspace. */
export function processOwner(): string {
  return `${defaultOwner()}:${process.pid}`;
}
