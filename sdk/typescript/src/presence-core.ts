// R2.11's presence reporting contract as a state machine with no I/O.
//
// A port of reef_client::presence::Reporter's loop (and the Python SDK's core): one entry for one
// subject, replaced by id at most once a second, refreshed before its TTL runs out while the
// activity lasts, reported again without an id when the daemon no longer knows it, and cleared at
// the end. The Reporter drives it with timers; tests drive it with a fake clock. Times are
// seconds on a monotonic clock.

import { create } from "@bufbuild/protobuf";
import { durationFromMs } from "@bufbuild/protobuf/wkt";

import type { ErrorKind } from "./errors.js";
import {
  type PresenceKind,
  type Progress,
  type ReportPresenceRequest,
  ReportPresenceRequestSchema,
} from "./gen/reef/api/v1/presence_pb.js";

/** How often a long-lived reporter replaces its entry. */
export const REFRESH_EVERY = 10;
/** A long-lived reporter's TTL: three refreshes, the daemon's default. */
export const REFRESH_TTL = 30;
/** A bursty reporter's TTL, sent with each report. */
export const BURST_TTL = 60;
/** The least time between two reports of one entry. */
export const MIN_INTERVAL = 1;
/** How long a clear waits for the daemon in all. */
export const CLEAR_DEADLINE = 2;
/** Each report's deadline. */
export const CALL_DEADLINE = 5;

/**
 * Refusals after which reporting again cannot succeed: the subject is gone, the daemon does not
 * serve presence, or the report itself is wrong.
 */
export const ENDS_REPORTING: ReadonlySet<ErrorKind> = new Set<ErrorKind>([
  "conflict",
  "not_found",
  "unsupported",
  "invalid_argument",
  "permission_denied",
]);

/** When a reporter replaces its entry. */
export interface Cadence {
  /** Each report's TTL, in seconds. */
  readonly ttl: number;
  /** For an activity that lasts, the refresh interval; undefined for bursts, never refreshed. */
  readonly every?: number | undefined;
}

/** An activity that lasts: refreshed every 10 s with a 30 s TTL. */
export const REFRESH: Cadence = { ttl: REFRESH_TTL, every: REFRESH_EVERY };
/** Activity in bursts: a 60 s TTL, never refreshed. */
export const BURST: Cadence = { ttl: BURST_TTL };

/** An entry's fixed fields: a replacement must keep them. */
export interface Subject {
  readonly agent: string;
  /** Any kind but `PresenceKind.LEASE`. */
  readonly kind: PresenceKind;
  readonly repo?: string | undefined;
  readonly workspace?: string | undefined;
}

/** What an entry says: everything a replacement may change. */
export interface State {
  readonly activity?: string | undefined;
  readonly change?: string | undefined;
  readonly paths?: readonly string[] | undefined;
  /** For `PresenceKind.PROGRESS`. */
  readonly progress?: Progress | undefined;
}

/** Send this report now. */
export interface Send {
  readonly type: "send";
  readonly request: ReportPresenceRequest;
  readonly replacing: boolean;
}

/** Wait until `until` (undefined: until woken). */
export interface Wait {
  readonly type: "wait";
  readonly until: number | undefined;
}

/** Clear the entry, if one is held, and stop. */
export interface Close {
  readonly type: "close";
  readonly presenceId: string | undefined;
}

export type Action = Send | Wait | Close;

/** The reporter's state, and what it does next. */
export class Core {
  readonly subject: Subject;
  readonly cadence: Cadence;
  state: State | undefined;
  pending = false;
  sending = false;
  presenceId: string | undefined;
  liveUntil: number | undefined;
  closing = false;
  stopped = false;
  last: number | undefined;

  constructor(subject: Subject, cadence: Cadence) {
    this.subject = subject;
    this.cadence = cadence;
  }

  /** What the entry says from now on; whether the driver should wake. */
  set(state: State): boolean {
    if (this.stopped || this.closing) {
      return false;
    }
    this.state = state;
    this.pending = true;
    return true;
  }

  /** Stops reporting; the next action clears the entry. */
  close(): void {
    this.closing = true;
  }

  /**
   * Whether it may hold a live entry: a report is due or in flight, or the last one's TTL has not
   * run out.
   */
  isLive(now: number): boolean {
    return (
      !this.stopped &&
      (this.pending || this.sending || (this.liveUntil !== undefined && this.liveUntil > now))
    );
  }

  /** What to do at `now`. A Send marks the report in flight. */
  next(now: number): Action {
    if (this.closing) {
      return { type: "close", presenceId: this.presenceId };
    }
    let due: Wait | undefined;
    if (this.pending) {
      due = this.#due(now, MIN_INTERVAL);
    } else if (this.cadence.every !== undefined && this.state !== undefined) {
      due = this.#due(now, this.cadence.every);
    } else {
      due = { type: "wait", until: undefined };
    }
    if (due !== undefined) {
      return due;
    }
    this.pending = false;
    this.sending = true;
    this.last = now;
    return { type: "send", request: this.#request(), replacing: this.presenceId !== undefined };
  }

  /** A Wait while the interval since the last report has not passed, else undefined: send. */
  #due(now: number, after: number): Wait | undefined {
    if (this.last !== undefined && now < this.last + after) {
      return { type: "wait", until: this.last + after };
    }
    return undefined;
  }

  /** The report succeeded; the daemon's entry is `presenceId`. */
  answered(now: number, presenceId: string): void {
    this.sending = false;
    if (presenceId) {
      this.presenceId = presenceId;
    }
    this.liveUntil = now + this.cadence.ttl;
  }

  /**
   * The report failed (`kind` undefined: it timed out). A lapsed entry is reported anew at once;
   * a refusal that cannot change stops the reporter; anything else waits for the next set or
   * refresh.
   */
  failed(replacing: boolean, kind: ErrorKind | undefined, reason: string): void {
    this.sending = false;
    if (kind === undefined) {
      return;
    }
    if (replacing && reason === "PRESENCE_NOT_FOUND") {
      this.presenceId = undefined;
      this.liveUntil = undefined;
      this.pending = true;
      this.last = undefined;
      return;
    }
    if (ENDS_REPORTING.has(kind)) {
      this.stopped = true;
      this.presenceId = undefined;
    }
  }

  #request(): ReportPresenceRequest {
    const state = this.state ?? {};
    return create(ReportPresenceRequestSchema, {
      presenceId: this.presenceId ?? "",
      agent: this.subject.agent,
      repo: this.subject.repo ?? "",
      workspace: this.subject.workspace ?? "",
      change: state.change ?? "",
      kind: this.subject.kind,
      activity: state.activity ?? "",
      paths: [...(state.paths ?? [])],
      ttl: durationFromMs(Math.round(this.cadence.ttl * 1000)),
      ...(state.progress === undefined ? {} : { progress: state.progress }),
    });
  }
}
