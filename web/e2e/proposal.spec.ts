// Reviewing a proposal in a real browser against a real daemon (R3.31): an
// agent proposes a stack of two changes and one more; the person opens the
// Proposals tab, opens the stack's head, reads its intent, its evidence and
// its definitions, expands one to its lines, writes a note and an
// instruction it confirms, declines the other proposal with a reason, and
// accepts the head with one passkey touch, after which the stack lands.
// Accept needs a virtual authenticator, which only Chromium's CDP gives
// Playwright, so WebKit and Firefox run every step but it.
import type { CDPSession, Page } from "@playwright/test";
import { expect, test, type Reef, type Workspace } from "./reef";

const AGENT = "claude@e2e";

const base = (identity: string) => `version = 1

[owners]
identities = ["${identity}"]

[landing]
checks = "proposal"
`;

/** The policy the review runs under: a person's verified sign-off, with the pins in `pins`. */
const reviewed = (identity: string, pins: string) => `${base(identity)}
[[signoff]]
name = "review"
identities = ["${identity}"]
presence = "user_verified"
${pins ? `\n${pins}` : ""}`;

test.use({ policy: { text: base } });

async function webAuthn(page: Page): Promise<CDPSession> {
  const cdp = await page.context().newCDPSession(page);
  await cdp.send("WebAuthn.enable", { enableUI: false });
  await cdp.send("WebAuthn.addVirtualAuthenticator", {
    options: {
      protocol: "ctap2",
      transport: "internal",
      hasResidentKey: true,
      hasUserVerification: true,
      isUserVerified: true,
      automaticPresenceSimulation: true,
    },
  });
  return cdp;
}

async function land(reef: Reef, commit: string) {
  await reef.reef(["propose", commit, "--repo", reef.repo, "-m", "ready"]);
  expect(JSON.stringify(await reef.reef(["land", commit, "--repo", reef.repo]))).toContain(
    "landed",
  );
}

/** Adds a passkey on the Passkeys page; the `[passkeys]` lines that pin it. */
async function addPasskey(page: Page, reef: Reef, url: string): Promise<string> {
  await page.goto(`${url}passkeys`);
  const card = page.getByRole("region", { name: new RegExp(`^${reef.identity}`) });
  await card.getByLabel("Label").fill("Virtual Touch ID");
  await card.getByRole("button", { name: "Add a passkey" }).click();
  await expect(card.getByRole("status")).toHaveText(/^Added “Virtual Touch ID”/);
  await card.getByRole("button", { name: "Confirm a passkey to pin" }).click();
  return (await card.locator("pre").textContent()) ?? "";
}

/** Lands the policy the review runs under. */
async function landPolicy(reef: Reef, pins: string) {
  const [ws] = reef.workspaces;
  if (!ws) throw new Error("setup made no workspace");
  const commit = await reef.edit(ws, { "reef-policy.toml": reviewed(reef.identity, pins) });
  // The policy before this one asks the owners for no presence: a sign-off
  // from the CLI changes it.
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
  await land(reef, commit);
}

/** A new workspace on `commit`, released at the end. */
async function workspaceOn(reef: Reef, commit: string, purpose: string): Promise<Workspace> {
  const ws = (await reef.reef([
    "ws",
    "create",
    "--repo",
    reef.repo,
    "--new-on",
    commit,
    "--purpose",
    purpose,
  ])) as { id: string; change_id_reverse_hex: string; path: string };
  const workspace = { id: ws.id, change: ws.change_id_reverse_hex, path: ws.path };
  reef.workspaces.push(workspace);
  return workspace;
}

/** Describes `ws`'s change as the agent; its commit after. */
async function describe(reef: Reef, ws: Workspace, text: string): Promise<string> {
  await reef.reef(["describe", "--workspace", ws.id, "-m", text], { agent: AGENT });
  return ((await reef.reef(["ws", "snapshot", ws.id])) as { commit_id: string }).commit_id;
}

/** `reef proposal show` on `commit`, as JSON text. */
async function shown(reef: Reef, commit: string): Promise<string> {
  return JSON.stringify(await reef.reef(["proposal", "show", commit, "--repo", reef.repo]));
}

test("a person reviews a proposal: intent, evidence, definitions, comments, decline, accept", async ({
  page,
  reef,
  browserName,
}) => {
  const chromium = browserName === "chromium";
  const session = await reef.session();
  const { url } = session;
  if (chromium) await webAuthn(page);
  await page.goto(session.sessionUrl);
  await expect.poll(() => new URL(page.url()).hash).toBe("");
  await test.step("land a policy that needs a person's verified sign-off", async () => {
    const pins = chromium ? await addPasskey(page, reef, url) : "";
    await landPolicy(reef, pins);
  });

  const { head, other } = await test.step("an agent proposes a stack and one more", async () => {
    const bottomWs = await reef.workspace("e2e stack bottom");
    await reef.edit(bottomWs, { "lib.rs": "pub fn lease_length() -> u32 {\n    30\n}\n" });
    const bottom = await describe(reef, bottomWs, "Add the lease length");
    const headWs = await workspaceOn(reef, bottom, "e2e stack head");
    await reef.edit(headWs, {
      "lib.rs":
        "pub fn lease_length() -> u32 {\n    30\n}\n\npub fn renew(at: u32) -> u32 {\n    at + lease_length()\n}\n",
    });
    const headCommit = await describe(
      reef,
      headWs,
      "Renew a lease\n\nA lease renews by its length from now.",
    );
    await reef.reef(["propose", headCommit, "--repo", reef.repo, "-m", "ready"], {
      agent: AGENT,
    });
    const otherWs = await reef.workspace("e2e other");
    await reef.edit(otherWs, { "notes.txt": "an unrelated change\n" });
    await describe(reef, otherWs, "Rewrite the notes");
    const otherCommit = ((await reef.reef(["ws", "snapshot", otherWs.id])) as { commit_id: string })
      .commit_id;
    await reef.reef(["propose", otherCommit, "--repo", reef.repo, "-m", "ready"], {
      agent: AGENT,
    });
    return { head: headCommit, other: otherCommit };
  });

  await test.step("open the head from the Proposals tab", async () => {
    await page.goto(`${url}repos/${reef.repo}/smartlog`);
    await page
      .getByRole("navigation", { name: "Repository" })
      .getByRole("link", { name: "Proposals" })
      .click();
    const list = page.getByRole("list", { name: "Open proposals" });
    await expect(list.getByRole("link")).toHaveCount(3);
    await list.getByRole("link", { name: "Renew a lease" }).click();
    await expect(page.getByRole("heading", { level: 2 })).toContainText("Renew a lease");
  });

  await test.step("read the intent, the evidence and the definitions", async () => {
    await expect(page.getByText("A lease renews by its length from now.")).toBeVisible();
    await expect(page.getByRole("link", { name: "Add the lease length" })).toBeVisible();
    await expect(page.getByText("The policy blocks it.")).toBeVisible();
    await expect(page.getByText(/sign-offs 0 of 1, user verified/)).toBeVisible();
    const file = page.getByRole("region", { name: "lib.rs" });
    await expect(file.getByText("renew", { exact: true })).toBeVisible();
    await expect(file.getByText("added", { exact: true })).toBeVisible();
  });

  await test.step("expand a definition to its lines", async () => {
    const file = page.getByRole("region", { name: "lib.rs" });
    await file.getByRole("button", { name: "Show lines" }).first().click();
    await expect(file.getByText("at + lease_length()")).toBeVisible();
  });

  await test.step("write a note, and an instruction confirmed after its estimate", async () => {
    await page.getByRole("textbox", { name: "Comment" }).fill("The renewal reads right.");
    await page.getByRole("button", { name: "Comment", exact: true }).click();
    await expect(page.getByRole("status")).toHaveText("Comment added.");
    await expect(page.getByText("The renewal reads right.")).toBeVisible();
    await page.getByRole("textbox", { name: "Comment" }).fill("Add a test for renew.");
    await page.getByRole("checkbox", { name: /Send as an instruction/ }).check();
    await page.getByRole("button", { name: "Send the instruction" }).click();
    const confirm = page.getByRole("group", { name: "Confirm the instruction" });
    await confirm.getByRole("button", { name: "Confirm" }).click();
    await expect(page.getByRole("status")).toHaveText(
      "Instruction confirmed: the agent gets it next.",
    );
    const listed = JSON.stringify(await reef.reef(["comment", "list", head, "--repo", reef.repo]));
    expect(listed).toContain("Add a test for renew.");
    expect(listed, "the page's comments name its web UI session").toContain("web_session");
  });

  await test.step("decline the other proposal with a reason", async () => {
    await page.getByRole("link", { name: "Proposals", exact: true }).first().click();
    await page
      .getByRole("list", { name: "Open proposals" })
      .getByRole("link", { name: "Rewrite the notes" })
      .click();
    await page.getByRole("button", { name: "Decline" }).click();
    await page.getByLabel(/^Why/).fill("Out of scope for this release.");
    await page.getByRole("button", { name: "Decline" }).last().click();
    await expect(page.getByRole("status")).toHaveText("Declined.");
    const record = await shown(reef, other);
    expect(record).toContain("declined");
    expect(record).toContain("Out of scope for this release.");
    expect(record, "the decline names the page's web UI session").toContain("web_session");
  });

  if (!chromium) return;
  await test.step("accept the head with one touch; the stack lands", async () => {
    await page.goto(`${url}repos/${reef.repo}/proposals`);
    await page
      .getByRole("list", { name: "Open proposals" })
      .getByRole("link", { name: "Renew a lease" })
      .click();
    await page.getByRole("button", { name: /^Accept/ }).click();
    await expect(page.getByRole("status")).toHaveText(/^Signed off and (queued to land|landed)/);
    const evidence = JSON.stringify(
      await reef.reef(["evidence", "list", "--repo", reef.repo, "-r", head, "--kind", "signoff"]),
    );
    expect(evidence).toContain("user_verified");
    await expect.poll(() => shown(reef, head), { timeout: 30_000 }).toContain('"landed"');
  });
});
