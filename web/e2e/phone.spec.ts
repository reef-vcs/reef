// Reef on a phone (R3.40), at 375 by 812 against a real daemon. The first
// test is the layout, through the local listener: nothing scrolls sideways,
// no field makes iOS zoom, the inbox is in reach, and a link to a help
// request opens its answer. The second is a person's phone: paired by the
// code `reef remote pair` prints, at the remote listener's address, it shows
// the inbox, a proposal and its diff, writes nothing, and ends when told.
import type { Page } from "@playwright/test";
import { expect, test, type Reef } from "./reef";

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
      await page.goto("/#session=another");
      await rowsAreTappable(page);
      expect(await token()).toBe(own);
      const other = await context.newPage();
      await other.goto("/");
      await rowsAreTappable(other);
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
