// A connection whose reads the test counts, fails and hangs, and whose
// streams it loses and has back.
import { Code, ConnectError } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import { streamsOf } from "../api/streams";

export class Dying {
  /** Methods called, in order. */
  calls: string[] = [];
  /** What becomes of a call: on a dead connection it hangs, until the browser gives that up. */
  state: "alive" | "hung" | "failing" = "alive";
  /** The one method that the state is for, if not every one. */
  only: string | undefined;
  /** What waits to hear that the streams are live again. */
  held = 0;
  readonly connection: Connection;
  private hung: (() => void)[] = [];

  constructor(inner: Connection) {
    this.connection = {
      label: inner.label,
      transport: {
        stream: (...args) => inner.transport.stream(...args),
        unary: async (...args) => {
          this.calls.push(args[0].localName);
          if (this.only && args[0].localName !== this.only) return inner.transport.unary(...args);
          if (this.state === "hung") await new Promise<void>((give) => this.hung.push(give));
          if (this.state !== "alive") throw new ConnectError("Load failed", Code.Unknown);
          return inner.transport.unary(...args);
        },
      },
    };
    const streams = streamsOf(this.connection);
    streams.change(true);
    const { onResume } = streams;
    streams.onResume = (run) => {
      this.held += 1;
      const stop = onResume(run);
      return () => {
        this.held -= 1;
        stop();
      };
    };
  }

  /** The browser gives the dead connection up, and what hung on it fails. */
  giveUp() {
    this.state = "failing";
    for (const give of this.hung.splice(0)) give();
  }

  /** The streams are lost. */
  lose() {
    streamsOf(this.connection).change(false);
  }

  /** The streams are live again. */
  back() {
    this.lose();
    streamsOf(this.connection).change(true);
  }
}
