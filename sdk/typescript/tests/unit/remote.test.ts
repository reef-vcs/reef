// Remote URLs against the shared table, and choosing a remote (tests 2 and 3).

import { mkdtempSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { afterEach, beforeEach, describe, expect, it } from "vitest";

import { connect } from "../../src/client.js";
import { ConfigError } from "../../src/errors.js";
import { choose, isFingerprint, RemoteUrl } from "../../src/remote.js";

const FIXTURES = resolve(import.meta.dirname, "..", "..", "..", "fixtures");

interface UrlCase {
  url: string;
  host?: string;
  port?: number;
  authority?: string;
  valid?: boolean;
}

const URLS = (
  JSON.parse(readFileSync(join(FIXTURES, "remote-urls.json"), "utf8")) as { cases: UrlCase[] }
).cases;

describe("remote urls follow the shared table", () => {
  it.each(URLS.map((c) => [c.url, c] as const))("%s", (_url, c) => {
    const parsed = RemoteUrl.parse(c.url);
    if (c.valid === false) {
      expect(parsed).toBeUndefined();
      return;
    }
    expect(parsed?.host).toBe(c.host);
    expect(parsed?.port).toBe(c.port);
    expect(parsed?.authority).toBe(c.authority);
  });
});

describe("remote target selection", () => {
  let state: string;
  beforeEach(() => {
    state = mkdtempSync(join(tmpdir(), "reef-"));
  });
  afterEach(() => {
    rmSync(state, { recursive: true, force: true });
  });

  const pin = "sha256:" + "a".repeat(64);
  const other = "sha256:" + "b".repeat(64);

  it("takes a URL with a token", () => {
    const target = choose("https://vm:7470", {
      stateDir: state,
      token: " t1 ",
      env: () => undefined,
    });
    expect(target).toMatchObject({ token: "t1", fingerprint: undefined, name: undefined });
    expect(target.url.origin).toBe("https://vm:7470");
  });

  it("needs a token for a URL", () => {
    let thrown: unknown;
    try {
      choose("https://vm:7470", { stateDir: state, env: () => undefined });
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(ConfigError);
    expect((thrown as ConfigError).hint).toContain("REEF_REMOTE_TOKEN");
  });

  it("reads profiles, which the variables override", () => {
    writeFileSync(
      join(state, "remotes.toml"),
      `[vm]\nurl = "https://vm.example:7470"\ntoken = "from-file"\nfingerprint = "${pin}"\n`,
    );
    const plain = choose("vm", { stateDir: state, env: () => undefined });
    expect(plain).toMatchObject({ token: "from-file", fingerprint: pin, name: "vm" });
    const env: Record<string, string> = {
      REEF_REMOTE_TOKEN: "from-env",
      REEF_REMOTE_FINGERPRINT: other,
    };
    const overridden = choose("vm", { stateDir: state, env: (n) => env[n] });
    expect(overridden).toMatchObject({ token: "from-env", fingerprint: other });
    const given = choose("vm", { stateDir: state, token: "given", env: (n) => env[n] });
    expect(given.token).toBe("given");
  });

  it("names an unknown profile", () => {
    let thrown: unknown;
    try {
      choose("nope", { stateDir: state, env: () => undefined });
    } catch (error) {
      thrown = error;
    }
    expect(thrown).toBeInstanceOf(ConfigError);
    expect((thrown as ConfigError).kind).toBe("not_found");
    expect((thrown as ConfigError).next).toEqual(["reef", "remote", "list"]);
  });

  it("refuses a profiles file that is not TOML, or a wrong profile", () => {
    writeFileSync(join(state, "remotes.toml"), "[vm\n");
    expect(() => choose("vm", { stateDir: state, env: () => undefined })).toThrow(ConfigError);
    writeFileSync(join(state, "remotes.toml"), '[vm]\nurl = "https://vm"\n');
    expect(() => choose("vm", { stateDir: state, env: () => undefined })).toThrow(/is wrong/);
  });

  it("refuses a socket with a remote", async () => {
    await expect(connect({ socket: "/x.sock", remote: "vm" })).rejects.toBeInstanceOf(TypeError);
  });

  it("knows a fingerprint", () => {
    expect(isFingerprint(pin)).toBe(true);
    expect(isFingerprint("sha256:" + "A".repeat(64))).toBe(false);
    expect(isFingerprint("a".repeat(64))).toBe(false);
  });
});
