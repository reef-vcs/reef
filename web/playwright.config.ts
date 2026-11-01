// The browser smoke test (#128, `just web-e2e`): one scenario per browser,
// one at a time, each against its own scratch daemon (e2e/reef.ts). No
// retries: a pass on retry would hide the cross-tab races it exists to find.
import { defineConfig, devices } from "@playwright/test";

const ci = Boolean(process.env.CI);

export default defineConfig({
  testDir: "e2e",
  workers: 1,
  fullyParallel: false,
  retries: 0,
  forbidOnly: ci,
  timeout: 180_000,
  globalTimeout: ci ? 10 * 60_000 : 0,
  expect: { timeout: 10_000 },
  reporter: ci ? [["github"], ["list"], ["html", { open: "never" }]] : "list",
  use: { trace: "retain-on-failure", screenshot: "only-on-failure", video: "off" },
  projects: [
    { name: "chromium", use: { ...devices["Desktop Chrome"] } },
    { name: "firefox", use: { ...devices["Desktop Firefox"] } },
    { name: "webkit", use: { ...devices["Desktop Safari"] } },
  ],
});
