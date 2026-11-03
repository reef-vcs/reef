// The integration tests' harness: a scratch daemon, `$REEF_BIN_DIR/reef-daemon` (default
// `${CARGO_TARGET_DIR:-<repo>/target}/debug`), on a state directory whose path has a space.

import { type ChildProcess, execFileSync, spawn } from "node:child_process";
import {
  existsSync,
  mkdirSync,
  mkdtempSync,
  openSync,
  readFileSync,
  rmSync,
  writeFileSync,
} from "node:fs";
import { createServer } from "node:net";
import { tmpdir } from "node:os";
import { join, resolve } from "node:path";

import { IfNotJj } from "../../src/api/v1.js";
import { connect, type Client, isReefError } from "../../src/index.js";

export const ROOT = resolve(import.meta.dirname, "..", "..", "..", "..");

/** Where the debug `reef` and `reef-daemon` are. */
export function binDir(): string {
  const given = process.env["REEF_BIN_DIR"];
  if (given) {
    return given;
  }
  return join(process.env["CARGO_TARGET_DIR"] || join(ROOT, "target"), "debug");
}

export const BASE_CONFIG = '[watcher]\nbackend = "notify"\n\n[lease]\nheartbeat = "1s"\n';

function sleep(ms: number): Promise<void> {
  return new Promise((done) => setTimeout(done, ms));
}

/** A scratch daemon. */
export class Daemon {
  readonly root: string;
  readonly config: string;
  readonly env: Record<string, string>;
  process: ChildProcess | undefined;

  constructor(config: string = BASE_CONFIG) {
    if (!existsSync(join(binDir(), "reef-daemon"))) {
      throw new Error(
        `no reef-daemon in ${binDir()}: run \`cargo build -p reef-vcs -p reef-daemon\`, or set REEF_BIN_DIR`,
      );
    }
    this.root = mkdtempSync(join(tmpdir(), "reef ts "));
    this.config = config;
    this.env = daemonEnv(this.root);
    if (Buffer.byteLength(this.socket) > 103) {
      throw new Error(`${this.socket} is too long for a socket: set TMPDIR to a shorter directory`);
    }
  }

  get state(): string {
    return join(this.root, "state dir");
  }

  get socket(): string {
    return join(this.state, "daemon.sock");
  }

  async start(): Promise<void> {
    mkdirSync(this.state, { recursive: true, mode: 0o700 });
    mkdirSync(join(this.root, "home"), { recursive: true });
    writeFileSync(join(this.state, "config.toml"), this.config);
    const log = openSync(join(this.root, "daemon.log"), "a");
    const child = spawn(join(binDir(), "reef-daemon"), ["--foreground"], {
      env: this.env,
      cwd: this.root,
      stdio: ["ignore", log, log],
    });
    this.process = child;
    const deadline = Date.now() + 30_000;
    for (;;) {
      if (child.exitCode !== null) {
        throw new Error(`reef-daemon exited during start:\n${this.log()}`);
      }
      try {
        const client = await connect({ stateDir: this.state, connectTimeoutMs: 500 });
        await client.close();
        return;
      } catch (error) {
        if (!isReefError(error) || Date.now() > deadline) {
          throw new Error(`reef-daemon did not answer in 30 s:\n${this.log()}`, { cause: error });
        }
        await sleep(100);
      }
    }
  }

  async stop(): Promise<void> {
    const child = this.process;
    this.process = undefined;
    if (child === undefined || child.exitCode !== null) {
      return;
    }
    const exited = new Promise((done) => child.once("exit", done));
    child.kill("SIGTERM");
    const timer = setTimeout(() => child.kill("SIGKILL"), 30_000);
    await exited;
    clearTimeout(timer);
  }

  async remove(): Promise<void> {
    await this.stop();
    rmSync(this.root, { recursive: true, force: true });
  }

  log(): string {
    try {
      return readFileSync(join(this.root, "daemon.log"), "utf8").slice(-4000);
    } catch {
      return "";
    }
  }

  /** Runs the debug `reef` CLI against this daemon. */
  reef(...args: string[]): string {
    return execFileSync(join(binDir(), "reef"), args, { env: this.env, encoding: "utf8" });
  }

  /** A client to this daemon. */
  client(agent = "sdk-test"): Promise<Client> {
    return connect({ stateDir: this.state, agent });
  }
}

export function daemonEnv(root: string): Record<string, string> {
  const env: Record<string, string> = {};
  for (const [key, value] of Object.entries(process.env)) {
    if (value !== undefined && !key.startsWith("REEF_") && !key.startsWith("XDG_")) {
      env[key] = value;
    }
  }
  return {
    ...env,
    REEF_STATE_DIR: join(root, "state dir"),
    REEF_LOG: "info",
    HOME: join(root, "home"),
    GIT_CONFIG_GLOBAL: "/dev/null",
    GIT_CONFIG_NOSYSTEM: "1",
    GIT_AUTHOR_NAME: "reef sdk",
    GIT_AUTHOR_EMAIL: "sdk@example.com",
    GIT_COMMITTER_NAME: "reef sdk",
    GIT_COMMITTER_EMAIL: "sdk@example.com",
  };
}

function git(cwd: string, env: Record<string, string>, ...args: string[]): void {
  execFileSync("git", args, { cwd, env, stdio: "ignore" });
}

/** A git repository with three commits under the daemon's root. */
export function makeRepo(daemon: Daemon, name: string): string {
  const path = join(daemon.root, name);
  mkdirSync(path);
  git(path, daemon.env, "init", "-q", "-b", "main");
  for (let n = 0; n < 3; n++) {
    writeFileSync(join(path, "README.md"), `The SDK's fixture, commit ${n}.\n`);
    git(path, daemon.env, "add", "README.md");
    git(path, daemon.env, "commit", "-q", "-m", `commit ${n}`);
  }
  return path;
}

/** Registers a fixture repository through the SDK; its id. */
export async function registerRepo(daemon: Daemon, name = "fixture repo"): Promise<string> {
  const path = makeRepo(daemon, name);
  const client = await daemon.client();
  try {
    const response = await client.registerRepository({ path, ifNotJj: IfNotJj.INIT_ALONGSIDE_GIT });
    return response.repository?.id ?? "";
  } finally {
    await client.close();
  }
}

/** A free TCP port on 127.0.0.1. */
export function freePort(): Promise<number> {
  return new Promise((done, fail) => {
    const server = createServer();
    server.once("error", fail);
    server.listen(0, "127.0.0.1", () => {
      const address = server.address();
      server.close(() => {
        done(typeof address === "object" && address !== null ? address.port : 0);
      });
    });
  });
}

/** Waits until `check` returns a value, polling. */
export async function eventually<T>(
  check: () => Promise<T | undefined> | T | undefined,
  timeoutMs = 5_000,
): Promise<T> {
  const deadline = Date.now() + timeoutMs;
  for (;;) {
    const value = await check();
    if (value !== undefined) {
      return value;
    }
    if (Date.now() > deadline) {
      throw new Error(`not within ${timeoutMs} ms`);
    }
    await sleep(50);
  }
}
