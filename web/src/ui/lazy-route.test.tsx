// R2.21 test 9: a page loaded by `import()`, while it loads, when it fails and
// when the connection that failed it is back (R3.40).
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Connection } from "../api/connection";
import { Dying } from "../test/dying";
import { fakeTransport, settle } from "../test/fakes";
import { act, cleanup, render, screen } from "../test/testing-library";
import { lazyRoute } from "./lazy-route";

const reload = vi.fn();

beforeEach(() => {
  reload.mockReset();
  vi.stubGlobal("location", { reload, protocol: "https:" });
});

afterEach(() => {
  cleanup();
  vi.unstubAllGlobals();
});

interface PageProps {
  name: string;
  connection?: Connection;
}

function Page({ name }: PageProps) {
  return <p>the {name} page</p>;
}

function shell(Route: ReturnType<typeof lazyRoute<PageProps>>) {
  return (
    <>
      <header>the header</header>
      <main>
        <Route name="smartlog" />
      </main>
    </>
  );
}

/** A route of `load`'s over a connection, and its streams lost and live again. */
function line(load: () => Promise<typeof Page>) {
  const dying = new Dying({ transport: fakeTransport({}), label: "test" });
  const { connection } = dying;
  const Route = lazyRoute<PageProps>(load);
  return {
    dying,
    show: () => render(<Route name="smartlog" connection={connection} />),
    back: () => {
      dying.back();
    },
  };
}

describe("lazyRoute", () => {
  it("says the page is loading inside main, with the header kept, then renders it", async () => {
    let resolve: (page: typeof Page) => void = () => undefined;
    const Route = lazyRoute(
      () =>
        new Promise<typeof Page>((r) => {
          resolve = r;
        }),
    );
    render(shell(Route));
    expect(screen.getByText("Loading the page.").closest("main")).toBeTruthy();
    expect(screen.getByText("the header")).toBeTruthy();
    await act(async () => {
      resolve(Page);
      await settle(10);
    });
    expect(screen.getByText("the smartlog page")).toBeTruthy();
    expect(screen.queryByText("Loading the page.")).toBeNull();
  });

  it("shows a failed load with a reload, and the rest of the page stays", async () => {
    const Route = lazyRoute<PageProps>(() =>
      Promise.reject(new TypeError("Failed to fetch dynamically imported module")),
    );
    render(shell(Route));
    await act(async () => {
      await settle(10);
    });
    expect(screen.getByRole("alert").textContent).toContain(
      "If the daemon was updated, reload the page.",
    );
    expect(screen.getByRole("button", { name: "Reload" })).toBeTruthy();
    expect(screen.getByText("the header")).toBeTruthy();
  });

  it("asks again for what the connection failed: once at once, and when the network is back", async () => {
    let asked = 0;
    let dead = true;
    const Route = lazyRoute<PageProps>(() => {
      asked += 1;
      return dead ? Promise.reject(new TypeError("Load failed")) : Promise.resolve(Page);
    });
    render(shell(Route));
    await act(async () => {
      await settle(10);
    });
    expect(screen.getByRole("alert")).toBeTruthy();
    expect(asked).toBe(2);
    dead = false;
    await act(async () => {
      window.dispatchEvent(new Event("online"));
      await settle(10);
    });
    expect(screen.getByText("the smartlog page")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
    expect(asked).toBe(3);
    cleanup();
    render(shell(Route));
    await act(async () => {
      window.dispatchEvent(new Event("online"));
      await settle(10);
    });
    expect(screen.getByText("the smartlog page")).toBeTruthy();
    expect(asked).toBe(3);
  });

  it("asks again for what the connection failed when the streams are live again", async () => {
    let asked = 0;
    const { dying, show, back } = line(() => {
      asked += 1;
      return asked < 3 ? Promise.reject(new TypeError("Load failed")) : Promise.resolve(Page);
    });
    show();
    await act(async () => {
      await settle(10);
    });
    expect(screen.getByRole("alert")).toBeTruthy();
    expect(reload).not.toHaveBeenCalled();
    await act(async () => {
      back();
      await settle(10);
    });
    expect(screen.getByText("the smartlog page")).toBeTruthy();
    cleanup();
    expect(dying.held).toBe(0);
  });

  describe("still loading when the streams are live again", () => {
    let resolve: (page: typeof Page) => void = () => undefined;
    const hung = () =>
      line(
        () =>
          new Promise<typeof Page>((r) => {
            resolve = r;
          }),
      );
    const served = vi.fn<(url: string) => Promise<Response>>();
    const pass = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
        await settle(10);
      });
    };

    beforeEach(() => {
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
      vi.stubGlobal("fetch", served);
      served.mockClear();
      served.mockImplementation(() => Promise.resolve(new Response("code")));
      document.head.innerHTML =
        '<link rel="modulepreload" href="/assets/a.js"><link rel="modulepreload" href="/assets/b.js">';
    });

    afterEach(() => {
      vi.useRealTimers();
      document.head.innerHTML = "";
    });

    it("reloads once its files are here and it is not, a second later", async () => {
      const { dying, show, back } = hung();
      show();
      await pass(60 * 60_000);
      expect(served).not.toHaveBeenCalled();
      back();
      await pass(999);
      expect(served.mock.calls.map(([url]) => new URL(url).pathname)).toEqual([
        "/assets/a.js",
        "/assets/b.js",
      ]);
      expect(reload).not.toHaveBeenCalled();
      await pass(1);
      expect(reload).toHaveBeenCalledOnce();
      await pass(60 * 60_000);
      expect(reload, "nothing is on a timer").toHaveBeenCalledOnce();
      await act(async () => {
        resolve(Page);
        await settle(10);
      });
      expect(screen.getByText("the smartlog page")).toBeTruthy();
      expect(dying.held).toBe(0);
      back();
      await pass(10_000);
      expect(reload).toHaveBeenCalledOnce();
    });

    it("does not once it is shown, as over a slow link", async () => {
      const { show, back } = hung();
      show();
      back();
      await pass(500);
      await act(async () => {
        resolve(Page);
        await settle(10);
      });
      await pass(60_000);
      expect(reload).not.toHaveBeenCalled();
    });

    it.each([
      ["is not answered", () => new Promise<Response>(() => {})],
      ["fails", () => Promise.reject(new TypeError("Load failed"))],
      ["is gone", () => Promise.resolve(new Response("", { status: 404 }))],
      [
        "breaks off",
        () =>
          Promise.resolve(
            new Response(
              new ReadableStream({
                start(body) {
                  body.error(new TypeError("Load failed"));
                },
              }),
            ),
          ),
      ],
    ])("does not when a file %s, however often the streams are lost", async (_, answer) => {
      const { show, back } = hung();
      served.mockImplementation((url) =>
        url.endsWith("b.js") ? answer() : Promise.resolve(new Response("code")),
      );
      show();
      for (let i = 0; i < 20; i++) {
        back();
        await pass(2_000);
      }
      expect(reload).not.toHaveBeenCalled();
    });

    it("does not on the local page, or with nothing to go by", async () => {
      const { show, back } = hung();
      show();
      vi.stubGlobal("location", { reload, protocol: "http:" });
      back();
      await pass(10_000);
      expect(served).not.toHaveBeenCalled();
      vi.stubGlobal("location", { reload, protocol: "https:" });
      document.head.innerHTML = "";
      back();
      await pass(10_000);
      expect(reload).not.toHaveBeenCalled();
    });

    it("loses nothing a person typed", async () => {
      const { show, back } = hung();
      show();
      const field = document.createElement("input");
      field.type = "datetime-local";
      field.value = "2026-11-23T17:00";
      const slider = document.createElement("input");
      slider.type = "range";
      document.body.append(field, slider);
      back();
      await pass(10_000);
      expect(reload).not.toHaveBeenCalled();
      field.remove();
      back();
      await pass(10_000);
      expect(reload).toHaveBeenCalledOnce();
      slider.remove();
    });
  });
});
