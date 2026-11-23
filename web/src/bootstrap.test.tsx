// W3: the token from `#session=` is stored and stripped from the URL, a
// stored one is reused, and with neither the app shows the no-session page.
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { createRouterTransport } from "@connectrpc/connect";
import { accepted, client } from "./api/connection";
import { DaemonService } from "./gen/reef/api/v1/daemon_pb";
import {
  EMBEDDED_KEY,
  embedded,
  endSession,
  SESSION_KEY,
  storeSession,
  takeSession,
} from "./session";
import { cleanup, render, screen } from "./test/testing-library";

afterEach(() => {
  cleanup();
  localStorage.clear();
  sessionStorage.clear();
});

function at(url: string) {
  window.history.replaceState(null, "", url);
}

describe("takeSession", () => {
  it("takes the fragment's token and strips it and the launch id with replaceState", () => {
    at("/states?launch=0123456789abcdef&x=1#session=abc_DEF-123");
    const replace = vi.spyOn(window.history, "replaceState");
    expect(takeSession()).toBe("abc_DEF-123");
    expect(replace).toHaveBeenCalledOnce();
    expect(window.location.hash).toBe("");
    expect(window.location.pathname + window.location.search).toBe("/states?x=1");
  });

  it("reuses a stored token on reload and in a second tab of the origin", () => {
    storeSession("tok");
    at("/");
    expect(takeSession()).toBe("tok");
    // Another tab of the origin shares localStorage and has no fragment.
    at("/states");
    expect(takeSession()).toBe("tok");
  });

  it("takes the embedded flag with the token and keeps it for the tab", () => {
    at("/repos/r/proposals/c#session=tok&embedded=1");
    expect(takeSession()).toBe("tok");
    expect(window.location.hash).toBe("");
    expect(embedded()).toBe(true);
    // Kept for the tab only once the token is accepted: any page can link
    // here with the flag.
    expect(sessionStorage.getItem(EMBEDDED_KEY)).toBeNull();
    storeSession("tok");
    expect(sessionStorage.getItem(EMBEDDED_KEY)).toBe("1");
    sessionStorage.clear();
    at("/#session=tok");
    takeSession();
    expect(embedded()).toBe(false);
  });

  it("ignores a fragment that is not a session", () => {
    at("/#other=1");
    expect(takeSession()).toBeNull();
    expect(window.location.hash).toBe("#other=1");
  });

  it("works when storage throws", () => {
    at("/#session=tok");
    const storage = {
      getItem: () => {
        throw new Error("blocked");
      },
      setItem: () => {
        throw new Error("blocked");
      },
    } as unknown as Storage;
    expect(takeSession(window.location, window.history, storage)).toBe("tok");
    expect(takeSession(window.location, window.history, storage, storage)).toBeNull();
  });

  it("keeps a token for its tab until the daemon accepts it", () => {
    storeSession("mine");
    at("/#session=new");
    expect(takeSession()).toBe("new");
    // A reload before the first answer, in this tab and in another.
    expect(takeSession()).toBe("new");
    expect(takeSession(window.location, window.history, localStorage, null)).toBe("mine");
    // Refused: this tab falls back to the origin's.
    endSession("new");
    expect(takeSession()).toBe("mine");
  });

  // #346: any page can link to the web UI's port with a fragment of its own.
  it("stores a fragment's token once the daemon accepts it, and no refused one", async () => {
    storeSession("mine");
    for (const refuse of [true, false]) {
      at("/#session=new");
      const token = takeSession() ?? "";
      expect(token).toBe("new");
      const transport = createRouterTransport(
        (router) => {
          router.service(DaemonService, {
            getDaemonStatus: () => {
              if (refuse) throw new Error("refused");
              return {};
            },
          });
        },
        {
          transport: {
            interceptors: [
              accepted(() => {
                storeSession(token);
              }),
            ],
          },
        },
      );
      const daemon = client({ transport, label: "test" }, DaemonService);
      await daemon.getDaemonStatus({}).catch(() => undefined);
      expect(localStorage.getItem(SESSION_KEY)).toBe(refuse ? "mine" : "new");
    }
  });
});

describe("no session", () => {
  it("renders the no-session page", () => {
    at("/");
    expect(takeSession()).toBeNull();
    render(<App session={null} />);
    expect(screen.getByText(/This page has no session with the daemon/)).toBeTruthy();
    expect(screen.getByText("reef ui")).toBeTruthy();
  });
});
