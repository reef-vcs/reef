// One chunk per page (R2.21): a page loads by `import()` the first time it is
// shown. While it loads, its place in `main` says so and the header stays.
// Chunk names are content hashes served `immutable`, so a tab older than
// the daemon (one upgraded since the tab loaded) asks for chunks the daemon
// no longer embeds: the failure is shown, with a reload, never a blank page.
// Nothing reloads on its own, which could loop.
import { lazy, Suspense, type ComponentType } from "react";

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

/** A page whose code `load` imports, with the loading and failed states. */
export function lazyRoute<P extends object>(
  load: () => Promise<ComponentType<P>>,
): ComponentType<P> {
  const Lazy = lazy(() =>
    load().then(
      (component) => ({ default: component }),
      () => ({ default: ChunkError as ComponentType<P> }),
    ),
  );
  function Route(props: P) {
    return (
      <Suspense fallback={<p className="secondary">Loading the page.</p>}>
        <Lazy {...props} />
      </Suspense>
    );
  }
  return Route;
}
