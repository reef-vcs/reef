// One read in flight, spaced by its own cost (R2.19's rule, shared since
// R2.20): a request during a run asks for exactly one more, and a run starts
// no sooner than `max(250 ms, 4 × the last run)` after the last one started.
// A burst is one run and a trailing one, and a slow daemon is asked less
// often. Nothing runs on a timer: the only delay is the spacing after a
// request.
import { after, type Cancel } from "./backoff";

/** The least time between the starts of two runs. */
export const MIN_SPACING_MS = 250;

/**
 * What a run does. It returns undefined when it did not start (say, the
 * stream is not live), which records no start and no duration.
 */
export type Run = () => Promise<void> | undefined;

/** Starts a run: at once by default, or at an animation frame, so a hidden tab runs nothing. */
export type Gate = (start: () => void) => void;

export class Paced {
  private loading = false;
  private again = false;
  /** The run asked for during this one was the user's: not spaced. */
  private againNow = false;
  private pending: Cancel | undefined;
  private stopped = false;
  private lastStart = Number.NEGATIVE_INFINITY;
  private lastDuration = 0;

  constructor(
    private readonly run: Run,
    private readonly now: () => number = Date.now,
    private readonly gate: Gate = (start) => {
      start();
    },
  ) {}

  /** A run is in flight. */
  get busy(): boolean {
    return this.loading;
  }

  /** Asks for a run: now, after the spacing, or after the one in flight. */
  request() {
    if (this.stopped) return;
    if (this.loading) {
      this.again = true;
      return;
    }
    if (this.pending) return;
    const spacing = Math.max(MIN_SPACING_MS, 4 * this.lastDuration);
    const wait = this.lastStart + spacing - this.now();
    if (wait <= 0) {
      void this.start();
      return;
    }
    this.pending = after(wait, () => {
      this.pending = undefined;
      this.gate(() => void this.start());
    });
  }

  /**
   * A run the user asked for: not spaced, since the spacing paces runs the
   * stream asks for. One in flight still finishes first.
   */
  requestNow() {
    if (this.stopped) return;
    this.pending?.();
    this.pending = undefined;
    if (this.loading) {
      this.again = true;
      this.againNow = true;
    } else void this.start();
  }

  stop() {
    this.stopped = true;
    this.pending?.();
    this.pending = undefined;
  }

  private async start() {
    if (this.stopped) return;
    const started = this.now();
    const work = this.run();
    if (!work) return;
    this.loading = true;
    this.lastStart = started;
    try {
      await work;
    } finally {
      this.loading = false;
      this.lastDuration = this.now() - started;
      if (this.again) {
        const now = this.againNow;
        this.again = false;
        this.againNow = false;
        if (now) void this.start();
        else this.request();
      }
    }
  }
}
