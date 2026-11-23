// What a process that held the web UI's port leaves in the browser (#346):
// service workers and cached pages and scripts, all at the origin a policy
// pins. A launch from `reef ui` clears the address first, from a page at a
// port no one could have prepared. Chromium removes everything then. WebKit
// does not honour the header for another origin: there the daemon ends a
// worker when the browser asks for its script, and Reef's page removes the
// rest, so a squatter with many workers outlives some launches (S-276).
import { readdirSync } from "node:fs";
import { createServer, type Server } from "node:http";
import { join } from "node:path";
import type { Page } from "@playwright/test";
import { expect, freePort, test, WEB_DIR, type Reef } from "./reef";

const ASSETS = join(WEB_DIR, "../crates/reef-web/assets/assets");

/** Answers every navigation in its scope. */
const WORKER = `
  self.addEventListener("install", () => self.skipWaiting());
  self.addEventListener("activate", (event) => event.waitUntil(self.clients.claim()));
  self.addEventListener("fetch", (event) => {
    if (event.request.mode === "navigate")
      event.respondWith(new Response("<!doctype html><title>Squatter</title>", {
        headers: { "content-type": "text/html" },
      }));
  });`;

function squat(path: string) {
  if (path === "/sw.js") return { type: "text/javascript", body: WORKER };
  if (path.endsWith(".css")) return { type: "text/css", body: "body { display: none }" };
  if (path.endsWith(".js")) return { type: "text/javascript", body: "globalThis.squatted = true;" };
  return { type: "text/html", body: "<!doctype html><title>Squatter</title>" };
}

async function listen(port: number): Promise<Server> {
  const server = createServer((request, response) => {
    const { type, body } = squat((request.url ?? "/").split("?")[0] ?? "/");
    response.setHeader("content-type", type);
    response.setHeader("cache-control", "public, max-age=31536000, immutable");
    // Chromium's cache then answers any query with the page cached for another.
    response.setHeader("no-vary-search", "params");
    response.end(body);
  });
  await new Promise<void>((resolve) => server.listen(port, "localhost", resolve));
  return server;
}

const registrations = (page: Page) =>
  page.evaluate(async () => (await navigator.serviceWorker.getRegistrations()).length);

/** The daemon stopped, its port squatted and the browser poisoned; started again. */
async function poisoned(page: Page, reef: Reef, scopes: string[], updateViaCache: string) {
  const port = await freePort();
  await reef.stop();
  await reef.writeConfig(`127.0.0.1:${String(port)}`);
  const origin = `http://localhost:${String(port)}`;
  const squatter = await listen(port);
  for (const path of ["/passkeys", "/?launch=0", "/"]) await page.goto(`${origin}${path}`);
  const names = readdirSync(ASSETS).map((name) => `/assets/${name}`);
  expect(names.length).toBeGreaterThan(0);
  await page.evaluate(
    async ({ names, scopes, updateViaCache }) => {
      await Promise.all(names.map((name) => fetch(name).then((response) => response.text())));
      for (const scope of scopes) {
        await navigator.serviceWorker.register("/sw.js", {
          scope,
          updateViaCache: updateViaCache as ServiceWorkerUpdateViaCache,
        });
      }
    },
    { names, scopes, updateViaCache },
  );
  await page.goto(`${origin}/`);
  await expect(page).toHaveTitle("Squatter");
  if (scopes.length > 0) expect(await registrations(page)).toBe(scopes.length);
  squatter.closeAllConnections();
  await new Promise((resolve) => squatter.close(resolve));
  await reef.start();
  return origin;
}

/** Reef's own page is what shows, and none of the squatter's scripts ran. */
async function reefs(page: Page) {
  await expect(page.getByRole("heading", { name: "Repositories" })).toBeVisible();
  expect(await page.evaluate(() => "squatted" in globalThis)).toBe(false);
  expect(await registrations(page)).toBe(0);
}

test("a worker and cached pages a squatter left end", async ({ page, reef }) => {
  const origin = await poisoned(page, reef, ["/"], "imports");
  await test.step("the first launch may be the squatter's, and ends its worker", async () => {
    await page.goto(await reef.launch());
    await page.waitForURL(`${origin}/**`);
    await expect.poll(() => registrations(page), { timeout: 30_000 }).toBe(0);
  });
  await test.step("the next launch is Reef's, and runs none of its scripts", async () => {
    await page.goto(await reef.launch());
    await reefs(page);
    for (const path of ["/", "/passkeys"]) {
      await page.goto(`${origin}${path}?fresh=1`);
      await expect(page.locator("#root")).not.toBeEmpty();
      expect(await page.evaluate(() => "squatted" in globalThis)).toBe(false);
    }
  });
});

test.describe("a determined squatter", () => {
  test.skip(
    ({ browserName }) => browserName === "webkit",
    "WebKit does not honour Clear-Site-Data from another origin (S-276)",
  );

  for (const [name, scopes, updateViaCache] of [
    ["a worker the browser never checks for updates", ["/"], "all"],
    [
      "workers for many addresses",
      ["/", "/passkeys", ..."0123456789abcdef".split("").map((hex) => `/?launch=${hex}`)],
      "all",
    ],
    ["a cached page for every query", [], "imports"],
  ] as const) {
    test(`${name} is gone at the first launch`, async ({ page, reef }) => {
      const origin = await poisoned(page, reef, [...scopes], updateViaCache);
      await page.goto(await reef.launch());
      await page.waitForURL(`${origin}/**`);
      await reefs(page);
      // A bookmark, and the launch URL of another session, are Reef's too.
      await page.goto(`${origin}/`);
      await reefs(page);
    });
  }
});
