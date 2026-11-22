// The triage inbox in a real browser against a real daemon (R3.33): with the
// home page open, an agent asks for help and the request arrives in "Needs
// you now" behind a count, without a reload; the person answers it in place
// and the agent's session takes the answer as its next instruction (the
// Phase 3 exit criterion); a proposal under a sign-off rule arrives the same
// way once the daemon judges it; a repository registered while the page is
// open is followed too (#372: the daemon owns the inbox's triggers); a
// landing shows in the digest, which is marked seen once on screen, so a
// reload says nothing new has landed.
import type { Page } from "@playwright/test";
import { expect, test, type Reef } from "./reef";

const AGENT = "claude@e2e";
const SESSION = "e2e-agent-session";

const base = (identity: string) => `version = 1

[owners]
identities = ["${identity}"]

[landing]
checks = "proposal"
`;

/** A person's sign-off on every change; the automatic service judges it. */
const reviewed = (identity: string) => `version = 1

[owners]
identities = ["${identity}"]

[[signoff]]
name = "review"
identities = ["${identity}"]
`;

test.use({ policy: { text: base } });

const needs = (page: Page) => page.getByRole("region", { name: "Needs you now" });

/** Lands a commit of `reviewed`, signed off from the CLI as the owners need. */
async function landPolicy(reef: Reef) {
  const [ws] = reef.workspaces;
  if (!ws) throw new Error("setup made no workspace");
  await reef.edit(ws, { "reef-policy.toml": reviewed(reef.identity) });
  await reef.reef(["describe", "--workspace", ws.id, "-m", "Ask for a review"]);
  const described = ((await reef.reef(["ws", "snapshot", ws.id])) as { commit_id: string })
    .commit_id;
  await reef.reef([
    "evidence",
    "add",
    "--repo",
    reef.repo,
    "-r",
    described,
    "--kind",
    "signoff",
    "--result",
    "pass",
    "--actor",
    "human",
  ]);
  await reef.reef(["propose", described, "--repo", reef.repo, "-m", "ready"]);
  expect(JSON.stringify(await reef.reef(["land", described, "--repo", reef.repo]))).toContain(
    "landed",
  );
}

test("the inbox: a help request answered in place, a sign-off asked for, a digest seen", async ({
  page,
  reef,
}) => {
  const session = await reef.session();
  await page.goto(session.sessionUrl);
  await expect.poll(() => new URL(page.url()).hash).toBe("");
  await expect(needs(page).getByText("Nothing needs you now.")).toBeVisible();
  await expect(page).toHaveTitle("Inbox — Reef");

  const asking = reef.workspaces[1];
  if (!asking) throw new Error("setup made two workspaces");
  await test.step("an agent asks for help; it arrives behind a count", async () => {
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
        "TOML or YAML?",
        "--reason",
        "decision",
        "--client-session",
        SESSION,
      ],
      { agent: AGENT },
    );
    await needs(page).getByRole("button", { name: "Show 1 new item" }).click();
    await expect(
      needs(page).getByRole("link", { name: /asks for help with a decision/ }),
    ).toBeVisible();
    await expect(needs(page).getByText("TOML or YAML?")).toBeVisible();
    await expect(needs(page).getByText("Tried: read docs/config.md")).toBeVisible();
    await expect(page).toHaveTitle("(1) Inbox — Reef");
  });

  await test.step("the person answers in place; the agent's session takes it", async () => {
    await needs(page).getByRole("button", { name: "Answer" }).click();
    await needs(page).getByLabel("Your answer").fill("TOML, as the other services do.");
    await needs(page).getByRole("button", { name: "Answer" }).click();
    await expect(needs(page).getByRole("status").last()).toHaveText(
      "Answer sent: the agent gets it as its next instruction.",
    );
    await expect(needs(page).getByText(/Answered/)).toBeVisible();
    const next = JSON.stringify(
      await reef.reef(
        ["comment", "next", "--workspace", asking.id, "--client-session", SESSION, "--wait", "10s"],
        { agent: AGENT },
      ),
    );
    expect(next, "the answer resumes the session that asked").toContain(
      "TOML, as the other services do.",
    );
  });

  await test.step("a proposal under a sign-off rule arrives once the daemon judges it", async () => {
    await landPolicy(reef);
    const ws = await reef.workspace("e2e reviewed change");
    await reef.edit(ws, { "lib.rs": "pub fn answer() -> u32 {\n    42\n}\n" });
    await reef.reef(["describe", "--workspace", ws.id, "-m", "Add the answer"], { agent: AGENT });
    const commit = ((await reef.reef(["ws", "snapshot", ws.id])) as { commit_id: string })
      .commit_id;
    await reef.reef(["propose", commit, "--repo", reef.repo, "-m", "ready"], { agent: AGENT });
    await needs(page)
      .getByRole("button", { name: /^Show 1 new item/ })
      .click({ timeout: 30_000 });
    await expect(needs(page).getByRole("link", { name: "Add the answer" })).toBeVisible();
    await expect(needs(page).getByText(`Needs a sign-off from ${reef.identity}`)).toBeVisible();
  });

  await test.step("a repository registered while the page is open is followed", async () => {
    const other = await reef.registerAnother("e2e-other");
    const ws = await reef.workspace("e2e other repository", other);
    await reef.edit(ws, { "notes.txt": "which branch?\n" });
    await reef.reef(
      [
        "ask",
        "--workspace",
        ws.id,
        "-m",
        "Which branch should this go to?",
        "--tried",
        "read the README",
        "--reason",
        "unclear",
        "--client-session",
        "e2e-other-session",
      ],
      { agent: AGENT },
    );
    await needs(page)
      .getByRole("button", { name: /^Show 1 new item/ })
      .click({ timeout: 30_000 });
    await expect(needs(page).getByText("Which branch should this go to?")).toBeVisible();
  });

  await test.step("the landing shows in the digest, seen once on screen", async () => {
    const landed = page.getByRole("region", { name: "Landed since you last looked" });
    await expect(landed.getByText("Ask for a review")).toBeVisible();
    await expect(landed.locator("[data-seen]")).toHaveAttribute("data-seen", "yes", {
      timeout: 15_000,
    });
    await page.reload();
    await expect(landed.getByText("Nothing new has landed.")).toBeVisible();
    await expect(needs(page).getByRole("link", { name: "Add the answer" })).toBeVisible();
  });
});
