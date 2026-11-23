// The session token (R2.18, ADR 0047). `reef ui` sends the browser to
// `/#session=<token>`. The bootstrap stores the token in `localStorage`
// (per origin, so every tab of this daemon's listener shares it) and strips
// it from the address bar, so no bookmark, copy or screenshot of the URL
// carries it. A host that cannot run passkey ceremonies, such as the
// desktop app's webview (R3.39), adds `&embedded=1`; the tab keeps it in
// `sessionStorage`, and the passkey pages hand the ceremony to the browser.

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

/** The stored token, whoever stored it. */
export function storedSession(storage: Storage | null = localStorageOrNull()): string | null {
  return read(storage);
}

/** Makes `token` the tab's session, and every later tab's of this origin. */
export function storeSession(token: string, storage: Storage | null = localStorageOrNull()) {
  write(storage, token);
}

/**
 * The tab's token: from `#session=` (stored, then removed from the URL), else
 * from storage. Null means the tab has no session.
 */
export function takeSession(
  location: Location = window.location,
  history: History = window.history,
  storage: Storage | null = localStorageOrNull(),
  tab: Storage | null = sessionStorageOrNull(),
): string | null {
  const match = FRAGMENT.exec(location.hash);
  if (match?.[1]) {
    write(storage, match[1]);
    if (match[2]) {
      try {
        tab?.setItem(EMBEDDED_KEY, "1");
      } catch {
        // Storage refused: this page load still knows (`embeddedNow`).
      }
      embeddedNow = true;
    }
    history.replaceState(history.state, "", location.pathname + location.search);
    return match[1];
  }
  return read(storage);
}

/**
 * Forget `token` after the daemon refused it (`WEB_SESSION_INVALID`). Only
 * if it is still the stored one: a tab `reef ui` opened since may have
 * stored a newer token, which other tabs of the origin now share.
 */
export function endSession(token: string, storage: Storage | null = localStorageOrNull()) {
  if (read(storage) === token) write(storage, null);
}
