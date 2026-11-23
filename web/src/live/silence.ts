// A connection can die with no FIN and no RST: a phone that changes networks
// or sleeps keeps streams open that nothing will arrive on. The daemon sends
// every stream a frame at least every 15 s, so one that has been silent for
// longer is cut, with the others, and each is opened again as after any
// error. Sending on such a connection is also what makes a browser notice
// it and open another (10 to 13 s in Chromium and WebKit, measured).
import { Code, ConnectError } from "@connectrpc/connect";
import type { Cancel } from "./backoff";

/** Silence that cuts the streams. */
export const SILENT_MS = 40_000;
/** Silence that cuts them when the page is shown or the network is back. */
export const SILENT_ON_WAKE_MS = 20_000;
/**
 * Each cut that finds a stream still without a frame since it was opened
 * doubles the wait for the next, so a daemon slow to answer is not asked
 * over and over.
 */
const MAX_DOUBLINGS = 3;

/**
 * Time that a suspended page or a set clock cannot hide: `mono` may stop
 * while the device sleeps, and `wall` may go back.
 */
export interface Clock {
  mono: () => number;
  wall: () => number;
}

export const browserClock: Clock = {
  mono: () => performance.now(),
  wall: () => Date.now(),
};

/** When something last happened. */
export class Since {
  private mono: number;
  private wall: number;

  constructor(private readonly clock: Clock) {
    this.mono = clock.mono();
    this.wall = clock.wall();
  }

  now() {
    this.mono = this.clock.mono();
    this.wall = this.clock.wall();
  }

  /** The longer of the two clocks' times since. */
  elapsed(): number {
    return Math.max(this.clock.mono() - this.mono, this.clock.wall() - this.wall);
  }
}

/** Runs `run` when the page is shown again or the network is back. */
export function onWake(run: () => void): () => void {
  if (typeof document === "undefined") return () => undefined;
  const shown = () => {
    if (document.visibilityState !== "hidden") run();
  };
  document.addEventListener("visibilitychange", shown);
  window.addEventListener("pageshow", run);
  window.addEventListener("online", run);
  return () => {
    document.removeEventListener("visibilitychange", shown);
    window.removeEventListener("pageshow", run);
    window.removeEventListener("online", run);
  };
}

interface Attempt {
  since: Since;
  /** Whether a frame arrived since it was opened. */
  heard: boolean;
  cut: () => void;
}

/** The open streams of a tab, cut together when one falls silent. */
export class Silence {
  private readonly attempts = new Set<Attempt>();
  private silentOpens = 0;
  private cancel: Cancel | undefined;

  constructor(
    private readonly clock: Clock,
    private readonly after: (ms: number, run: () => void) => Cancel,
  ) {}

  /** `open`'s frames, until `signal` aborts or the streams are cut. */
  async *watch<T>(
    signal: AbortSignal,
    open: (signal: AbortSignal) => AsyncIterable<T>,
  ): AsyncGenerator<T> {
    const controller = new AbortController();
    const cut = () => {
      controller.abort();
    };
    if (signal.aborted) cut();
    signal.addEventListener("abort", cut, { once: true });
    const attempt: Attempt = { since: new Since(this.clock), heard: false, cut };
    this.attempts.add(attempt);
    this.arm();
    try {
      const frames = open(controller.signal)[Symbol.asyncIterator]();
      for (;;) {
        const next = await orAborted(frames.next(), controller.signal);
        if (next.done) return;
        attempt.since.now();
        attempt.heard = true;
        this.silentOpens = 0;
        yield next.value;
      }
    } finally {
      this.attempts.delete(attempt);
      signal.removeEventListener("abort", cut);
      // A cut or abandoned stream's request ends with it.
      controller.abort();
      if (this.attempts.size === 0) this.disarm();
    }
  }

  /** Cuts the streams if one has been silent for `limit`. */
  check(limit: number) {
    const attempts = [...this.attempts];
    if (!attempts.some((attempt) => attempt.since.elapsed() >= limit)) return;
    if (attempts.some((attempt) => !attempt.heard)) this.silentOpens += 1;
    this.attempts.clear();
    this.disarm();
    for (const attempt of attempts) attempt.cut();
  }

  private limit(): number {
    return SILENT_MS * 2 ** Math.min(this.silentOpens, MAX_DOUBLINGS);
  }

  private arm() {
    if (this.cancel || this.attempts.size === 0) return;
    const limit = this.limit();
    const longest = Math.max(...[...this.attempts].map((attempt) => attempt.since.elapsed()));
    this.cancel = this.after(Math.max(limit - longest, 1000), () => {
      this.cancel = undefined;
      this.check(limit);
      this.arm();
    });
  }

  private disarm() {
    this.cancel?.();
    this.cancel = undefined;
  }
}

/** `step`, or a refusal as soon as `signal` aborts, whatever `step` then does. */
function orAborted<T>(step: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const aborted = () => {
      reject(new ConnectError("the connection fell silent", Code.Unavailable));
    };
    if (signal.aborted) {
      aborted();
      return;
    }
    signal.addEventListener("abort", aborted, { once: true });
    step.then(
      (value) => {
        signal.removeEventListener("abort", aborted);
        resolve(value);
      },
      (thrown: unknown) => {
        signal.removeEventListener("abort", aborted);
        reject(thrown instanceof Error ? thrown : new Error(String(thrown)));
      },
    );
  });
}
