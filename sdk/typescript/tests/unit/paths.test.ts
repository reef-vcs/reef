// Socket discovery, against the table reef-client and the Python SDK pass too (tests 1 and 4).

import { mkdirSync, mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { dirname, join, resolve } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { ConfigError } from "../../src/errors.js";
import {
  platformDefaults,
  settle,
  settleStateDir,
  type PlatformDefaults,
} from "../../src/paths.js";

const FIXTURES = resolve(import.meta.dirname, "..", "..", "..", "fixtures");

interface Case {
  name: string;
  env?: Record<string, string>;
  args?: { socket?: string; state_dir?: string; config?: string };
  files?: Record<string, string>;
  defaults?: { state_dir: string; runtime_dir?: string } | null;
  socket?: string;
  state_dir?: string;
  config_file?: string;
  error?: string;
}

const CASES = (
  JSON.parse(readFileSync(join(FIXTURES, "socket-paths.json"), "utf8")) as { cases: Case[] }
).cases;

let tmp: string;
beforeEach(() => {
  // A short scratch directory: a socket path has at most 103 bytes.
  tmp = mkdtempSync(join(tmpdir(), "reef-"));
});
afterEach(() => {
  rmSync(tmp, { recursive: true, force: true });
});

function fill(text: string): string {
  return text.replaceAll("{tmp}", tmp).replaceAll("{cwd}", process.cwd());
}

describe("socket paths follow the shared table", () => {
  it.each(CASES.map((c) => [c.name, c] as const))("%s", (_name, c) => {
    for (const [path, text] of Object.entries(c.files ?? {})) {
      const target = fill(path);
      mkdirSync(dirname(target), { recursive: true });
      writeFileSync(target, fill(text));
    }
    const env = Object.fromEntries(Object.entries(c.env ?? {}).map(([k, v]) => [k, fill(v)]));
    const args = c.args ?? {};
    const defaults = (): PlatformDefaults => {
      if (c.defaults === null || c.defaults === undefined) {
        throw new ConfigError("cannot find the home directory");
      }
      return {
        stateDir: fill(c.defaults.state_dir),
        runtimeDir: c.defaults.runtime_dir ? fill(c.defaults.runtime_dir) : undefined,
      };
    };
    const run = () =>
      settle({
        socket: args.socket === undefined ? undefined : fill(args.socket),
        stateDir: args.state_dir === undefined ? undefined : fill(args.state_dir),
        config: args.config === undefined ? undefined : fill(args.config),
        env,
        defaults,
      });
    if (c.error !== undefined) {
      let thrown: unknown;
      try {
        run();
      } catch (error) {
        thrown = error;
      }
      expect(thrown).toBeInstanceOf(ConfigError);
      expect((thrown as ConfigError).kind).toBe(c.error);
      return;
    }
    const settled = run();
    expect(settled.socket).toBe(fill(c.socket ?? ""));
    if (c.state_dir !== undefined) {
      expect(settled.stateDir).toBe(fill(c.state_dir));
    }
    if (c.config_file !== undefined) {
      expect(settled.configFile).toBe(fill(c.config_file));
    }
  });
});

describe("platform defaults", () => {
  it("follows the platform and XDG", () => {
    const env: Record<string, string> = {
      XDG_STATE_HOME: "/x/state",
      XDG_RUNTIME_DIR: "/run/user/7",
    };
    expect(platformDefaults((n) => env[n], "/home/u", "linux")).toEqual({
      stateDir: "/x/state/reef",
      runtimeDir: "/run/user/7",
    });
    // etcetera ignores relative XDG variables, as the daemon does.
    const relative: Record<string, string> = { XDG_STATE_HOME: "state", XDG_RUNTIME_DIR: "" };
    expect(platformDefaults((n) => relative[n], "/home/u", "linux")).toEqual({
      stateDir: "/home/u/.local/state/reef",
      runtimeDir: undefined,
    });
    expect(platformDefaults((n) => env[n], "/Users/u", "darwin")).toEqual({
      stateDir: "/Users/u/Library/Application Support/reef",
    });
  });

  it("needs a home", () => {
    expect(() => platformDefaults(() => undefined, "", "linux")).toThrow(ConfigError);
  });

  it("reads the real environment", () => {
    const saved = { ...process.env };
    try {
      process.env["REEF_STATE_DIR"] = join(tmp, "with space");
      delete process.env["REEF_SOCKET"];
      delete process.env["REEF_CONFIG"];
      expect(settle().socket).toBe(join(tmp, "with space", "daemon.sock"));
      expect(settleStateDir()).toBe(join(tmp, "with space"));
    } finally {
      process.env = saved;
    }
  });
});
