// A connection can die with no FIN and no RST: a phone that changes networks
// or sleeps keeps streams open that nothing will arrive on. Once a stream has
// sent a frame, the daemon sends another at least every 15 s, so one that is
// silent for longer is cut, and opened again as after any error. Each stream
// has its own limit and is cut alone. Sending on a dead connection is also
// what makes a browser notice it and open another (10 to 13 s in Chromium
// and WebKit, measured on a Mac).
//
// A first frame may take longer: the daemon may wait to read, or the link be
// slow. An opening that is cut before it doubles the next one's limit.
import { Code, ConnectError } from "@connectrpc/connect";
import { after, type Cancel } from "./backoff";

/** Silence that cuts a stream. */
export const SILENT_MS = 40_000;
/** Silence that cuts it when the page is shown or the network is back. */
export const SILENT_ON_WAKE_MS = 20_000;
/** Up to 320 s for a first frame, in which a frame of 1 MiB arrives at 3.3 kB/s. */
export const MAX_DOUBLINGS = 3;

/**
 * Time that a suspended page or a set clock cannot hide: `mono` may stop
 * while the device sleeps, and `wall` may go back.
 */
export interface Clock {
  mono: () => number;
  wall: () => number;
}

export interface SilenceEnvironment {
  clock: Clock;
  after: (ms: number, run: () => void) => Cancel;
  /** Runs `run` when the page is shown again or the network is back, until the result is called. */
  onWake: (run: () => void) => () => void;
}

/** Reads what `open` opens, until `signal` aborts or it is cut. */
export type Line = <T>(
  signal: AbortSignal,
  open: (signal: AbortSignal) => AsyncIterable<T>,
) => AsyncIterable<T>;

export function onWake(run: () => void): () => void {
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

export const browserSilence: SilenceEnvironment = {
  clock: { mono: () => performance.now(), wall: () => Date.now() },
  after,
  onWake,
};

/** One stream's line: it is cut when it falls silent. */
export function line({ clock, after, onWake }: SilenceEnvironment = browserSilence): Line {
  /** Openings in a row cut before a frame. */
  let unheard = 0;
  return async function* (signal, open) {
    const controller = new AbortController();
    const cut = () => {
      controller.abort();
    };
    signal.addEventListener("abort", cut, { once: true });
    let last = { mono: clock.mono(), wall: clock.wall() };
    let heard = false;
    let cancel: Cancel = () => undefined;
    const elapsed = () => Math.max(clock.mono() - last.mono, clock.wall() - last.wall);
    const limit = () => SILENT_MS * 2 ** Math.min(unheard, MAX_DOUBLINGS);
    const check = (most: number) => {
      if (controller.signal.aborted) return;
      if (elapsed() < most) {
        arm();
        return;
      }
      if (!heard) unheard += 1;
      cut();
    };
    const arm = () => {
      cancel();
      cancel = after(Math.max(limit() - elapsed(), 0), () => {
        check(limit());
      });
    };
    const stopWaking = onWake(() => {
      check(heard ? SILENT_ON_WAKE_MS : limit());
    });
    try {
      if (signal.aborted) return;
      arm();
      const frames = open(controller.signal)[Symbol.asyncIterator]();
      for (;;) {
        const next = await orAborted(frames.next(), controller.signal);
        if (next.done) return;
        last = { mono: clock.mono(), wall: clock.wall() };
        heard = true;
        unheard = 0;
        arm();
        yield next.value;
      }
    } finally {
      cancel();
      stopWaking();
      signal.removeEventListener("abort", cut);
      // The request of a stream that nothing reads any more ends with it.
      cut();
    }
  };
}

/** `step`, or a refusal as soon as `signal` aborts, whatever `step` then does. */
function orAborted<T>(step: Promise<T>, signal: AbortSignal): Promise<T> {
  return new Promise((resolve, reject) => {
    const aborted = () => {
      reject(new ConnectError("the connection fell silent", Code.Unavailable));
    };
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
