// Whether a connection's streams are live (live/share.ts says), for what
// reads over it: what was read before the streams were lost may be old once
// they are back, and is read again.
import { useCallback, useEffect, useState, useSyncExternalStore } from "react";
import type { Connection } from "./connection";

export class Streams {
  live = false;
  private before = false;
  /** How often they were live again after they were lost. */
  resumes = 0;
  private readonly listeners = new Set<() => void>();

  /** Calls `run` whenever they are live again. */
  onResume = (run: () => void): (() => void) => {
    this.listeners.add(run);
    return () => {
      this.listeners.delete(run);
    };
  };

  change(live: boolean) {
    this.live = live;
    if (!live) return;
    const again = this.before;
    this.before = true;
    if (!again) return;
    this.resumes += 1;
    for (const listener of [...this.listeners]) listener();
  }
}

const made = new WeakMap<Connection, Streams>();

export function streamsOf(connection: Connection): Streams {
  let streams = made.get(connection);
  if (!streams) made.set(connection, (streams = new Streams()));
  return streams;
}

/** How often the streams were live again: an effect depends on it whose answer can change. */
export function useResumes(connection: Connection): number {
  const streams = streamsOf(connection);
  return useSyncExternalStore(
    streams.onResume,
    useCallback(() => streams.resumes, [streams]),
  );
}

/** As `useResumes`, counting only while `failed`: for an answer that never changes, once it came. */
export function useRetries(connection: Connection, failed: boolean): number {
  const [retries, setRetries] = useState(0);
  useEffect(() => {
    if (!failed) return;
    return streamsOf(connection).onResume(() => {
      setRetries((before) => before + 1);
    });
  }, [connection, failed]);
  return retries;
}
