// @vitest-environment node
// R2.19 test 4: the copyable command runs nothing but `reef log`, whatever a
// link put in the revset.
import { execFileSync } from "node:child_process";
import { existsSync, mkdtempSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { describe, expect, it } from "vitest";
import { logCommand, SMARTLOG_REVSET } from "./revset";

/** The words a POSIX shell splits `command` into, run in `dir`. */
function words(command: string, dir: string): string[] {
  const out = execFileSync("/bin/sh", ["-c", `set -- ${command}; printf '%s\\0' "$@"`], {
    cwd: dir,
    encoding: "utf8",
  });
  return out.split("\0").slice(0, -1);
}

describe("logCommand", () => {
  const hostile = [
    SMARTLOG_REVSET,
    "description('it''s')",
    `description("x")`,
    "$(touch pwned)",
    "`touch pwned`",
    "a\nb; touch pwned",
    'bookmarks(exact:"föö-ブランチ")',
    "'; touch pwned; echo '",
  ];

  for (const revset of hostile) {
    it(`quotes ${JSON.stringify(revset)}`, () => {
      const dir = mkdtempSync(join(tmpdir(), "reef-quote-"));
      const command = logCommand({ repo: "a3dda866c7e8f343", revset, limit: 200 });
      expect(words(command, dir)).toEqual([
        "reef",
        "log",
        "--repo",
        "a3dda866c7e8f343",
        "-r",
        revset,
        "--limit",
        "200",
      ]);
      expect(existsSync(join(dir, "pwned"))).toBe(false);
    });
  }

  it("reads at an operation with --at-op", () => {
    expect(logCommand({ repo: "r", revset: "@", limit: 5, at: "abcd1234" })).toBe(
      "reef log --repo r -r @ --limit 5 --at-op abcd1234",
    );
  });
});
