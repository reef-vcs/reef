// R2.19 test 3: routes with parameters and a query string.
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it } from "vitest";
import { href, Link, match, navigate, usePath, useSearch } from "./router";

beforeEach(() => {
  window.history.replaceState(null, "", "/");
});

afterEach(() => {
  cleanup();
});

describe("match", () => {
  it("decodes parameters", () => {
    expect(match("/repos/:repo/smartlog", "/repos/ab%2Fc/smartlog")).toEqual({ repo: "ab/c" });
  });

  it("refuses extra segments, a trailing slash, and empty or broken parameters", () => {
    expect(match("/repos/:repo/smartlog", "/repos/x/smartlog/extra")).toBeNull();
    expect(match("/repos/:repo/smartlog", "/repos/x/smartlog/")).toBeNull();
    expect(match("/repos/:repo/smartlog", "/repos//smartlog")).toBeNull();
    expect(match("/repos/:repo/smartlog", "/repos/%E0%A4%A/smartlog")).toBeNull();
    expect(match("/repos/:repo/smartlog", "/repo/x/smartlog")).toBeNull();
  });
});

describe("href", () => {
  it("leaves out empty values and encodes the rest", () => {
    expect(href("/a", { revset: "x | y", limit: undefined, change: "" })).toBe("/a?revset=x+%7C+y");
    expect(href("/a")).toBe("/a");
  });
});

function Where() {
  return (
    <p data-testid="where">
      {usePath()}
      {useSearch()}
    </p>
  );
}

describe("navigate", () => {
  it("re-renders on a push, a replace, a new query, and popstate", () => {
    render(<Where />);
    const where = () => screen.getByTestId("where").textContent;
    act(() => {
      navigate("/repos/r/smartlog");
    });
    expect(where()).toBe("/repos/r/smartlog");
    act(() => {
      navigate("/repos/r/smartlog?change=abc");
    });
    expect(where()).toBe("/repos/r/smartlog?change=abc");
    act(() => {
      window.history.replaceState(null, "", "/states");
      window.dispatchEvent(new PopStateEvent("popstate"));
    });
    expect(where()).toBe("/states");
  });

  it("adds a history entry for a push and none for a replace", () => {
    const before = window.history.length;
    navigate("/one");
    expect(window.history.length).toBe(before + 1);
    navigate("/one?x=1", { replace: true });
    expect(window.history.length).toBe(before + 1);
    expect(window.location.search).toBe("?x=1");
    navigate("/one?x=1");
    expect(window.history.length).toBe(before + 1);
  });
});

describe("Link", () => {
  it("navigates in the app with its query, and leaves modified clicks to the browser", () => {
    render(
      <>
        <Link to="/repos/r/smartlog?limit=400">smartlog</Link>
        <Where />
      </>,
    );
    const link = screen.getByRole("link", { name: "smartlog" });
    expect(link.getAttribute("href")).toBe("/repos/r/smartlog?limit=400");
    const modified = fireEvent.click(link, { button: 0, metaKey: true });
    expect(modified).toBe(true);
    expect(window.location.pathname).toBe("/");
    act(() => {
      fireEvent.click(link, { button: 0 });
    });
    expect(screen.getByTestId("where").textContent).toBe("/repos/r/smartlog?limit=400");
  });
});
