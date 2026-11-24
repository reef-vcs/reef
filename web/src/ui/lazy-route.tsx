// One chunk per page (R2.21): a page loads by `import()` the first time it is
// shown. While it loads, its place in `main` says so and the header stays.
// Chunk names are content hashes served `immutable`, so a tab older than
// the daemon (one upgraded since the tab loaded) asks for chunks the daemon
// no longer embeds: the failure is shown, with a reload, never a blank page.
// Nothing reloads on its own, which could loop. A chunk the connection failed
// (R3.40) is asked for once more, and again when the page is shown, or the
// network or the streams are back. One that hangs on a connection that died
// cannot be asked for again: every `import()` of it waits for the first. The
// person reloads, unless it was here before it was needed (`loadAhead`).
import { lazy, Suspense, useEffect, useState, type ComponentType } from "react";
import type { Connection } from "../api/connection";
import { streamsOf } from "../api/streams";
import { after, sleep } from "../live/backoff";
import { onWake } from "../live/silence";

/** How long a page may load before it offers a reload. */
export const LONG_MS = 10_000;
/** How long a route that loads ahead may hold back the next. */
const AHEAD_MS = 40_000;

const loads: (() => Promise<unknown>)[] = [];

/** Loads each page's code, and that of its own pages as it comes. */
async function loadAll(connection: Connection, signal: AbortSignal) {
  const streams = streamsOf(connection);
  for (const load of loads) {
    do {
      await sleep(500, signal);
    } while (streams.reads > 0 && !signal.aborted);
    if (signal.aborted || !streams.live) return;
    await Promise.race([load().catch(() => undefined), sleep(AHEAD_MS, signal)]);
  }
}

/**
 * Loads the code of every page whenever the streams are live, one chunk at a
 * time and only while nothing is read, so that a tap needs no network. Until
 * the result is called.
 */
export function loadAhead(connection: Connection): () => void {
  let run = new AbortController();
  const stop = streamsOf(connection).onLive(() => {
    run.abort();
    run = new AbortController();
    void loadAll(connection, run.signal);
  });
  return () => {
    stop();
    run.abort();
  };
}

function Reload({ children }: { children: string }) {
  return (
    <>
      <p>{children}</p>
      <button
        type="button"
        onClick={() => {
          location.reload();
        }}
      >
        Reload
      </button>
    </>
  );
}

function ChunkError() {
  return (
    <div className="error" role="alert">
      <Reload>This page&apos;s code could not be loaded. Reload to try again.</Reload>
    </div>
  );
}

function Loading() {
  const [long, setLong] = useState(false);
  useEffect(
    () =>
      after(LONG_MS, () => {
        setLong(true);
      }),
    [],
  );
  return (
    <div className="secondary">
      {long ? (
        <Reload>Loading the page takes long. Reload if it does not come.</Reload>
      ) : (
        <p>Loading the page.</p>
      )}
    </div>
  );
}

interface Props {
  /** The connection whose streams a page waits for. */
  connection?: Connection;
}

/** A page whose code `load` imports, with the loading and failed states. */
export function lazyRoute<P extends Props>(
  load: () => Promise<ComponentType<P>>,
): ComponentType<P> {
  loads.push(load);
  let loaded: ComponentType<P> | null = null;
  function Failed(props: P) {
    const { connection } = props;
    const [Page, setPage] = useState(() => loaded);
    useEffect(() => {
      if (Page) return;
      const again = () => {
        load().then(
          (component) => {
            loaded = component;
            setPage(() => component);
          },
          () => undefined,
        );
      };
      again();
      const stops = [onWake(again), connection && streamsOf(connection).onLive(again)];
      return () => {
        for (const stop of stops) stop?.();
      };
    }, [Page, connection]);
    return Page ? <Page {...props} /> : <ChunkError />;
  }
  const Lazy = lazy(() =>
    load().then(
      (component) => ({ default: component }),
      () => ({ default: Failed as ComponentType<P> }),
    ),
  );
  function Route(props: P) {
    return (
      <Suspense fallback={<Loading />}>
        <Lazy {...props} />
      </Suspense>
    );
  }
  return Route;
}
