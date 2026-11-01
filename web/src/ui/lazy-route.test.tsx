// R2.21 test 9: a page loaded by `import()`, while it loads and when it fails.
import { act, cleanup, render, screen } from "@testing-library/react";
import { afterEach, describe, expect, it } from "vitest";
import { settle } from "../test/fakes";
import { lazyRoute } from "./lazy-route";

afterEach(cleanup);

function Page({ name }: { name: string }) {
  return <p>the {name} page</p>;
}

function shell(Route: ReturnType<typeof lazyRoute<{ name: string }>>) {
  return (
    <>
      <header>the header</header>
      <main>
        <Route name="smartlog" />
      </main>
    </>
  );
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
    const Route = lazyRoute<{ name: string }>(() =>
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
});
