// The design pass: the app bar is the one banner. It holds the brand, the
// connection, and Settings with the developer-mode switch; on a repository
// page the frame portals its name, tabs and scrubber into it.
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { App } from "../App";
import { environment, FakeStreams, manualFrames, settle } from "../test/fakes";
import { DEV_MODE_KEY, reloadDevMode } from "./dev-mode";
import { navigate } from "./router";

afterEach(() => {
  cleanup();
  localStorage.clear();
  reloadDevMode();
  window.history.replaceState(null, "", "/");
});

async function mount(path: string) {
  window.history.replaceState(null, "", path);
  const streams = new FakeStreams();
  const connection = streams.connection();
  const pending = () => new Promise(() => undefined);
  Object.assign(connection.clients, {
    daemon: { getDaemonStatus: () => Promise.resolve({ version: "0.2.0", apiRevision: 14 }) },
    repositories: { listRepositories: pending, getRepository: pending },
    operations: { ...connection.clients.operations, listOperations: pending },
  });
  render(
    <App session={{ connection, token: "tok" }} environment={environment(manualFrames().frame)} />,
  );
  await act(async () => {
    await settle(10);
  });
}

describe("the app bar", () => {
  it("has the brand, the connection and Settings, and no machine details", async () => {
    await mount("/");
    const banner = screen.getByRole("banner");
    expect(within(banner).getByRole("link", { name: "Reef" }).getAttribute("href")).toBe("/");
    expect(within(banner).getByRole("button", { name: "Settings" })).toBeTruthy();
    expect(banner.textContent).not.toContain("API revision");
    expect(banner.textContent).not.toContain("Developer mode");
  });

  it("opens Settings with the switch, the version and the links; Escape closes it", async () => {
    await mount("/");
    const button = screen.getByRole("button", { name: "Settings" });
    fireEvent.click(button);
    expect(button.getAttribute("aria-expanded")).toBe("true");
    const toggle = screen.getByRole("switch", { name: "Developer mode" });
    expect(toggle.getAttribute("aria-checked")).toBe("false");
    expect(screen.getByText("reef-daemon 0.2.0 · API revision 14")).toBeTruthy();
    expect(screen.getByRole("link", { name: "State colors" }).getAttribute("href")).toBe("/states");
    expect(screen.getByRole("link", { name: "Third-party licenses" })).toBeTruthy();
    fireEvent.keyDown(document, { key: "Escape" });
    expect(screen.queryByRole("switch")).toBeNull();
    expect(document.activeElement).toBe(button);
  });

  it("closes Settings on a click outside it", async () => {
    await mount("/");
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    fireEvent.pointerDown(document.body);
    expect(screen.queryByRole("switch")).toBeNull();
  });

  it("stays closed on returning to the page it was opened on (review finding 2)", async () => {
    await mount("/");
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    fireEvent.click(screen.getByRole("link", { name: "State colors" }));
    await act(async () => {
      await settle(10);
    });
    expect(screen.queryByRole("switch")).toBeNull();
    // jsdom does not traverse history: return through the router instead.
    act(() => {
      navigate("/");
    });
    expect(window.location.pathname).toBe("/");
    expect(screen.queryByRole("switch")).toBeNull();
  });

  it("turns developer mode on: the pill and the version show, and it persists", async () => {
    await mount("/");
    fireEvent.click(screen.getByRole("button", { name: "Settings" }));
    fireEvent.click(screen.getByRole("switch", { name: "Developer mode" }));
    expect(localStorage.getItem(DEV_MODE_KEY)).toBe("1");
    fireEvent.keyDown(document, { key: "Escape" });
    const banner = screen.getByRole("banner");
    expect(within(banner).getByText("Developer mode", { exact: true })).toBeTruthy();
    expect(banner.textContent).toContain("reef-daemon 0.2.0 · API revision 14");
  });

  it("holds a repository page's name, tabs and scrubber", async () => {
    await mount("/repos/r1/smartlog");
    const banner = screen.getByRole("banner");
    const nav = await within(banner).findByRole(
      "navigation",
      { name: "Repository" },
      { timeout: 5000 },
    );
    expect(within(nav).getByRole("link", { name: "Smartlog" }).getAttribute("aria-current")).toBe(
      "page",
    );
    expect(within(banner).getByRole("link", { name: "r1" })).toBeTruthy();
    expect(within(banner).getByRole("slider")).toBeTruthy();
  });
});
