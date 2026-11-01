// The design pass: developer mode is off by default, stored per origin, safe
// when storage throws, and followed across tabs.
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DEV_MODE_KEY, Dev, devMode, Human, reloadDevMode, setDevMode } from "./dev-mode";

afterEach(() => {
  cleanup();
  vi.restoreAllMocks();
  window.localStorage.clear();
  reloadDevMode();
});

describe("developer mode", () => {
  it("is off by default", () => {
    expect(devMode()).toBe(false);
  });

  it("stores 1 when on and removes the key when off", () => {
    setDevMode(true);
    expect(window.localStorage.getItem(DEV_MODE_KEY)).toBe("1");
    setDevMode(false);
    expect(window.localStorage.getItem(DEV_MODE_KEY)).toBeNull();
  });

  it("reads a stored setting", () => {
    window.localStorage.setItem(DEV_MODE_KEY, "1");
    reloadDevMode();
    expect(devMode()).toBe(true);
  });

  it("still toggles for the tab when storage throws", () => {
    vi.spyOn(Storage.prototype, "setItem").mockImplementation(() => {
      throw new Error("blocked");
    });
    setDevMode(true);
    expect(devMode()).toBe(true);
  });

  it("follows another tab's change, and renders Dev and Human by mode", () => {
    render(
      <>
        <Dev>
          <span>machine</span>
        </Dev>
        <Human>
          <span>person</span>
        </Human>
      </>,
    );
    expect(screen.queryByText("machine")).toBeNull();
    expect(screen.getByText("person")).toBeTruthy();
    act(() => {
      window.localStorage.setItem(DEV_MODE_KEY, "1");
      window.dispatchEvent(new StorageEvent("storage", { key: DEV_MODE_KEY }));
    });
    expect(screen.getByText("machine")).toBeTruthy();
    expect(screen.queryByText("person")).toBeNull();
  });
});
