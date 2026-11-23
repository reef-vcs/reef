// The session token (R2.18, ADR 0047). `reef ui` sends the browser to
// `/?launch=<hex>#session=<token>`. The bootstrap strips both from the
// address bar, so no bookmark, copy or screenshot of the URL carries them,
// and keeps the token for the tab in `sessionStorage`. Once the daemon
// accepts it, it moves to `localStorage` (per origin, so every tab of this
// daemon's listener shares it). Not before: the port is known (#346), and any
// page can link to it with a fragment of its own. A host that cannot run passkey ceremonies, such as the
// desktop app's webview (R3.39), adds `&embedded=1`; the tab keeps it in
// `sessionStorage` once the token is accepted, and the passkey pages hand the
// ceremony to the browser.

export const SESSION_KEY = "reef.session";
export const EMBEDDED_KEY = "reef.embedded";

const FRAGMENT = /^#session=([A-Za-z0-9_-]+)(&embedded=1)?$/;

function read(storage: Storage | null): string | null {
  try {
    return storage?.getItem(SESSION_KEY) ?? null;
  } catch {
    return null;
  }
}

function write(storage: Storage | null, token: string | null) {
  try {
    if (token === null) storage?.removeItem(SESSION_KEY);
    else storage?.setItem(SESSION_KEY, token);
  } catch {
    // Storage refused (a private window, blocked site data): the session
    // lasts as long as this tab.
  }
}

function sessionStorageOrNull(): Storage | null {
  try {
    return window.sessionStorage;
  } catch {
    return null;
  }
}

let embeddedNow = false;

export function embedded(tab: Storage | null = sessionStorageOrNull()): boolean {
  if (embeddedNow) return true;
  try {
    return tab?.getItem(EMBEDDED_KEY) === "1";
  } catch {
    return false;
  }
}

export function localStorageOrNull(): Storage | null {
  try {
    return window.localStorage;
  } catch {
    return null;
  }
}

/** Share `token`, which the daemon has accepted, with the origin's tabs. */
export function storeSession(
  token: string,
  storage: Storage | null = localStorageOrNull(),
  tab: Storage | null = sessionStorageOrNull(),
) {
  write(storage, token);
  write(tab, null);
  if (!embeddedNow) return;
  try {
    tab?.setItem(EMBEDDED_KEY, "1");
  } catch {
    // Storage refused: this page load still knows (`embeddedNow`).
  }
}

/**
 * The tab's token: from `#session=` (removed from the URL, with the launch
 * id), else the tab's own not yet accepted, else the origin's. Null means the
 * tab has no session.
 */
export function takeSession(
  location: Location = window.location,
  history: History = window.history,
  storage: Storage | null = localStorageOrNull(),
  tab: Storage | null = sessionStorageOrNull(),
): string | null {
  const match = FRAGMENT.exec(location.hash);
  if (match?.[1]) {
    write(tab, match[1]);
    embeddedNow = match[2] !== undefined;
    const query = new URLSearchParams(location.search);
    query.delete("launch");
    const rest = query.toString() && `?${query.toString()}`;
    history.replaceState(history.state, "", location.pathname + rest);
    return match[1];
  }
  return read(tab) ?? read(storage);
}

/**
 * Forget `token` after the daemon refused it (`WEB_SESSION_INVALID`). Only
 * if it is still the stored one: a tab `reef ui` opened since may have
 * stored a newer token, which other tabs of the origin now share.
 */
export function endSession(
  token: string,
  storage: Storage | null = localStorageOrNull(),
  tab: Storage | null = sessionStorageOrNull(),
) {
  if (read(storage) === token) write(storage, null);
  if (read(tab) === token) write(tab, null);
}

/**
 * Removes every service worker and cache of a local web UI's origin. Reef
 * makes none there, so another process that held the port left them (#346).
 * The remote listener's origin is left alone: the phone's app has a worker of
 * its own (R3.40).
 */
export async function dropForeignWorkers(hostname: string = window.location.hostname) {
  if (hostname !== "localhost") return;
  try {
    const workers = await navigator.serviceWorker.getRegistrations();
    await Promise.all(workers.map((worker) => worker.unregister()));
    await Promise.all((await caches.keys()).map((name) => caches.delete(name)));
  } catch {
    // No service workers here, or site data is blocked.
  }
}
