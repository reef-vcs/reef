// The only module that schedules work (ESLint enforces it): the streams'
// reconnect backoff, and `after` for the few deliberate delays elsewhere.
// Nothing here fetches on a timer; a delay only spaces out a retry or a
// reload that an event asked for.

export type Cancel = () => void;

export function after(ms: number, run: () => void): Cancel {
  const id = setTimeout(run, ms);
  return () => {
    clearTimeout(id);
  };
}

/** Resolves after `ms`, or at once when `signal` aborts. */
export function sleep(ms: number, signal: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal.aborted) {
      resolve();
      return;
    }
    const cancel = after(ms, () => {
      signal.removeEventListener("abort", onAbort);
      resolve();
    });
    function onAbort() {
      cancel();
      resolve();
    }
    signal.addEventListener("abort", onAbort, { once: true });
  });
}

/** `reef op watch`'s backoff: 100 ms doubling to 2 s, then every 2 s, forever. */
export class Backoff {
  static readonly FIRST_MS = 100;
  static readonly MAX_MS = 2000;
  private delay = 0;

  next(): number {
    this.delay = this.delay === 0 ? Backoff.FIRST_MS : Math.min(this.delay * 2, Backoff.MAX_MS);
    return this.delay;
  }

  reset() {
    this.delay = 0;
  }
}
