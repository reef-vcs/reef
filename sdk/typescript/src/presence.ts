// Reporting presence under R2.11's contract: the core (presence-core.ts) driven by timers.

import { Code } from "@connectrpc/connect";

import type { Client } from "./client.js";
import { isReefError } from "./errors.js";
import {
  BURST,
  CALL_DEADLINE,
  CLEAR_DEADLINE,
  Core,
  REFRESH,
  type Cadence,
  type State,
  type Subject,
} from "./presence-core.js";
import { debug, monotonic, sleep } from "./warn.js";

export { BURST, REFRESH };
export type { Cadence, State, Subject };

/** A Reporter's deadlines, in seconds. */
export interface ReporterOptions {
  /** How long `clear()` waits for the daemon in all; 2 s by default. */
  clearDeadline?: number;
  /** Each report's deadline; 5 s by default. */
  callDeadline?: number;
}

/**
 * Holds one presence entry for one subject, as `reef presence report` does from a shell: set what
 * it says with `set()`, and `clear()` it at the end. Failures never reach the caller: they are
 * debug lines (`NODE_DEBUG=reef`).
 */
export class Reporter {
  readonly #client: Client;
  readonly #core: Core;
  readonly #clearDeadline: number;
  readonly #callDeadline: number;
  readonly #inflight = new AbortController();
  readonly #task: Promise<void>;
  #wake: (() => void) | undefined;
  #woken = false;

  constructor(
    client: Client,
    subject: Subject,
    cadence: Cadence = REFRESH,
    options: ReporterOptions = {},
  ) {
    this.#client = client;
    this.#core = new Core(subject, cadence);
    this.#clearDeadline = options.clearDeadline ?? CLEAR_DEADLINE;
    this.#callDeadline = options.callDeadline ?? CALL_DEADLINE;
    this.#task = this.#run().catch((error: unknown) => {
      // Presence never fails the work: a client closed under it ends reporting quietly.
      debug("presence reporting stopped: %s", error);
    });
  }

  /**
   * What the entry says from now on: sent at once if the last report was a second ago or more,
   * else at that second's end with whatever was set last.
   */
  set(state: State): void {
    if (this.#core.set(state)) {
      this.#poke();
    }
  }

  /** Whether it may still hold a live entry. */
  isLive(): boolean {
    return this.#core.isLive(monotonic());
  }

  /** Stops reporting and clears the entry, waiting at most `clearDeadline` in all. */
  async clear(): Promise<void> {
    this.#core.close();
    this.#poke();
    const deadline = new AbortController();
    const timedOut = sleep(this.#clearDeadline * 1000, deadline.signal).then(() => "timeout");
    const result = await Promise.race([this.#task.then(() => "done"), timedOut]);
    deadline.abort();
    if (result === "timeout") {
      this.#inflight.abort();
    }
  }

  #poke(): void {
    this.#woken = true;
    this.#wake?.();
  }

  async #waitUntil(until: number | undefined): Promise<void> {
    if (this.#woken) {
      this.#woken = false;
      return;
    }
    const woken = new Promise<void>((resolve) => {
      this.#wake = resolve;
    });
    const waits: Promise<void>[] = [woken];
    const stop = new AbortController();
    if (until !== undefined) {
      waits.push(sleep((until - monotonic()) * 1000, stop.signal));
    }
    await Promise.race(waits);
    stop.abort();
    this.#wake = undefined;
    this.#woken = false;
  }

  async #run(): Promise<void> {
    for (;;) {
      if (this.#client.connection.closed) {
        return;
      }
      const action = this.#core.next(monotonic());
      if (action.type === "wait") {
        if (action.until === undefined || action.until > monotonic()) {
          await this.#waitUntil(action.until);
        }
        continue;
      }
      const options = { timeoutMs: this.#callDeadline * 1000, signal: this.#inflight.signal };
      if (action.type === "close") {
        if (action.presenceId !== undefined) {
          try {
            await this.#client.clearPresence({ presenceId: action.presenceId }, options);
          } catch (error) {
            debug("presence clear failed: %s", error);
          }
        }
        return;
      }
      let presenceId: string;
      try {
        const response = await this.#client.reportPresence(action.request, options);
        presenceId = response.presence?.id ?? "";
      } catch (error) {
        if (this.#inflight.signal.aborted) {
          return;
        }
        if (isReefError(error) && error.code === Code.DeadlineExceeded) {
          debug("presence report timed out");
          this.#core.failed(action.replacing, undefined, "");
          continue;
        }
        const kind = isReefError(error) ? error.kind : "internal";
        const reason = isReefError(error) ? error.reason : "";
        debug("presence report failed: %s", error);
        this.#core.failed(action.replacing, kind, reason);
        if (this.#core.stopped) {
          debug("presence reporting stopped");
          return;
        }
        continue;
      }
      this.#core.answered(monotonic(), presenceId);
    }
  }
}
