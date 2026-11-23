// What a process that held the web UI's port leaves in the browser (#346):
// service workers never checked for updates, pages cached for every query,
// and storage, at every name it can guess. A run of the daemon has a name no
// one could, so the first launch is Reef's and finds none of it.
import { createServer } from "node:http";
import { expect, freePort, test } from "./reef";

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

test("what a squatter left at the port is not at a run's name", async ({ page, reef }) => {
  const port = await freePort();
  await reef.stop();
  await reef.writeConfig(`127.0.0.1:${String(port)}`);

  await test.step("a squatter poisons every name it can guess", async () => {
    const squatter = createServer((request, response) => {
      const worker = request.url === "/sw.js";
      response.setHeader("content-type", worker ? "text/javascript" : "text/html");
      response.setHeader("cache-control", "public, max-age=31536000, immutable");
      response.setHeader("no-vary-search", "params");
      response.end(worker ? WORKER : "<!doctype html><title>Squatter</title>");
    });
    await new Promise<void>((resolve) => squatter.listen(port, "localhost", resolve));
    for (const host of ["localhost", "reef.localhost", "0123456789abcdef.reef.localhost"]) {
      const origin = `http://${host}:${String(port)}`;
      await page.goto(`${origin}/`);
      await page.evaluate(async () => {
        localStorage.setItem("reef.session", "squatted");
        await navigator.serviceWorker.register("/sw.js", { updateViaCache: "all" });
        await navigator.serviceWorker.ready;
      });
      await page.goto(`${origin}/`);
      await expect(page).toHaveTitle("Squatter");
    }
    squatter.closeAllConnections();
    await new Promise((resolve) => squatter.close(resolve));
  });

  await reef.start();
  const session = await reef.session();
  await page.goto(session.sessionUrl);
  await expect(page.getByRole("heading", { name: "Repositories" })).toBeVisible();
  const left = await page.evaluate(async () => ({
    workers: (await navigator.serviceWorker.getRegistrations()).length,
    session: localStorage.getItem("reef.session"),
  }));
  expect(left.workers).toBe(0);
  expect(left.session).not.toBe("squatted");
  for (const path of ["", "passkeys"]) {
    await page.goto(`${session.url}${path}`);
    await expect(page.locator("#root")).not.toBeEmpty();
  }
});
