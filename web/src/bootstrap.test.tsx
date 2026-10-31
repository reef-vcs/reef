// W3: the token from `#session=` is stored and stripped from the URL, a
// stored one is reused, and with neither the app shows the no-session page.
import { cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { App } from "./App";
import { SESSION_KEY, takeSession } from "./session";

afterEach(() => {
  cleanup();
  localStorage.clear();
});

function at(url: string) {
  window.history.replaceState(null, "", url);
}

describe("takeSession", () => {
  it("stores the fragment's token and strips it with replaceState", () => {
    at("/states?x=1#session=abc_DEF-123");
    const replace = vi.spyOn(window.history, "replaceState");
    expect(takeSession()).toBe("abc_DEF-123");
    expect(localStorage.getItem(SESSION_KEY)).toBe("abc_DEF-123");
    expect(replace).toHaveBeenCalledOnce();
    expect(window.location.hash).toBe("");
    expect(window.location.pathname + window.location.search).toBe("/states?x=1");
  });

  it("reuses a stored token on reload and in a second tab of the origin", () => {
    at("/#session=tok");
    takeSession();
    at("/");
    expect(takeSession()).toBe("tok");
    // Another tab of the origin shares localStorage and has no fragment.
    at("/states");
    expect(takeSession()).toBe("tok");
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
    expect(takeSession(window.location, window.history, storage)).toBeNull();
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
