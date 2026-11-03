// How the SDK speaks: warnings through process.emitWarning, which a caller can route with
// process.on("warning") or silence with --no-warnings, and debug lines under NODE_DEBUG=reef. It
// never writes to the console itself.

import { debuglog } from "node:util";

/** Debug lines, shown with `NODE_DEBUG=reef`. */
export const debug = debuglog("reef");

/** Emits a `ReefWarning` with `code`. */
export function warn(message: string, code: string): void {
  process.emitWarning(message, { type: "ReefWarning", code });
}

/** A promise that resolves after `ms`, without keeping the process alive. */
export function sleep(ms: number, signal?: AbortSignal): Promise<void> {
  return new Promise((resolve) => {
    if (signal?.aborted === true) {
      resolve();
      return;
    }
    const timer = setTimeout(done, Math.max(0, ms));
    timer.unref();
    signal?.addEventListener("abort", done, { once: true });
    function done(): void {
      clearTimeout(timer);
      signal?.removeEventListener("abort", done);
      resolve();
    }
  });
}

/** Seconds on a monotonic clock. */
export function monotonic(): number {
  return performance.now() / 1000;
}
