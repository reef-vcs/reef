// Keeping a lease alive, as `reef ws run`'s heartbeater does (R1.21), with no I/O.
//
// Renews on the daemon's `heartbeatEvery`; while the daemon is unreachable, retries after 1, 2,
// 4, 8, 16, then 30 s, noting each change of the link once, and once that the lease ran out;
// stops when the daemon says the lease ended. The driver emits the notes as warnings.

import { durationMs, timestampMs } from "@bufbuild/protobuf/wkt";

import type { ErrorKind } from "./errors.js";
import type { Lease } from "./gen/reef/api/v1/workspaces_pb.js";

/** Each heartbeat's deadline, in seconds. */
export const HEARTBEAT_DEADLINE = 5;
/** The longest wait between heartbeats while the daemon is unreachable, in seconds. */
export const BACKOFF_CAP = 30;

/** Whether a heartbeat failed because the lease can no longer be renewed. */
export function leaseGone(kind: ErrorKind): boolean {
  return kind === "conflict" || kind === "not_found";
}

function lapse(lease: Lease, grace: number): number | undefined {
  if (lease.expiresAt === undefined) {
    return undefined;
  }
  return timestampMs(lease.expiresAt) + grace * 1000;
}

/** A note, and its warning code. */
export interface Note {
  readonly code:
    "REEF_LEASE_LINK_LOST" | "REEF_LEASE_LINK_BACK" | "REEF_LEASE_LAPSED" | "REEF_LEASE_ENDED";
  readonly message: string;
}

/** One lease's heartbeat schedule and what to say about it. */
export class LeaseCore {
  readonly workspace: string;
  readonly leaseId: string;
  every: number;
  readonly grace: number;
  /** When the lease runs out, in epoch milliseconds. */
  lapsesAt: number | undefined;
  warnedLapsed = false;
  failures = 0;
  up = true;

  /** For `lease` on `workspace`. Throws when the lease has no `heartbeatEvery`. */
  constructor(workspace: string, lease: Lease) {
    const every = lease.heartbeatEvery === undefined ? 0 : durationMs(lease.heartbeatEvery) / 1000;
    if (every <= 0) {
      throw new Error("the daemon's lease has no heartbeatEvery");
    }
    this.workspace = workspace;
    this.leaseId = lease.id;
    this.every = every;
    this.grace = lease.grace === undefined ? 0 : durationMs(lease.grace) / 1000;
    this.lapsesAt = lapse(lease, this.grace);
  }

  /** How long to wait before the next heartbeat, in seconds. */
  nextDelay(): number {
    if (this.up) {
      return this.every;
    }
    const doubling = Math.min(Math.max(this.failures - 1, 0), 5);
    return Math.min(2 ** doubling, BACKOFF_CAP);
  }

  /** A heartbeat answered with `lease`; the notes to emit. */
  renewed(lease: Lease | undefined): Note[] {
    if (lease === undefined) {
      return []; // as heartbeat.rs: an answer without a lease changes nothing
    }
    const notes: Note[] = [];
    if (!this.up) {
      notes.push({
        code: "REEF_LEASE_LINK_BACK",
        message: "reef: daemon reachable again; lease renewed",
      });
    }
    this.up = true;
    this.failures = 0;
    if (lease.heartbeatEvery !== undefined) {
      const every = durationMs(lease.heartbeatEvery) / 1000;
      if (every > 0) {
        this.every = every;
      }
    }
    this.lapsesAt = lapse(lease, this.grace) ?? this.lapsesAt;
    this.warnedLapsed = false;
    return notes;
  }

  /** A heartbeat failed for a reason other than the lease ending; the notes to emit. */
  unreachable(reason: string, nowMs: number): Note[] {
    const notes: Note[] = [];
    if (this.up) {
      notes.push({
        code: "REEF_LEASE_LINK_LOST",
        message: `reef: daemon unreachable (${reason}); retrying, and the block keeps running`,
      });
    }
    this.up = false;
    this.failures += 1;
    if (this.lapsesAt !== undefined && nowMs > this.lapsesAt && !this.warnedLapsed) {
      this.warnedLapsed = true;
      notes.push({
        code: "REEF_LEASE_LAPSED",
        message:
          `reef: lease ${this.leaseId} has expired; the daemon may reclaim workspace ` +
          `${this.workspace} and remove its directory`,
      });
    }
    return notes;
  }

  /** The lease ended; the note to emit. */
  gone(message: string): Note {
    return {
      code: "REEF_LEASE_ENDED",
      message:
        `reef: lease ${this.leaseId} cannot be renewed: ${message}; ` +
        "the workspace may be reclaimed",
    };
  }
}
