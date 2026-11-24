// Whether a connection's streams are live (live/share.ts says), for what
// reads over it: what was read before the streams were lost may be old once
// they are back, and is read again. A page that is hidden then reads once it
// is shown.
import { useEffect, useState } from "react";
import { after, Backoff } from "../live/backoff";
import type { Connection } from "./connection";
import { isPassing, type ReefError } from "./errors";

export class Streams {
  live = false;
  private before = false;
  private owed = false;
  private readonly listeners = new Set<() => void>();

  /** Calls `run` whenever they are live again and the page is shown. */
  onResume = (run: () => void): (() => void) => {
    this.listeners.add(run);
    return () => {
      this.listeners.delete(run);
    };
  };

  change(live: boolean) {
    this.live = live;
    if (!live) return;
    this.owed = this.before;
    this.before = true;
    this.resume();
  }

  private resume = () => {
    if (!this.owed || !this.live) return;
    if (document.visibilityState === "hidden") {
      document.addEventListener("visibilitychange", this.resume, { once: true });
      return;
    }
    for (const run of [...this.listeners]) run();
  };
}

const made = new WeakMap<Connection, Streams>();

export function streamsOf(connection: Connection): Streams {
  let streams = made.get(connection);
  if (!streams) made.set(connection, (streams = new Streams()));
  return streams;
}

/**
 * A number that an effect that reads depends on, as `error` is how its last
 * read failed: it reads again after a backoff while another try may not meet
 * the error, and when the streams are live again if it failed or what it
 * reads can `change`.
 */
export function useAgain(connection: Connection, error: ReefError | null, change: boolean): number {
  const [again, setAgain] = useState(0);
  const [backoff] = useState(() => new Backoff(Backoff.PAGE_MS));
  const failed = error !== null;
  useEffect(() => {
    if (!change && !failed) return;
    return streamsOf(connection).onResume(() => {
      setAgain((before) => before + 1);
    });
  }, [connection, change, failed]);
  useEffect(() => {
    if (!error) backoff.reset();
    else if (isPassing(error)) {
      return after(backoff.next(), () => {
        setAgain((before) => before + 1);
      });
    }
  }, [error, backoff]);
  return again;
}
