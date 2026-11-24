// One chunk per page (R2.21): a page loads by `import()` the first time it is
// shown. While it loads, its place in `main` says so and the header stays.
// Chunk names are content hashes served `immutable`, so a tab older than
// the daemon (one upgraded since the tab loaded) asks for chunks the daemon
// no longer embeds: the failure is shown, with a reload, never a blank page.
// Such a failure reloads nothing on its own, which could loop. A chunk the
// connection failed (R3.40) is asked for once more, and again when the page is
// shown, or the network or the streams are back. One still loading then may
// hang for good on the connection that died, and every `import()` of it waits
// for that (`reloadIfHung`).
import { lazy, Suspense, useEffect, useState, type ComponentType } from "react";
import type { Connection } from "../api/connection";
import { streamsOf } from "../api/streams";
import { after } from "../live/backoff";
import { onWake } from "../live/silence";

function ChunkError() {
  return (
    <div className="error" role="alert">
      <p>This page&apos;s code could not be loaded. If the daemon was updated, reload the page.</p>
      <button
        type="button"
        onClick={() => {
          location.reload();
        }}
      >
        Reload
      </button>
    </div>
  );
}

interface Props {
  /** The connection whose streams a page waits for. */
  connection?: Connection;
}

/** How long a page whose files are all here may take to show. */
const SHOW_MS = 1_000;

/** Whether the daemon serves every file that a page shown so far asked for. */
async function served(): Promise<boolean> {
  const files = document.querySelectorAll<HTMLLinkElement>('link[rel="modulepreload"]');
  const answers = await Promise.all(
    [...files].map((file) =>
      fetch(file.href)
        .then(async (response) => {
          await response.arrayBuffer();
          return response.ok;
        })
        .catch(() => false),
    ),
  );
  return answers.length > 0 && answers.every(Boolean);
}

function typed(): boolean {
  const fields = document.querySelectorAll<HTMLInputElement>(
    "textarea, input:not([type=range], [type=checkbox], [type=radio])",
  );
  return [...fields].some((field) => field.value !== "");
}

/**
 * Reloads the page of the remote listener once its files, asked for again, are
 * all here and one asked for before still is not. They are then in the
 * browser's cache, so the reload gets past them. A slow link answers the first
 * request first, and where the daemon does not answer nothing is reloaded.
 */
async function reloadIfHung(loading: () => boolean) {
  if (location.protocol !== "https:" || !(await served())) return;
  after(SHOW_MS, () => {
    if (loading() && !typed()) location.reload();
  });
}

function Loading({ connection }: { connection: Connection | undefined }) {
  useEffect(() => {
    if (!connection) return;
    let loading = true;
    const stop = streamsOf(connection).onResume(() => {
      void reloadIfHung(() => loading);
    });
    return () => {
      loading = false;
      stop();
    };
  }, [connection]);
  return <p className="secondary">Loading the page.</p>;
}

/** A page whose code `load` imports, with the loading and failed states. */
export function lazyRoute<P extends Props>(
  load: () => Promise<ComponentType<P>>,
): ComponentType<P> {
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
      const stops = [onWake(again), connection && streamsOf(connection).onResume(again)];
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
      <Suspense fallback={<Loading connection={props.connection} />}>
        <Lazy {...props} />
      </Suspense>
    );
  }
  return Route;
}
