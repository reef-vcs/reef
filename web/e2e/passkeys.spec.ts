// Verified human presence in a real browser against a real daemon (R3.43):
// a passkey added on the Passkeys page, pinned by a policy the test lands,
// signs off a change that then lands; a second, unpinned passkey's sign-off
// does not count; an authenticator that cannot verify the user is refused;
// and a passkey is removed with a touch. Chromium only: the passkeys come
// from CDP virtual authenticators, which only Chromium has.
import type { CDPSession, Page } from "@playwright/test";
import { expect, test, type Reef, type Workspace } from "./reef";

test.skip(
  ({ browserName }) => browserName !== "chromium",
  "passkeys need a virtual authenticator, which only Chromium's CDP WebAuthn domain gives Playwright",
);

const base = (identity: string) => `version = 1

[owners]
identities = ["${identity}"]

[landing]
checks = "proposal"
`;

test.use({ policy: { text: base } });

/** A virtual platform authenticator with user verification. */
class Authenticator {
  private constructor(
    private readonly cdp: CDPSession,
    readonly id: string,
  ) {}

  static async add(cdp: CDPSession): Promise<Authenticator> {
    const { authenticatorId } = await cdp.send("WebAuthn.addVirtualAuthenticator", {
      options: {
        protocol: "ctap2",
        transport: "internal",
        hasResidentKey: true,
        hasUserVerification: true,
        isUserVerified: true,
        automaticPresenceSimulation: true,
      },
    });
    return new Authenticator(cdp, authenticatorId);
  }

  async credentials(): Promise<number> {
    const { credentials } = await this.cdp.send("WebAuthn.getCredentials", {
      authenticatorId: this.id,
    });
    return credentials.length;
  }

  async setUserVerified(isUserVerified: boolean) {
    await this.cdp.send("WebAuthn.setUserVerified", { authenticatorId: this.id, isUserVerified });
  }

  async remove() {
    await this.cdp.send("WebAuthn.removeVirtualAuthenticator", { authenticatorId: this.id });
  }
}

/**
 * Runs `hook.run` in the test before each `navigator.credentials.create` the
 * page makes, then the real ceremony. Adding a second passkey asserts with
 * the first authenticator, then creates on another. Chrome offers each
 * ceremony to every authenticator present, and one without the asserted
 * passkey, or one holding the excluded passkey, fails the whole ceremony;
 * so the test swaps the authenticators between the two.
 */
async function beforeCreate(page: Page): Promise<{ run: () => Promise<void> }> {
  const hook = { run: () => Promise.resolve() };
  await page.exposeFunction("reefE2eBeforeCreate", () => hook.run());
  await page.addInitScript(() => {
    const credentials = navigator.credentials;
    const create = credentials.create.bind(credentials);
    credentials.create = async (options) => {
      const page = window as unknown as { reefE2eBeforeCreate: () => Promise<void> };
      await page.reefE2eBeforeCreate();
      return create(options);
    };
  });
  return hook;
}

async function webAuthn(page: Page): Promise<CDPSession> {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("WebAuthn.enable", { enableUI: false });
  return cdp;
}

/** Adds a passkey labelled `label` to the default identity on the Passkeys page. */
async function addPasskey(page: Page, identity: string, label: string) {
  const card = page.getByRole("region", { name: new RegExp(`^${identity}`) });
  await card.getByLabel("Label").fill(label);
  await card.getByRole("button", { name: "Add a passkey" }).click();
  await expect(card.getByRole("status")).toHaveText(new RegExp(`^Added “${label}”`));
  return card;
}

/** Signs off `ws`'s change from its smartlog panel; returns the panel. */
async function signOff(page: Page, reef: Reef, url: string, ws: Workspace) {
  await page.goto(`${url}repos/${reef.repo}/smartlog?change=${ws.change}`);
  const panel = page.locator(".signoff");
  await panel.getByRole("button", { name: "Sign off" }).click();
  return panel;
}

/** `reef evidence list` on `commit`, as JSON text. */
async function evidenceOn(reef: Reef, commit: string): Promise<string> {
  return JSON.stringify(
    await reef.reef(["evidence", "list", "--repo", reef.repo, "-r", commit, "--kind", "signoff"]),
  );
}

async function land(reef: Reef, commit: string): Promise<string> {
  await reef.reef(["propose", commit, "--repo", reef.repo, "-m", "ready"]);
  return JSON.stringify(await reef.reef(["land", commit, "--repo", reef.repo]));
}

test("a person signs off with a pinned passkey, and only pinned passkeys count", async ({
  page,
  reef,
}) => {
  const session = await reef.session();
  const { url } = session;
  expect(new URL(url).hostname, "a ceremony needs the localhost origin").toBe("localhost");
  const identity = reef.identity;
  const cdp = await webAuthn(page);
  const first = await Authenticator.add(cdp);
  const hook = await beforeCreate(page);
  await page.goto(session.sessionUrl);
  await expect.poll(() => new URL(page.url()).hash).toBe("");

  const snippet = await test.step("add a passkey and copy its snippet", async () => {
    await page.goto(`${url}passkeys`);
    const card = await addPasskey(page, identity, "Virtual Touch ID");
    expect(await first.credentials()).toBe(1);
    const text = (await card.locator("pre").textContent()) ?? "";
    expect(text).toMatch(new RegExp(`^\\[passkeys\\]\\n${identity} = \\["[0-9a-f]{64}"\\]\\n$`));
    return text;
  });

  await test.step("land a policy that pins it and needs a verified sign-off", async () => {
    const [ws] = reef.workspaces;
    if (!ws) throw new Error("setup made no workspace");
    const policy = `${base(identity)}\n${snippet}\n[[signoff]]\nname = "verified"\nidentities = ["${identity}"]\npresence = "user_verified"\n`;
    const commit = await reef.edit(ws, { "reef-policy.toml": policy });
    // A change to the policy needs the owners' sign-off, which the current policy asks no presence of.
    await reef.reef([
      "evidence",
      "add",
      "--repo",
      reef.repo,
      "-r",
      commit,
      "--kind",
      "signoff",
      "--result",
      "pass",
      "--actor",
      "human",
    ]);
    expect(await land(reef, commit)).toContain("landed");
  });

  await test.step("Sign off a change, which then lands", async () => {
    const ws = await reef.workspace("e2e sign-off");
    const commit = await reef.edit(ws, { "signed.txt": "signed off by a person\n" });
    const panel = await signOff(page, reef, url, ws);
    await expect(panel.getByRole("status")).toHaveText(
      "Signed off: user verified, passkey “Virtual Touch ID”.",
    );
    await expect(panel.getByRole("listitem")).toContainText([/user verified/]);
    expect(await evidenceOn(reef, commit)).toContain("user_verified");
    expect(await land(reef, commit)).toContain("landed");
  });

  const second = await test.step("a second passkey, added with the first's touch", async () => {
    let added: Authenticator | undefined;
    hook.run = async () => {
      await first.remove();
      added = await Authenticator.add(cdp);
    };
    await page.goto(`${url}passkeys`);
    await addPasskey(page, identity, "Second key");
    if (!added) throw new Error("the page made no passkey");
    expect(await added.credentials()).toBe(1);
    return added;
  });

  await test.step("the unpinned passkey's sign-off is verified but does not count", async () => {
    const ws = await reef.workspace("e2e unpinned");
    const commit = await reef.edit(ws, { "unpinned.txt": "signed with an unpinned passkey\n" });
    const panel = await signOff(page, reef, url, ws);
    await expect(panel.getByRole("status")).toHaveText(
      "Signed off: user verified, passkey “Second key”.",
    );
    expect(await evidenceOn(reef, commit)).toContain("user_verified");
    await expect(
      reef.reef(["policy", "eval", "--repo", reef.repo, "-r", commit, "--require-allow"]),
      "a sign-off from a passkey the policy does not pin meets no presence",
    ).rejects.toThrow(/exit 5/);
  });

  await test.step("an authenticator that does not verify the user approves nothing", async () => {
    await second.setUserVerified(false);
    const ws = await reef.workspace("e2e unverified");
    const commit = await reef.edit(ws, { "unverified.txt": "nobody verified\n" });
    const panel = await signOff(page, reef, url, ws);
    await expect(panel.getByRole("alert")).toContainText("could not verify you");
    await expect(panel.getByRole("status")).toHaveCount(0);
    expect(await evidenceOn(reef, commit)).not.toContain("user_verified");
    await second.setUserVerified(true);
  });

  await test.step("remove the first passkey with the second's touch", async () => {
    await page.goto(`${url}passkeys`);
    const card = page.getByRole("region", { name: new RegExp(`^${identity}`) });
    await card.getByRole("button", { name: "Remove Virtual Touch ID" }).click();
    await expect(card.getByRole("status")).toHaveText(/^Removed “Virtual Touch ID”/);
    await expect(card.getByRole("row")).toHaveCount(2);
    await expect(card.getByRole("cell", { name: "Second key", exact: true })).toBeVisible();
  });
});
