// The web UI in a real browser against a real daemon (#128): the session
// hand-off, one operation stream and one presence stream shared by two tabs,
// live updates of the overview and the smartlog, and the no-session page
// after a restart. Each project (browser) gets its own scratch daemon.
import { readFile } from "node:fs/promises";
import { join } from "node:path";
import type { BrowserContext, Page, Request } from "@playwright/test";
import { expect, test, WEB_DIR } from "./reef";

const STREAMS = [
  "/reef.api.v1.OperationService/SubscribeOperations",
  "/reef.api.v1.PresenceService/SubscribePresence",
];

/** Every stream request the context's pages make, with the page and whether it ended. */
class Streams {
  // Keyed by the request: Playwright passes the same object to each event.
  private readonly seen = new Map<Request, { path: string; page: Page; ended: boolean }>();

  constructor(context: BrowserContext) {
    context.on("request", (request) => {
      const path = new URL(request.url()).pathname;
      if (!STREAMS.includes(path)) return;
      const page = request.frame().page();
      this.seen.set(request, { path, page, ended: false });
      // A closed page's requests get no end event.
      page.once("close", () => {
        const entry = this.seen.get(request);
        if (entry) entry.ended = true;
      });
    });
    const end = (request: Request) => {
      const entry = this.seen.get(request);
      if (entry) entry.ended = true;
    };
    context.on("requestfinished", end);
    context.on("requestfailed", end);
  }

  /** Each stream path with the tabs that opened it and whether each request still runs. */
  summary(pages: Page[]) {
    const seen = [...this.seen.values()];
    return STREAMS.map((path) =>
      seen
        .filter((s) => s.path === path)
        .map((s) => ({ tab: pages.indexOf(s.page), open: !s.ended })),
    );
  }

  /** One operation stream and one presence stream, both from the first tab and running. */
  async hold(pages: Page[]) {
    await expect
      .poll(() => this.summary(pages), { message: "one of each stream, from the first tab" })
      .toEqual([[{ tab: 0, open: true }], [{ tab: 0, open: true }]]);
  }
}

/** A value on `window`, gone if the page reloads. */
async function mark(page: Page): Promise<string> {
  const marker = crypto.randomUUID();
  await page.evaluate((m) => {
    Object.assign(window, { __reefE2e: m });
  }, marker);
  return marker;
}

function marker(page: Page): Promise<unknown> {
  return page.evaluate(() => (window as unknown as { __reefE2e?: string }).__reefE2e);
}

function indicator(page: Page, word: string) {
  return page.getByRole("banner").getByText(word, { exact: true });
}

/** The `change <8>` of each smartlog row, in order. */
async function changeOrder(page: Page): Promise<string[]> {
  const labels = await page
    .getByRole("listbox", { name: "Changes" })
    .getByRole("option")
    .evaluateAll((els) => els.map((e) => e.getAttribute("aria-label") ?? ""));
  return labels.map((label) => /\bchange ([a-z]{8})\b/.exec(label)?.[1] ?? "");
}

test("smoke", async ({ reef, context, page }) => {
  const streams = new Streams(context);
  const [wsA, wsB] = reef.workspaces as [(typeof reef.workspaces)[0], (typeof reef.workspaces)[0]];
  const repoRow = (p: Page) =>
    p.getByRole("row").filter({ has: p.getByRole("link", { name: "e2e-fixture", exact: true }) });

  const first = await test.step("the daemon serves this checkout's app", async () => {
    const session = await reef.session();
    const script = /assets\/index-[^"]+\.js/;
    const served = script.exec(await (await fetch(session.url)).text())?.[0];
    const built = script.exec(
      await readFile(join(WEB_DIR, "../crates/reef-web/assets/index.html"), "utf8"),
    )?.[0];
    expect(built, "crates/reef-web/assets holds a build").toBeTruthy();
    expect(served, "the daemon embeds another build: run `just web-e2e`").toBe(built);
    return session;
  });
  const { url } = first;

  await test.step("the token leaves the address bar", async () => {
    // Assertions on the token compare it, never print it, so no failure
    // message or annotation carries one.
    await page.goto(first.sessionUrl);
    await expect.poll(() => new URL(page.url()).hash, { message: "no fragment" }).toBe("");
    expect(page.url(), "the address without the fragment").toBe(url);
    const stored = await page.evaluate(() => localStorage.getItem("reef.session"));
    expect(stored === first.token, "the token is stored").toBe(true);
    await expect(indicator(page, "Live")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Repositories" })).toBeVisible();
  });

  await test.step("the overview lists the repository", async () => {
    await expect(repoRow(page)).toContainText(reef.repo.slice(0, 8));
  });

  const second = await context.newPage();
  const tabs = [page, second];
  await test.step("a second tab shares the streams", async () => {
    await second.goto(url);
    await expect(indicator(second, "Live")).toBeVisible();
    await expect(repoRow(second)).toContainText(reef.repo.slice(0, 8));
    await streams.hold(tabs);
  });

  const markers = [await mark(page), await mark(second)];
  await test.step("a describe updates the overview live in both tabs", async () => {
    await reef.reef(["describe", "--workspace", wsB.id, "-m", "e2e: overview"], {
      agent: "e2e-overview",
    });
    for (const tab of tabs) await expect(repoRow(tab)).toContainText("describe · e2e-overview");
    expect(await marker(page)).toBe(markers[0]);
    expect(await marker(second)).toBe(markers[1]);
    await streams.hold(tabs);
  });

  await test.step("the smartlog updates the change's row in place", async () => {
    await page.getByRole("link", { name: "e2e-fixture", exact: true }).click();
    await expect(page).toHaveURL(`${url}repos/${encodeURIComponent(reef.repo)}/smartlog`);
    const options = page.getByRole("listbox", { name: "Changes" }).getByRole("option");
    const a8 = wsA.change.slice(0, 8);
    await expect.poll(() => changeOrder(page)).toContain(a8);
    const before = await changeOrder(page);
    const index = before.indexOf(a8);
    // The first load is in jj's order, with wsB's change (described above)
    // first. After wsA's describe jj would put wsA first: only the view's
    // ranks keep it here, so the check below cannot pass by accident.
    expect(index, "wsA's change is not the first row").toBeGreaterThan(0);
    const commit = await options.nth(index).getAttribute("data-commit");

    await reef.reef(["describe", "--workspace", wsA.id, "-m", "e2e: smartlog"], {
      agent: "e2e-smartlog",
    });
    const row = options.nth(index);
    await expect(row).toHaveAccessibleName(new RegExp(`\\bchange ${a8}, .*e2e: smartlog`));
    expect(await row.getAttribute("data-commit"), "a new commit").not.toBe(commit);
    await expect(row).toHaveAttribute("title", /by e2e-smartlog, describe$/);
    expect(await changeOrder(page)).toEqual(before);
    expect(await marker(page)).toBe(markers[0]);
    await streams.hold(tabs);
  });

  await test.step("a restart on a fixed port shows the no-session page", async () => {
    const port = new URL(url).port;
    await reef.stop();
    for (const tab of tabs) await expect(indicator(tab, "Reconnecting")).toBeVisible();
    // The streams the counter saw open do end: "open" can turn false.
    await expect
      .poll(() => streams.summary(tabs).map((path) => path[0]), { message: "both streams ended" })
      .toEqual([
        { tab: 0, open: false },
        { tab: 0, open: false },
      ]);
    await reef.writeConfig(`127.0.0.1:${port}`);
    await reef.start();
    const next = await reef.session();
    expect(next.url, "the same origin as the tabs").toBe(url);
    for (const tab of tabs) {
      await expect(tab.getByText("This page has no session with the daemon.")).toBeVisible({
        timeout: 30_000,
      });
    }
    const left = await page.evaluate(() => localStorage.getItem("reef.session"));
    expect(left === null, "the dead token is forgotten").toBe(true);

    // A load, not a same-document fragment change: the page is on the
    // smartlog, so this goto loads the app and its bootstrap takes the token.
    await page.goto(next.sessionUrl);
    await expect(indicator(page, "Live")).toBeVisible();
    await expect(page.getByRole("heading", { name: "Repositories" })).toBeVisible();
    await expect(repoRow(page)).toContainText(reef.repo.slice(0, 8));
  });
});
