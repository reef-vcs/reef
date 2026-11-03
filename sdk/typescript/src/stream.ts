// A server stream: an async iterable that is also AsyncDisposable and can be cancelled.

/** Opens the call: the responses, given the signal that ends it. */
export type StreamOpener<T> = (signal: AbortSignal) => Promise<AsyncIterable<T>>;

/** Maps a failure of the call to the error the iteration throws. */
export type StreamErrorMapper = (error: unknown, received: boolean) => Promise<unknown>;

/**
 * The responses of a server-streaming RPC, for `for await`.
 *
 * The call starts on the first iteration. Breaking out of `for await`, leaving its `await using`
 * block, `cancel()`, or the call's `signal` ends it, after which iteration ends rather than
 * throwing. The daemon's errors, mid-stream ones included, are thrown as `ReefError`s. A stream is
 * one call: an ended stream yields nothing more.
 */
export class Stream<T> implements AsyncIterableIterator<T>, AsyncDisposable {
  readonly #controller = new AbortController();
  readonly #open: StreamOpener<T>;
  readonly #error: StreamErrorMapper;
  readonly #signal: AbortSignal | undefined;
  #opening: Promise<AsyncIterator<T>> | undefined;
  #received = false;
  #done = false;
  #unlink: (() => void) | undefined;

  /** @internal */
  constructor(open: StreamOpener<T>, error: StreamErrorMapper, signal?: AbortSignal) {
    this.#open = open;
    this.#error = error;
    this.#signal = signal;
  }

  /** Whether the stream was cancelled, by `cancel()`, disposal or the signal. */
  get cancelled(): boolean {
    return this.#controller.signal.aborted;
  }

  [Symbol.asyncIterator](): this {
    return this;
  }

  /** Opens the call once, on the first `next()`, and follows the caller's signal from then on. */
  #start(): Promise<AsyncIterator<T>> {
    if (this.#opening === undefined) {
      const signal = this.#signal;
      if (signal !== undefined) {
        if (signal.aborted) {
          this.cancel();
        } else {
          const onAbort = (): void => {
            this.cancel();
          };
          signal.addEventListener("abort", onAbort, { once: true });
          this.#unlink = () => {
            signal.removeEventListener("abort", onAbort);
          };
        }
      }
      this.#opening = this.#open(this.#controller.signal).then((responses) =>
        responses[Symbol.asyncIterator](),
      );
    }
    return this.#opening;
  }

  async next(): Promise<IteratorResult<T, undefined>> {
    if (this.#done || this.cancelled || this.#signal?.aborted === true) {
      return this.#finish();
    }
    try {
      const iterator = await this.#start();
      const result = await iterator.next();
      if (result.done === true) {
        return this.#finish();
      }
      this.#received = true;
      return { done: false, value: result.value };
    } catch (error) {
      if (this.#controller.signal.aborted) {
        return this.#finish();
      }
      this.#done = true;
      this.#unlink?.();
      throw await this.#error(error, this.#received);
    }
  }

  return(): Promise<IteratorResult<T, undefined>> {
    this.cancel();
    return Promise.resolve(this.#finish());
  }

  /** Ends the stream; iterating it afterwards stops at once. */
  cancel(): void {
    if (!this.#controller.signal.aborted) {
      this.#controller.abort();
    }
    this.#unlink?.();
  }

  async [Symbol.asyncDispose](): Promise<void> {
    this.cancel();
    return Promise.resolve();
  }

  #finish(): IteratorResult<T, undefined> {
    this.#done = true;
    this.#unlink?.();
    return { done: true, value: undefined };
  }
}
