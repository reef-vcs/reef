// Reads over a connection that can die with no FIN and no RST, as a phone's
// does: the streams' rule (live/silence.ts) for the calls that are not
// streams. A call the schema lets a client repeat is sent again when no whole
// answer came. One in which no byte arrives for a while is cut and sent
// again, and so is one that heard nothing while the streams were lost, once
// they are back: it may be on the dead connection. A slow answer whose bytes
// go on arriving is never cut.
import type { Interceptor } from "@connectrpc/connect";
import { after, Backoff, sleep } from "../live/backoff";
import { MAX_DOUBLINGS, orAborted, SILENT_MS } from "../live/silence";
import { isUnanswered, reefError } from "./errors";
import type { Streams } from "./streams";

/** Failures in a row, while the streams are live, that the page is told of. */
export const TRIES = 4;

export function steady(streams: () => Streams): {
  interceptor: Interceptor;
  fetch: typeof globalThis.fetch;
} {
  /** By a call's signal, which the transport hands to `fetch`: a byte of its answer came. */
  const heard = new WeakMap<AbortSignal, () => void>();

  const interceptor: Interceptor = (next) => async (request) => {
    if (request.stream || !request.method.idempotency) return next(request);
    const backoff = new Backoff();
    // A call, so TypeScript does not narrow it across the awaits.
    const ended = () => request.signal.aborted;
    let failures = 0;
    let unheard = 0;
    for (;;) {
      streams().reads += 1;
      const attempt = new AbortController();
      const cut = () => {
        attempt.abort();
      };
      /** A byte came while the streams were lost, so not on a dead connection. */
      let alive = false;
      let cancel = after(SILENT_MS * 2 ** Math.min(unheard, MAX_DOUBLINGS), () => {
        unheard += 1;
        cut();
      });
      heard.set(attempt.signal, () => {
        alive = !streams().live;
        cancel();
        cancel = after(SILENT_MS, cut);
      });
      request.signal.addEventListener("abort", cut, { once: true });
      if (ended()) cut();
      const stop = streams().onResume(() => {
        failures = 0;
        if (!alive) cut();
      });
      try {
        return await orAborted(next({ ...request, signal: attempt.signal }), attempt.signal);
      } catch (thrown) {
        if (!isUnanswered(reefError(thrown))) throw thrown;
        if (streams().live && ++failures >= TRIES) throw thrown;
        cancel();
        await sleep(backoff.next(), attempt.signal);
        if (ended()) throw thrown;
      } finally {
        streams().reads -= 1;
        cancel();
        stop();
        request.signal.removeEventListener("abort", cut);
      }
    }
  };

  return {
    interceptor,
    fetch: async (input, init) => {
      const response = await fetch(input, init);
      const byte = init?.signal ? heard.get(init.signal) : undefined;
      if (!byte || !response.body) return response;
      const body = response.body.pipeThrough(
        new TransformStream<Uint8Array, Uint8Array>({
          transform(chunk, to) {
            byte();
            to.enqueue(chunk);
          },
        }),
      );
      return new Response(body, response);
    },
  };
}
