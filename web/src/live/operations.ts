// The operation stream: one unfiltered `SubscribeOperations` per browser,
// resumed exactly from the last frame's cursor (R2.09), delivered to views
// as one batch per animation frame.
import { create } from "@bufbuild/protobuf";
import { Code } from "@connectrpc/connect";
import {
  SubscribeOperationsResponseSchema,
  type Operation,
  type SubscribeOperationsResponse,
} from "../gen/reef/api/v1/operations_pb";
import {
  OPERATION_LOG_REWRITTEN,
  TOO_FAR_BEHIND,
  isSessionInvalid,
  reefError,
  type ReefError,
} from "../api/errors";
import type { Backoff } from "./backoff";
import type { FrameScheduler } from "./frame";

export interface OperationBatch {
  /** Operations since the last batch, in stream order. */
  operations: Operation[];
  /** The stream has caught up with the log at least once since the last reset. */
  live: boolean;
  /** The stream started over: reload whatever was derived from earlier batches. */
  reset: boolean;
}

/**
 * Operations held for the next batch at most. A hidden tab gets no animation
 * frames, so what it reads waits; past this many, the batch becomes a reset
 * and views reload instead of replaying.
 */
export const PENDING_LIMIT = 10_000;

export class OperationStore {
  /** The last frame's cursor, from this tab's stream or the leader's. */
  cursor: string | undefined;
  live = false;
  private pending: Operation[] = [];
  private pendingReset = false;
  private scheduled = false;
  private readonly listeners = new Set<(batch: OperationBatch) => void>();

  constructor(private readonly frame: FrameScheduler) {}

  subscribe(listener: (batch: OperationBatch) => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  applyFrame(frame: SubscribeOperationsResponse) {
    if (frame.cursor) this.cursor = frame.cursor;
    const becameLive = frame.live && !this.live;
    if (frame.live) this.live = true;
    if (frame.operations.length === 0 && !becameLive) return;
    if (!this.pendingReset) {
      this.pending.push(...frame.operations);
      if (this.pending.length > PENDING_LIMIT) {
        this.pending = [];
        this.pendingReset = true;
      }
    }
    this.schedule();
  }

  /** The stream starts over with no cursor. */
  reset() {
    this.cursor = undefined;
    this.live = false;
    this.pending = [];
    this.pendingReset = true;
    this.schedule();
  }

  private schedule() {
    if (this.scheduled) return;
    this.scheduled = true;
    this.frame(() => {
      this.scheduled = false;
      const batch = { operations: this.pending, live: this.live, reset: this.pendingReset };
      this.pending = [];
      this.pendingReset = false;
      for (const listener of [...this.listeners]) listener(batch);
    });
  }
}

/** A synthetic frame that carries only a cursor and `live`, for a joining tab. */
export function positionFrame(store: OperationStore): SubscribeOperationsResponse {
  return create(SubscribeOperationsResponseSchema, {
    cursor: store.cursor ?? "",
    live: store.live,
  });
}

export interface StreamHooks {
  signal: AbortSignal;
  backoff: Backoff;
  sleep: (ms: number, signal: AbortSignal) => Promise<void>;
  /** A frame arrived (after the store has it). */
  onFrame?: (frame: SubscribeOperationsResponse) => void;
  /** The stream failed and will be retried. */
  onRetry?: (error: ReefError) => void;
  /** The daemon refused the session: the loop has stopped. */
  onSessionInvalid: () => void;
}

export type OpenOperations = (
  cursor: string | undefined,
  signal: AbortSignal,
) => AsyncIterable<SubscribeOperationsResponse>;

function startsOver(error: ReefError): boolean {
  return error.reason === TOO_FAR_BEHIND || error.reason === OPERATION_LOG_REWRITTEN;
}

/**
 * Reads the stream until `signal` aborts or the session ends. A transport
 * error, or `UNAVAILABLE` from a daemon shutting down, resumes from the
 * store's cursor, so nothing is missed or delivered twice.
 * `TOO_FAR_BEHIND` and `OPERATION_LOG_REWRITTEN` subscribe fresh and reset
 * the store.
 */
export async function runOperations(
  open: OpenOperations,
  store: OperationStore,
  hooks: StreamHooks,
  onReset?: () => void,
): Promise<void> {
  const { signal, backoff } = hooks;
  // A call, not `signal.aborted`, which TypeScript would narrow across the awaits.
  const aborted = () => signal.aborted;
  while (!aborted()) {
    try {
      for await (const frame of open(store.cursor, signal)) {
        backoff.reset();
        store.applyFrame(frame);
        hooks.onFrame?.(frame);
      }
      if (aborted()) return;
      // The daemon ended the stream without an error: resume like a drop.
      hooks.onRetry?.(reefError(new Error("the operation stream ended")));
    } catch (thrown) {
      if (aborted()) return;
      const error = reefError(thrown);
      if (isSessionInvalid(error)) {
        hooks.onSessionInvalid();
        return;
      }
      if (startsOver(error)) {
        store.reset();
        onReset?.();
      } else if (error.code === Code.InvalidArgument && store.cursor !== undefined) {
        // A cursor the daemon no longer understands: start over rather than
        // retry it forever.
        store.reset();
        onReset?.();
      }
      hooks.onRetry?.(error);
    }
    await hooks.sleep(backoff.next(), signal);
  }
}
