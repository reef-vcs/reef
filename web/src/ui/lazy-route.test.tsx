// R2.21 test 9: a page loaded by `import()`, while it loads, when it fails and
// when the connection that failed it is back (R3.40).
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Connection } from "../api/connection";
import { streamsOf } from "../api/streams";
import { Dying } from "../test/dying";
import { fakeTransport, settle } from "../test/fakes";
import { act, cleanup, render, screen } from "../test/testing-library";
import { lazyRoute, loadAhead, LONG_MS } from "./lazy-route";

const reload = vi.fn();

beforeEach(() => {
  reload.mockReset();
  vi.stubGlobal("location", { reload });
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
      "could not be loaded. Reload to try again.",
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

  it("asks again for what the connection failed when the streams are live the first time", async () => {
    let asked = 0;
    const connection: Connection = { transport: fakeTransport({}), label: "test" };
    const Route = lazyRoute<PageProps>(() => {
      asked += 1;
      return asked < 3 ? Promise.reject(new TypeError("Load failed")) : Promise.resolve(Page);
    });
    render(<Route name="smartlog" connection={connection} />);
    await act(async () => {
      await settle(10);
    });
    expect(screen.getByRole("button", { name: "Reload" })).toBeTruthy();
    await act(async () => {
      streamsOf(connection).change(true);
      await settle(10);
    });
    expect(screen.getByText("the smartlog page")).toBeTruthy();
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

  describe("that takes long", () => {
    beforeEach(() => {
      vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout"] });
    });

    afterEach(() => {
      vi.useRealTimers();
    });

    const pass = async (ms: number) => {
      await act(async () => {
        await vi.advanceTimersByTimeAsync(ms);
        await settle(10);
      });
    };

    it("offers a reload after 10 s, and shows the page when it comes after all", async () => {
      let resolve: (page: typeof Page) => void = () => undefined;
      const { show, back } = line(
        () =>
          new Promise<typeof Page>((r) => {
            resolve = r;
          }),
      );
      show();
      await pass(LONG_MS - 1);
      expect(screen.queryByRole("button")).toBeNull();
      expect(screen.getByText("Loading the page.")).toBeTruthy();
      await pass(1);
      back();
      await pass(60 * 60_000);
      expect(reload, "nothing reloads on its own").not.toHaveBeenCalled();
      screen.getByRole("button", { name: "Reload" }).click();
      expect(reload).toHaveBeenCalledOnce();
      await act(async () => {
        resolve(Page);
        await settle(10);
      });
      expect(screen.getByText("the smartlog page")).toBeTruthy();
      expect(screen.queryByRole("button")).toBeNull();
      expect(vi.getTimerCount()).toBe(0);
    });

    it("loads every page ahead while the streams are live, one at a time, and nothing else is read", async () => {
      const dying = new Dying({ transport: fakeTransport({}), label: "test" });
      const streams = streamsOf(dying.connection);
      streams.change(false);
      let under = 0;
      let most = 0;
      const asked: string[] = [];
      const gives: (() => void)[] = [];
      const route = (name: string, then = () => {}) =>
        lazyRoute<PageProps>(async () => {
          asked.push(name);
          most = Math.max(most, ++under);
          await new Promise<void>((give) => gives.push(give));
          under -= 1;
          then();
          if (name === "gone") throw new TypeError("Load failed");
          return Page;
        });
      // Those of the tests before resolve, reject or hang; each is passed.
      route("frame", () => {
        route("nested");
      });
      route("gone");
      route("last");
      const stop = loadAhead(dying.connection);
      await pass(60 * 60_000);
      expect(asked, "not before the streams are live").toEqual([]);
      streams.change(true);
      const until = async (names: string[]) => {
        for (let i = 0; i < 400 && asked.length < names.length; i++) await pass(1_000);
        expect(asked).toEqual(names);
      };
      await until(["frame"]);
      streams.reads = 1;
      gives.shift()?.();
      await pass(60_000);
      expect(asked, "not while the page reads").toEqual(["frame"]);
      streams.reads = 0;
      streams.change(false);
      await pass(60_000);
      expect(asked, "not while the streams are lost").toEqual(["frame"]);
      asked.length = 0;
      for (let i = 0; i < 3; i++) streams.change(true);
      await until(["frame"]);
      gives.shift()?.();
      await until(["frame", "gone"]);
      gives.shift()?.();
      await until(["frame", "gone", "last"]);
      // One that hangs holds back the next for 40 s, not for good.
      await until(["frame", "gone", "last", "nested"]);
      gives.shift()?.();
      streams.change(false);
      await pass(60 * 60_000);
      expect(asked).toHaveLength(4);
      expect(most).toBe(2);
      asked.length = 0;
      streams.change(true);
      await until(["frame", "gone"]);
      stop();
      gives.shift()?.();
      await pass(60 * 60_000);
      expect(asked, "and not once the page is left").toEqual(["frame", "gone"]);
      expect(vi.getTimerCount()).toBe(0);
      expect(dying.held).toBe(0);
    });
  });
});
