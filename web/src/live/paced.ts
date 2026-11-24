// One read in flight, spaced by its own cost (R2.19's rule, shared since
// R2.20): a request during a run asks for exactly one more, and a run starts
// no sooner than `max(250 ms, 4 × the last run)` after the last one started,
// and no later than 10 s after it ended: a run's time by the clock holds what
// it hung on a dead connection and what the device slept. A burst is one run
// and a trailing one, and a slow daemon is asked less often. Nothing runs on
// a timer: the only delays are the spacing after a request and the backoff
// after a run that no answer came to.
import { after, Backoff, type Cancel } from "./backoff";

/** The least time between the starts of two runs. */
export const MIN_SPACING_MS = 250;
/** The most between the end of one and the start of the next. */
export const MAX_SPACING_MS = 10_000;

/** How long a run waits that is asked for `now`, after one that began at `started`. */
export function spacing(started: number, duration: number, now: number): number {
  const apart = Math.max(MIN_SPACING_MS, duration + Math.min(3 * duration, MAX_SPACING_MS));
  return Math.min(started + apart - now, MAX_SPACING_MS);
}

/**
 * What a run does. It returns undefined when it did not start (say, the
 * stream is not live), which records no start and no duration, and resolves
 * to false when it was not answered (`isPassing`), and is then run again after
 * the backoff, or as soon as one is asked for, spaced as ever.
 */
export type Run = () => Promise<boolean> | undefined;

/** Starts a run: at once by default, or at an animation frame, so a hidden tab runs nothing. */
export type Gate = (start: () => void) => void;

export class Paced {
  private loading = false;
  private again = false;
  /** The run asked for during this one was the user's: not spaced. */
  private againNow = false;
  private pending: Cancel | undefined;
  /** The backoff after a run no answer came to: one that is asked for takes its place. */
  private retry: Cancel | undefined;
  private stopped = false;
  private lastStart = Number.NEGATIVE_INFINITY;
  private lastDuration = 0;
  private readonly backoff = new Backoff(Backoff.PAGE_MS);

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
    this.cancel();
    const wait = spacing(this.lastStart, this.lastDuration, this.now());
    if (wait <= 0) void this.start();
    else this.pending = this.startIn(wait);
  }

  /**
   * A run the user asked for: not spaced, since the spacing paces runs the
   * stream asks for. One in flight still finishes first.
   */
  requestNow() {
    if (this.stopped) return;
    this.cancel();
    if (this.loading) {
      this.again = true;
      this.againNow = true;
    } else void this.start();
  }

  stop() {
    this.stopped = true;
    this.cancel();
  }

  /** A call, so TypeScript does not narrow `stopped` across an await. */
  private isStopped(): boolean {
    return this.stopped;
  }

  private cancel() {
    this.pending?.();
    this.pending = undefined;
    this.retry?.();
  }

  private startIn(ms: number): Cancel {
    return after(ms, () => {
      this.pending = undefined;
      this.gate(() => void this.start());
    });
  }

  private async start() {
    if (this.stopped) return;
    const started = this.now();
    const work = this.run();
    if (!work) return;
    this.loading = true;
    this.lastStart = started;
    let answered = true;
    try {
      answered = await work;
    } finally {
      this.loading = false;
      this.lastDuration = this.now() - started;
      if (answered) this.backoff.reset();
      const now = this.againNow;
      const again = this.again;
      this.again = false;
      this.againNow = false;
      if (now) void this.start();
      else if (again) this.request();
      else if (!answered && !this.isStopped()) this.retry = this.startIn(this.backoff.next());
    }
  }
}
