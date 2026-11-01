// Lease health without a clock (R2.20). A lease's state changes with time
// and no event marks the change, so the page shows what events and reads
// establish, and times rather than predictions. Nothing here reads the
// clock: every word is a function of leases, presence and the daemon's
// uptime as last read.
//
// The daemon's `LEASE` entry is observed liveness: it is published at every
// issue and renewal and lapses two heartbeat intervals later (ADR 0037,
// decision 11). Only `reef ws run`, the MCP server and store builds
// heartbeat; a `reef ws create --lease 24h` holder never does, and relies on
// the lease's length. So an absent entry means "stuck" only for a lease that
// has been renewed: one never renewed reads `not heartbeating`, a holder's
// choice rather than a stall.
import {
  timestampFromMs,
  timestampMs,
  type Duration,
  type Timestamp,
} from "@bufbuild/protobuf/wkt";
import { LeaseState, type Lease } from "../gen/reef/api/v1/workspaces_pb";
import { clock, dateTime } from "../ui/time";

export type HealthWord =
  "heartbeating" | "not heartbeating" | "waiting for a heartbeat" | "missed heartbeats";

export interface Health {
  /** `-` while the presence set is not synced: an absence then means nothing. */
  word: HealthWord | "-";
  title: string;
  /** The panel's "stuck" signal: a renewing holder stopped. */
  stuck: boolean;
}

function ms(timestamp: Timestamp | undefined): number {
  return timestamp ? timestampMs(timestamp) : Number.NEGATIVE_INFINITY;
}

export function durationMs(duration: Duration | undefined): number {
  return duration ? Number(duration.seconds) * 1000 + duration.nanos / 1e6 : 0;
}

/**
 * The later of the listed lease and those the workspace's `LEASE` entries
 * carried, by `expires_at`: the publisher's own rule. A tie keeps the listed
 * one, whose state the daemon computed when it answered.
 */
export function foldLease(listed: Lease | undefined, carried: Iterable<Lease>): Lease | undefined {
  let best = listed;
  for (const lease of carried) {
    if (!best || ms(lease.expiresAt) > ms(best.expiresAt)) best = lease;
  }
  return best;
}

/** The lease has been renewed at least once since it was issued. */
export function renewed(lease: Lease): boolean {
  return ms(lease.lastHeartbeatAt) > ms(lease.issuedAt);
}

export function leaseHealth({
  lease,
  heartbeating,
  synced,
  restarting,
}: {
  lease: Lease | undefined;
  /** The workspace has a live `LEASE` entry. */
  heartbeating: boolean;
  /** The presence set is complete. */
  synced: boolean;
  /** Within two heartbeat intervals of a daemon start (`restartWindowMs`). */
  restarting: boolean;
}): Health {
  if (!synced || !lease) return { word: "-", title: "", stuck: false };
  const every = Math.round(durationMs(lease.heartbeatEvery) / 1000);
  const last = clock(lease.lastHeartbeatAt).text;
  if (heartbeating) {
    return {
      word: "heartbeating",
      title: `last heartbeat ${last}, every ${String(every)} s`,
      stuck: false,
    };
  }
  if (!renewed(lease)) {
    return {
      word: "not heartbeating",
      title: `issued ${clock(lease.issuedAt).text}, never renewed; ${expiry(lease).text}`,
      stuck: false,
    };
  }
  if (restarting) {
    return {
      word: "waiting for a heartbeat",
      title: `the daemon started recently; last heartbeat ${last}`,
      stuck: false,
    };
  }
  return { word: "missed heartbeats", title: `last heartbeat ${last}`, stuck: true };
}

/** A time, with the date when it is not on `reference`'s day. */
function at(timestamp: Timestamp | undefined, reference: Timestamp | undefined) {
  const time = clock(timestamp);
  const day = (t: Timestamp | undefined) => dateTime(t).text.slice(0, 10);
  return day(timestamp) === day(reference) ? time : dateTime(timestamp);
}

/** The lease's expiry, from its state as last reported, never predicted. */
export function expiry(lease: Lease): { text: string; title: string } {
  const reference = lease.lastHeartbeatAt ?? lease.issuedAt;
  if (lease.state === LeaseState.EXPIRED) {
    return { text: "expired, to be reclaimed", title: clock(lease.expiresAt).title };
  }
  if (lease.state === LeaseState.IN_GRACE) {
    const until = ms(lease.expiresAt) + durationMs(lease.grace);
    const time = at(timestampFromMs(until), reference);
    return { text: `in grace until ${time.text}`, title: time.title };
  }
  const time = at(lease.expiresAt, reference);
  return { text: `expires ${time.text}`, title: time.title };
}

/**
 * How long the restart window has left: while the daemon has run less than
 * two heartbeat intervals (the largest among `leases`), an absent entry is a
 * heartbeat not yet due, not a missed one. Measured from the daemon's own
 * uptime, so no clock skew enters.
 */
export function restartWindowMs(uptimeMs: number, leases: Iterable<Lease>): number {
  let every = 0;
  for (const lease of leases) every = Math.max(every, durationMs(lease.heartbeatEvery));
  return Math.max(0, 2 * every - uptimeMs);
}
