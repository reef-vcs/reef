// Reef on a phone (R3.40), at 375 by 812 against a real daemon. The first
// test is the layout, through the local listener: nothing scrolls sideways,
// no field makes iOS zoom, the inbox is in reach, and a link to a help
// request opens its answer. The second is a person's phone: paired by the
// code `reef remote pair` prints, at the remote listener's address, it shows
// the inbox, a proposal and its diff, writes nothing, and ends when told.
import type { Browser, Page } from "@playwright/test";
import { FreezingProxy } from "./freeze";
import { expect, freePort, test, type Reef } from "./reef";

const AGENT = "claude@e2e";
const LONG = "a_very_long_name_that_no_phone_is_wide_enough_for_".repeat(4);

const reviewed = (identity: string) => `version = 1

[owners]
identities = ["${identity}"]

[[signoff]]
name = "review"
identities = ["${identity}"]
`;

test.use({ viewport: { width: 375, height: 812 }, policy: { text: reviewed } });

const needs = (page: Page) => page.getByRole("region", { name: "Needs you now" });

/** A help request and a proposal that needs a sign-off, both awkwardly wide. */
async function needYou(reef: Reef) {
  const [asking, proposing] = reef.workspaces;
  if (!asking || !proposing) throw new Error("setup made two workspaces");
  await reef.edit(asking, { "config.txt": "format = ?\n" });
  await reef.reef(["describe", "--workspace", asking.id, "-m", "Pick a config format"], {
    agent: AGENT,
  });
  await reef.reef(
    [
      "ask",
      "--workspace",
      asking.id,
      "-m",
      "The task does not say which config format to use.",
      "--tried",
      "read docs/config.md",
      "--question",
      `TOML or YAML for https://config.example/${LONG}?`,
      "--reason",
      "decision",
    ],
    { agent: AGENT },
  );
  await reef.edit(proposing, { "lib.rs": `pub fn ${LONG}() -> u32 {\n    42 // ${LONG}\n}\n` });
  await reef.reef(
    ["describe", "--workspace", proposing.id, "-m", `Add ${LONG}\n\nThe body says ${LONG}.`],
    { agent: AGENT },
  );
  const commit = ((await reef.reef(["ws", "snapshot", proposing.id])) as { commit_id: string })
    .commit_id;
  await reef.reef(["propose", commit, "--repo", reef.repo, "-m", "ready"], { agent: AGENT });
  // Judged before a page opens, which then holds neither behind a count.
  await expect
    .poll(
      async () => {
        const inbox = (await reef.reef(["inbox", "--repo", reef.repo], { quiet: true })) as {
          needs_you_now?: unknown[];
        };
        return inbox.needs_you_now?.length ?? 0;
      },
      { timeout: 30_000 },
    )
    .toBe(2);
}

async function noSidewaysScroll(page: Page, where: string) {
  const widths = await page.evaluate(() => {
    const y = window.scrollY;
    window.scrollTo(1_000, y);
    const scrolled = window.scrollX;
    window.scrollTo(0, y);
    const page = document.documentElement;
    return { scrolled, content: page.scrollWidth, view: page.clientWidth };
  });
  expect(widths.scrolled, where).toBe(0);
  expect(widths.content, where).toBeLessThanOrEqual(widths.view);
}

/** What iOS would zoom into on focus. */
async function smallFields(page: Page): Promise<string[]> {
  return page
    .locator("input:visible, textarea:visible")
    .evaluateAll((fields) =>
      fields
        .filter((field) => parseFloat(getComputedStyle(field).fontSize) < 16)
        .map((field) => field.outerHTML),
    );
}

async function rowsAreTappable(page: Page) {
  const rows = needs(page).locator("li[data-key]");
  await expect(rows).toHaveCount(2);
  for (const link of await rows.locator("a.name-link").all()) {
    const box = await link.boundingBox();
    expect(box?.height, "a row's link").toBeGreaterThanOrEqual(44);
    expect(box?.width, "a row's link").toBeGreaterThan(300);
  }
}

/** Pairs `page` at the daemon's remote address. */
async function pairBrowser(page: Page, reef: Reef) {
  const { code } = (await reef.reef(["remote", "pair", "iphone"])) as { code: string };
  await page.goto("/");
  await page.getByRole("button", { name: "Pair this browser instead" }).click();
  await page.getByLabel("Pairing code").fill(code);
  await page.getByRole("button", { name: "Pair", exact: true }).click();
}

/** Opens the proposal from its inbox row. */
async function openProposal(page: Page) {
  await needs(page)
    .getByRole("link", { name: /^Add a_very_long/ })
    .click();
  await expect(page.getByRole("heading", { name: "Evidence" })).toBeVisible();
  await expect(page.locator(".diff-file").first()).toBeVisible();
}

/** The proposal's first file, its lines shown. */
async function openDiff(page: Page) {
  const file = page.locator(".diff-file").first();
  await file
    .getByRole("button", { name: /^Show / })
    .first()
    .click();
  const hunk = file.locator(".hunk").first();
  await expect(hunk).toBeVisible();
  return hunk;
}

test("at a phone's width the pages fit", async ({ page, reef }) => {
  await needYou(reef);
  const session = await reef.session();
  await page.goto(session.sessionUrl);

  await test.step("the home page is the inbox, with the rest folded", async () => {
    await rowsAreTappable(page);
    await noSidewaysScroll(page, "the home page");
    const watching = page.getByRole("region", { name: "Watching" });
    await expect(watching.getByText("No proposals or instructions in flight.")).toBeHidden();
    await watching.getByRole("heading").click();
    await expect(watching.getByText("No proposals or instructions in flight.")).toBeVisible();
    const last = page.getByRole("heading", { name: "Repositories" });
    const above = await page.locator(".inbox").boundingBox();
    const heading = await last.boundingBox();
    expect((heading?.y ?? 0) - ((above?.y ?? 0) + (above?.height ?? 0))).toBeGreaterThan(8);
  });

  await test.step("a repository's tabs have the inbox first and in view", async () => {
    for (const path of ["activity", "workspaces", "smartlog"]) {
      await page.goto(`${session.url}repos/${reef.repo}/${path}`);
      await expect(page.getByRole("slider")).toBeVisible();
      expect(await smallFields(page), path).toEqual([]);
      await noSidewaysScroll(page, path);
    }
    const tab = page
      .getByRole("navigation", { name: "Repository" })
      .getByRole("link", { name: "Inbox 2" });
    await expect(tab).toBeInViewport({ ratio: 1 });
    await expect(page.locator(".appbar").getByRole("link", { name: "e2e-fixture" })).toBeVisible();
    await tab.click();
    await rowsAreTappable(page);
    await noSidewaysScroll(page, "a repository's inbox");
    await expect(page.getByRole("slider")).toHaveCount(0);
  });

  await test.step("a proposal fits, and its diff scrolls inside its card", async () => {
    await openProposal(page);
    await expect(page.getByRole("slider")).toHaveCount(0);
    const hunk = await openDiff(page);
    expect(await hunk.evaluate((box) => box.scrollWidth - box.clientWidth)).toBeGreaterThan(100);
    await noSidewaysScroll(page, "a proposal");
    expect(await smallFields(page)).toEqual([]);
    const accept = page.getByRole("button", { name: "Accept" });
    await expect(accept).toBeDisabled();
    const why = (await accept.getAttribute("aria-describedby")) ?? "";
    await expect(page.locator(`#${why}`)).toContainText(/passkey/);
    await page.evaluate(() => {
      window.scrollTo(0, 0);
    });
    await expect(accept).toBeInViewport({ ratio: 1 });
    const looks = await accept.evaluate((button) => {
      const probe = document.createElement("button");
      probe.className = "primary";
      button.after(probe);
      const enabled = getComputedStyle(probe).backgroundColor;
      probe.remove();
      return { enabled, disabled: getComputedStyle(button).backgroundColor };
    });
    expect(looks.disabled, "a disabled Accept has no brand fill").not.toBe(looks.enabled);
  });

  await test.step("a link to the help request opens its answer", async () => {
    await page.goto(`${session.url}repos/${reef.repo}/inbox`);
    const row = needs(page).locator("li[data-key^='help:']");
    const key = (await row.getAttribute("data-key")) ?? "";
    await page.goto(`${session.url}repos/${reef.repo}/items/${encodeURIComponent(key)}`);
    const answer = row.getByLabel("Your answer");
    await expect(answer).toBeInViewport();
    await expect(page).toHaveURL(/\/inbox\?item=help/);
    await expect(row.getByText(/^TOML or YAML/)).toBeInViewport();
    await noSidewaysScroll(page, "a help answer");
    expect(await smallFields(page)).toEqual([]);
    await page.goto(`${session.url}repos/${reef.repo}/items/help%3Agone`);
    await expect(page.getByText("That no longer needs you.")).toBeVisible();
  });
});

test("a paired phone reads, and ends when told", async ({ browser, reef }) => {
  await needYou(reef);
  const origin = await reef.serveRemote();
  // A phone with the certificate installed: the generated one is not trusted.
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 375, height: 812 },
    baseURL: origin,
  });
  const page = await context.newPage();
  const errors: string[] = [];
  page.on("pageerror", (error) => errors.push(String(error)));
  try {
    await test.step("the code from the computer pairs it, once", async () => {
      const printed = (await reef.reef(["remote", "pair", "iphone"])) as {
        url: string;
        code: string;
      };
      expect(printed.url).toBe(`${origin}/`);
      await page.goto(`/repos/${reef.repo}/inbox`);
      await expect(page.getByText("Add to Home Screen")).toBeVisible();
      await page.getByRole("button", { name: "Pair this browser instead" }).click();
      expect(await smallFields(page)).toEqual([]);
      await noSidewaysScroll(page, "the pairing form");
      await page.getByLabel("Pairing code").fill("0000-0000");
      await page.getByRole("button", { name: "Pair", exact: true }).click();
      await expect(page.getByRole("alert")).toContainText("not valid");
      await page.getByLabel("Pairing code").fill(printed.code.toLowerCase());
      await page.getByRole("button", { name: "Pair", exact: true }).click();
      // The page it was opened at, now with the daemon's inbox.
      await rowsAreTappable(page);
      expect(new URL(page.url()).pathname).toBe(`/repos/${reef.repo}/inbox`);
      await page.reload();
      await rowsAreTappable(page);
    });

    await test.step("what it loads comes over HTTP/2, which its streams share", async () => {
      const protocols = await page.evaluate(() =>
        performance
          .getEntriesByType("resource")
          .map((entry) => (entry as PerformanceResourceTiming).nextHopProtocol),
      );
      expect(new Set(protocols.filter(Boolean))).toEqual(new Set(["h2"]));
    });

    await test.step("it shows a help request and a proposal, and offers no write", async () => {
      const help = needs(page).locator("li[data-key^='help:']");
      await expect(help.getByText("A paired phone only reads.")).toBeVisible();
      await expect(help.getByRole("button", { name: "Answer" })).toHaveCount(0);
      await openProposal(page);
      await openDiff(page);
      await expect(page).toHaveURL(new RegExp(`/repos/${reef.repo}/proposals/`));
      for (const name of ["Accept", "Decline"]) {
        await expect(page.getByRole("button", { name })).toBeDisabled();
      }
      await expect(page.locator(".decide-why")).toContainText("A paired phone only reads.");
      await expect(page.locator("textarea")).toHaveCount(0);
      await noSidewaysScroll(page, "a proposal on a paired phone");
    });

    await test.step("its token writes nothing, whatever page asks", async () => {
      const refused = await page.evaluate(async () => {
        const token = localStorage.getItem("reef.session") ?? "";
        const call = async (path: string) => {
          const response = await fetch(`/reef.api.v1.${path}`, {
            method: "POST",
            headers: {
              "content-type": "application/grpc-web+proto",
              authorization: `Bearer ${token}`,
            },
            body: new Uint8Array(5),
          });
          return response.headers.get("grpc-message") ?? "";
        };
        return {
          comment: await call("CommentService/CreateComment"),
          edit: await call("FileService/EditFiles"),
          pair: await call("RemoteService/StartPairing"),
        };
      });
      for (const [what, message] of Object.entries(refused)) {
        expect(decodeURIComponent(message), what).toContain("a device token cannot call");
      }
    });

    await test.step("a link's token does not take its place; unpairing ends every tab", async () => {
      const token = () => page.evaluate(() => localStorage.getItem("reef.session"));
      const own = await token();
      const other = await context.newPage();
      await other.goto("/");
      await rowsAreTappable(other);
      await page.goto("/#session=another&embedded=1");
      await rowsAreTappable(page);
      expect(await token()).toBe(own);
      expect(await page.evaluate(() => sessionStorage.length)).toBe(0);
      // The phone's other tab went on.
      await expect(other.locator(".connection")).toHaveText("Live");
      await page.getByRole("button", { name: "Settings" }).click();
      const phone = page.getByRole("region", { name: "This phone" });
      await phone.getByRole("button", { name: "Unpair this phone" }).click();
      await phone.getByRole("button", { name: "Unpair", exact: true }).click();
      for (const tab of [page, other]) {
        await expect(tab.getByRole("heading", { name: "Not connected" })).toBeVisible();
      }
      expect(await token()).toBeNull();
      await other.close();
      const again = (await reef.reef(["remote", "pair", "iphone"])) as { code: string };
      await page.getByRole("button", { name: "Pair this browser instead" }).click();
      await page.getByLabel("Pairing code").fill(again.code);
      await page.getByRole("button", { name: "Pair", exact: true }).click();
      await openProposal(page);
    });

    await test.step("a daemon with no origin for a while keeps the phone paired", async () => {
      const token = () => page.evaluate(() => localStorage.getItem("reef.session"));
      const own = await token();
      const without = { line: "" };
      await reef.reconfigure((text) =>
        text.replace(/^origin = .*$/m, (line) => {
          without.line = line;
          return "";
        }),
      );
      await page.goto("/");
      await expect(page.getByText(`This phone was paired at ${origin}.`)).toBeVisible();
      await expect(page.getByText("has no `[remote] origin` any more")).toBeVisible();
      await expect(page.getByRole("button", { name: "Pair this browser instead" })).toHaveCount(0);
      expect(await token()).toBe(own);
      await reef.reconfigure((text) => text.replace("[remote]\n", `[remote]\n${without.line}\n`));
      await page.getByRole("button", { name: "Try again" }).click();
      await rowsAreTappable(page);
      await openProposal(page);
    });

    await test.step("Settings names it; a revoke ends it", async () => {
      await page.getByRole("button", { name: "Settings" }).click();
      const phone = page.getByRole("region", { name: "This phone" });
      await expect(phone.getByText(/^Paired as iphone until .* It only reads\.$/)).toBeVisible();
      await noSidewaysScroll(page, "Settings");
      // Over the proposal's sticky action bar, not under it.
      await phone.getByRole("button", { name: "Unpair this phone" }).click();
      await expect(phone.getByText("reef remote token revoke iphone")).toBeVisible();
      await phone.getByRole("button", { name: "Keep it" }).click();
      await reef.reef(["remote", "token", "revoke", "iphone"]);
      await expect(page.getByRole("heading", { name: "Not connected" })).toBeVisible();
      expect(await page.evaluate(() => localStorage.length)).toBe(0);
    });

    await test.step("the app's manifest and icons load", async () => {
      for (const [path, type] of [
        ["/app.webmanifest", "application/manifest+json"],
        ["/apple-touch-icon.png", "image/png"],
      ]) {
        const response = await page.request.get(path ?? "");
        expect(response.status(), path).toBe(200);
        expect(response.headers()["content-type"]).toBe(type);
      }
      const manifest = (await (await page.request.get("/app.webmanifest")).json()) as {
        icons: { src: string }[];
      };
      for (const icon of manifest.icons) {
        expect((await page.request.get(icon.src)).status(), icon.src).toBe(200);
      }
    });
    expect(errors).toEqual([]);
  } finally {
    await context.close();
  }
});

const NOT_FIREFOX = "a phone's Home Screen app is WebKit's or Chromium's";

/** A paired phone, live, whose connections can die, and whose time flows as it does and can be moved on. */
async function withMortalConnection(
  browser: Browser,
  reef: Reef,
  run: (page: Page, proxy: FreezingProxy) => Promise<void>,
) {
  const proxy = new FreezingProxy();
  const through = await proxy.listen();
  const listen = await freePort();
  proxy.to(listen);
  const origin = await reef.serveRemote({ through, listen });
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 375, height: 812 },
    baseURL: origin,
  });
  try {
    const page = await context.newPage();
    await page.clock.install();
    await pairBrowser(page, reef);
    await expect(page.locator(".connection")).toHaveText("Live");
    await run(page, proxy);
  } finally {
    await context.close();
    proxy.close();
  }
}

/**
 * The phone's connections die, long enough ago that Chromium doubts one it
 * sends on: it asks the other side of a connection that is silent for 10 s.
 */
async function die(proxy: FreezingProxy) {
  proxy.freeze();
  await expect.poll(() => proxy.deadMs, { timeout: 20_000 }).toBeGreaterThan(11_000);
}

const shown = (page: Page) =>
  page.evaluate(() => document.dispatchEvent(new Event("visibilitychange")));

test("a phone whose connection died says so, and catches up", async ({
  browser,
  browserName,
  reef,
}) => {
  test.skip(browserName === "firefox", NOT_FIREFOX);
  await withMortalConnection(browser, reef, async (page, proxy) => {
    const connection = page.locator(".connection");
    await expect(needs(page).getByText("Nothing needs you now.")).toBeVisible();

    /** What each stream was asked for, as its requests go out. */
    const opens: string[] = [];
    page.on("request", (request) => {
      const stream = /(SubscribeOperations|SubscribePresence|WatchInbox)$/.exec(request.url());
      if (stream?.[1]) opens.push(stream[1]);
    });
    /** Time moves on until the page is live again: the browser gives the dead connection up. */
    const liveAgain = async () => {
      await expect
        .poll(
          async () => {
            await page.clock.fastForward(5_000);
            return connection.textContent();
          },
          { timeout: 60_000, intervals: [1_000] },
        )
        .toBe("Live");
    };

    await test.step("while frames arrive, nothing is opened again", async () => {
      // Past the first keepalives, and then past the 40 s of the opening.
      const keepalive = page.waitForFunction(
        (from) => performance.now() - from > 17_000,
        await page.evaluate(() => performance.now()),
        { timeout: 60_000 },
      );
      await keepalive;
      await page.clock.fastForward(15_000);
      for (const event of ["pageshow", "online"]) {
        await page.evaluate((name) => window.dispatchEvent(new Event(name)), event);
      }
      await shown(page);
      await page.clock.fastForward(10_000);
      await expect(connection).toHaveText("Live");
      expect(opens).toEqual([]);
    });

    await test.step("a dead connection is not live, and the inbox comes back on another", async () => {
      proxy.freeze();
      const [asking] = reef.workspaces;
      if (!asking) throw new Error("setup made no workspace");
      await reef.edit(asking, { "config.txt": "format = ?\n" });
      await reef.reef(["describe", "--workspace", asking.id, "-m", "Pick a format"], {
        agent: AGENT,
      });
      await reef.reef(
        ["ask", "--workspace", asking.id, "-m", "Two fit.", "--tried", "the docs"].concat([
          "--question",
          "TOML or YAML?",
          "--reason",
          "decision",
        ]),
        { agent: AGENT },
      );
      await expect(connection).toHaveText("Live");
      await page.clock.fastForward(45_000);
      await expect(connection).toHaveText("Reconnecting");
      await liveAgain();
      expect(new Set(opens)).toEqual(
        new Set(["SubscribeOperations", "SubscribePresence", "WatchInbox"]),
      );
      await needs(page).getByRole("button", { name: "Show 1 new item" }).click();
      await expect(needs(page).getByText("TOML or YAML?")).toBeVisible();
    });

    await test.step("shown again after it died, it is not live at once", async () => {
      proxy.freeze();
      await page.clock.fastForward(25_000);
      await expect(connection).toHaveText("Live");
      await shown(page);
      // At once, and not once 40 s have passed.
      await expect(connection).toHaveText("Reconnecting", { timeout: 2_000 });
    });
  });
});

test("a proposal changed while the phone slept is shown when it wakes", async ({
  browser,
  browserName,
  reef,
}) => {
  test.skip(browserName === "firefox", NOT_FIREFOX);
  await needYou(reef);
  const [, proposing] = reef.workspaces;
  if (!proposing) throw new Error("setup made two workspaces");
  await withMortalConnection(browser, reef, async (page, proxy) => {
    await openProposal(page);
    const changed = page.getByText("The agent changed this proposal since you opened it.");
    const change = async (file: string) => {
      await reef.edit(proposing, { [file]: `pub fn ${file.slice(0, -3)}() {}\n` });
      const { commit_id } = (await reef.reef(["ws", "snapshot", proposing.id])) as {
        commit_id: string;
      };
      await reef.reef(["propose", commit_id, "--repo", reef.repo, "-m", file], { agent: AGENT });
    };
    await expect(changed).toHaveCount(0);
    await die(proxy);
    // A read that hangs: the page reads an open proposal every 10 s.
    const hung = page.waitForRequest(/ListProposals$/);
    await page.clock.fastForward(11_000);
    await hung;
    // Asleep for an hour: no timer runs.
    await page.clock.pauseAt(await page.evaluate(() => Date.now() + 1_000));
    await change("more.rs");
    await page.clock.fastForward(60 * 60_000);
    await page.clock.resume();
    await shown(page);
    await expect(changed).toBeVisible({ timeout: 20_000 });
    await expect(page.locator(".connection")).toHaveText("Live");
    await expect(page.getByRole("alert")).toHaveCount(0);
    await page.getByRole("button", { name: "Review the new version" }).click();
    await expect(page.locator(".diff-file")).toHaveCount(2);
    await expect(changed).toHaveCount(0);
    // And the next change too: the hour is in nothing the page waits for.
    await change("again.rs");
    await expect(changed).toBeVisible({ timeout: 20_000 });
  });
});

/** Shown again on a connection that died, and the proposal's row tapped. */
async function tapDead(page: Page, proxy: FreezingProxy) {
  await rowsAreTappable(page);
  await die(proxy);
  await page.clock.fastForward(25_000);
  await shown(page);
  await expect(page.locator(".connection")).toHaveText("Reconnecting");
  await needs(page)
    .getByRole("link", { name: /^Add a_very_long/ })
    .click();
}

async function proposalIsShown(page: Page) {
  await expect(page.getByRole("heading", { name: "Evidence" })).toBeVisible({ timeout: 30_000 });
  await expect(page.locator(".diff-file").first()).toBeVisible();
  await expect(page.locator(".connection")).toHaveText("Live");
}

test("a row tapped on a dead connection opens its proposal with no second tap", async ({
  browser,
  browserName,
  reef,
}) => {
  test.skip(browserName === "firefox", NOT_FIREFOX);
  await needYou(reef);
  await withMortalConnection(browser, reef, async (page, proxy) => {
    await test.step("the first time", async () => {
      await tapDead(page, proxy);
      await proposalIsShown(page);
    });
    await test.step("again", async () => {
      await page.getByRole("link", { name: "Reef", exact: true }).click();
      await tapDead(page, proxy);
      await proposalIsShown(page);
    });
  });
});

test("an app whose script does not come says so, and its link opens the page", async ({
  browser,
  browserName,
  reef,
}) => {
  test.skip(browserName === "firefox", NOT_FIREFOX);
  await needYou(reef);
  const origin = await reef.serveRemote();
  const context = await browser.newContext({
    ignoreHTTPSErrors: true,
    viewport: { width: 375, height: 812 },
    baseURL: origin,
  });
  try {
    const page = await context.newPage();
    await pairBrowser(page, reef);
    await openProposal(page);
    await expect(page.getByText("Loading Reef.")).toHaveCount(0);
    for (const how of ["hangs", "fails"]) {
      await test.step(`one that ${how}`, async () => {
        const cold = await context.newPage();
        let asked = 0;
        await cold.route(/\/assets\/index-.*\.js$/, (route) => {
          if (asked++ > 0) return route.continue();
          return how === "hangs" ? undefined : route.abort();
        });
        await cold.goto(page.url(), { waitUntil: "commit" });
        await expect.poll(() => asked).toBe(1);
        await expect(cold.getByText("Loading Reef.")).toBeHidden();
        await expect(cold.getByText("Loading Reef.")).toBeVisible({ timeout: 15_000 });
        await noSidewaysScroll(cold, "an app whose script does not come");
        await cold.getByRole("link", { name: "Load it again" }).click();
        await proposalIsShown(cold);
        await expect(cold.getByText("Loading Reef.")).toHaveCount(0);
        await cold.close();
      });
    }
  } finally {
    await context.close();
  }
});
