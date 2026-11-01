// The scratch daemon the smoke test runs against (#128), after
// scripts/test-remote.sh: a temporary root holding the state directory, the
// socket, a home and a fixture repository; the configuration written before
// any `reef` call; a foreground `reef-daemon` this harness spawns, logs and
// restarts; and `--no-autostart` on every call, so a call made while it is
// down fails instead of starting another, unlogged daemon.
//
// Every child gets this root's environment and none of the caller's `REEF_*`
// (a run from inside `reef ws run` has them), and its own HOME and git
// configuration, so the test touches no daemon, state or configuration but
// its own. The root is in TMPDIR, not under web/test-results/, because a Unix
// socket's path must fit in 103 bytes.
import { execFile, spawn, type ChildProcess } from "node:child_process";
import { once } from "node:events";
import { closeSync, existsSync, openSync, realpathSync } from "node:fs";
import { mkdir, mkdtemp, readdir, readFile, rm, writeFile } from "node:fs/promises";
import { tmpdir } from "node:os";
import { dirname, isAbsolute, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";
import { promisify } from "node:util";
import { test as base, expect } from "@playwright/test";

const run = promisify(execFile);

/** `web/`. */
export const WEB_DIR = dirname(dirname(fileURLToPath(import.meta.url)));

const SOCKET_MAX = 103;
const SESSION = /#session=[A-Za-z0-9_-]+/g;

function binDir(): string {
  const dir = process.env.REEF_E2E_BIN_DIR ?? "../target/debug";
  return isAbsolute(dir) ? dir : resolve(WEB_DIR, dir);
}

function redact(text: string): string {
  return text.replace(SESSION, "#session=<redacted>");
}

export interface Workspace {
  id: string;
  /** The change's id as jj prints it, for the smartlog's `change <8>`. */
  change: string;
}

export interface Session {
  /** `http://127.0.0.1:<port>/`. */
  url: string;
  /** `url` with `#session=<token>`. */
  sessionUrl: string;
  token: string;
}

export class Reef {
  readonly bin = binDir();
  readonly state: string;
  readonly fixture: string;
  repo = "";
  readonly workspaces: Workspace[] = [];
  private readonly env: NodeJS.ProcessEnv;
  private daemon: ChildProcess | null = null;
  private starts = 0;
  private readonly transcript: string[] = [];

  constructor(readonly root: string) {
    this.state = join(root, "state");
    this.fixture = join(root, "e2e-fixture");
    // No caller's Reef, jj or git state (a git hook exports GIT_DIR), and no
    // XDG configuration (git reads its default excludes from there).
    const inherited = Object.entries(process.env).filter(
      ([key]) => !/^(REEF|JJ|GIT|XDG)_/.test(key) && key !== "RUST_LOG",
    );
    this.env = {
      ...Object.fromEntries(inherited),
      REEF_STATE_DIR: this.state,
      REEF_SOCKET: join(root, "sock"),
      REEF_CONFIG: join(this.state, "config.toml"),
      REEF_OUTPUT: "json",
      REEF_LOG: "info",
      NO_COLOR: "1",
      HOME: join(root, "home"),
      XDG_CONFIG_HOME: join(root, "home", ".config"),
      GIT_CONFIG_GLOBAL: "/dev/null",
      GIT_CONFIG_NOSYSTEM: "1",
      GIT_AUTHOR_NAME: "reef e2e",
      GIT_AUTHOR_EMAIL: "e2e@example.com",
      GIT_COMMITTER_NAME: "reef e2e",
      GIT_COMMITTER_EMAIL: "e2e@example.com",
    };
  }

  /** An empty root; `setup` fills it. */
  static async create(): Promise<Reef> {
    for (const binary of ["reef", "reef-daemon"]) {
      if (!existsSync(join(binDir(), binary))) {
        throw new Error(
          `no ${binary} in ${binDir()}: run \`just web-e2e\`, which builds the app and both binaries`,
        );
      }
    }
    const root = realpathSync(await mkdtemp(join(tmpdir(), "reef-e2e-")));
    const socket = join(root, "sock");
    if (Buffer.byteLength(socket) > SOCKET_MAX) {
      await rm(root, { recursive: true, force: true });
      throw new Error(
        `the socket path ${socket} is over ${String(SOCKET_MAX)} bytes: set TMPDIR to a shorter directory`,
      );
    }
    return new Reef(root);
  }

  /** A configuration, a running daemon, the fixture and two workspaces. */
  async setup() {
    await mkdir(this.state, { mode: 0o700 });
    await mkdir(join(this.root, "home"));
    await this.writeConfig("127.0.0.1:0");
    await this.start();

    await mkdir(this.fixture);
    await this.git("init", "-q", "-b", "main");
    await writeFile(join(this.fixture, "README.md"), "The smoke test's fixture.\n");
    await this.git("add", "README.md");
    await this.git("commit", "-q", "-m", "fixture");
    const registered = (await this.reef(["repo", "register", this.fixture])) as {
      id?: string;
      repository?: { id: string };
    };
    this.repo = registered.repository?.id ?? registered.id ?? "";
    expect(this.repo, "repo register printed an id").not.toBe("");
    // Two workspaces, so that a describe of the older one would move it to
    // the top in jj's order: only the smartlog's ranks keep it in place.
    for (let i = 0; i < 2; i++) {
      const ws = (await this.reef([
        "ws",
        "create",
        "--repo",
        this.repo,
        "--purpose",
        "e2e smoke",
      ])) as { id: string; change_id_reverse_hex: string };
      this.workspaces.push({ id: ws.id, change: ws.change_id_reverse_hex });
    }
  }

  /**
   * `[ui] listen`, read by the next start. The watcher is notify's: Watchman,
   * if the machine runs one, is the user's daemon, not this test's.
   */
  async writeConfig(listen: string) {
    await writeFile(
      join(this.state, "config.toml"),
      `[ui]\nlisten = "${listen}"\n\n[watcher]\nbackend = "notify"\n`,
    );
  }

  async start() {
    if (this.daemon) throw new Error("the daemon is already running");
    this.starts++;
    const log = openSync(join(this.root, `daemon-${String(this.starts)}.log`), "a");
    const daemon = spawn(join(this.bin, "reef-daemon"), ["--foreground"], {
      env: this.env,
      cwd: this.root,
      stdio: ["ignore", log, log],
    });
    closeSync(log);
    this.daemon = daemon;
    // A worker that exits without its teardown (a timeout) takes the daemon along.
    const kill = () => daemon.kill("SIGKILL");
    process.once("exit", kill);
    const exit = { gone: false };
    daemon.once("exit", () => {
      exit.gone = true;
      process.off("exit", kill);
    });
    try {
      await expect
        .poll(
          async () => {
            if (exit.gone) return "exited";
            try {
              await this.reef(["daemon", "status"], { quiet: true });
              return "ready";
            } catch {
              return "starting";
            }
          },
          { timeout: 30_000, intervals: [100] },
        )
        .toMatch(/^(ready|exited)$/);
    } catch (error) {
      throw new Error(`reef-daemon did not answer in 30 s:\n${await this.daemonLog()}`, {
        cause: error,
      });
    }
    if (exit.gone) throw new Error(`reef-daemon exited during start:\n${await this.daemonLog()}`);
  }

  async stop() {
    const daemon = this.daemon;
    if (!daemon) return;
    this.daemon = null;
    const running = () => daemon.exitCode === null && daemon.signalCode === null;
    if (!running()) return;
    try {
      await this.reef(["daemon", "stop"]);
      if (running()) await once(daemon, "exit", { signal: AbortSignal.timeout(30_000) });
    } catch (error) {
      if (running()) {
        const exit = once(daemon, "exit");
        daemon.kill("SIGKILL");
        await exit;
      }
      throw new Error(`reef-daemon did not stop: ${String(error)}`, { cause: error });
    }
  }

  /** A new web session: `reef ui --print`. */
  async session(): Promise<Session> {
    let printed: { url: string; session_url?: string };
    try {
      printed = (await this.reef(["ui", "--print"])) as typeof printed;
    } catch (error) {
      if (String(error).includes("WEB_UI_NOT_BUILT")) {
        throw new Error(
          "this reef-daemon embeds the placeholder page, not the app: run `just web-e2e`",
          { cause: error },
        );
      }
      throw error;
    }
    const sessionUrl = printed.session_url ?? "";
    const token = sessionUrl.split("#session=")[1] ?? "";
    expect(token, "reef ui --print gave a session URL").not.toBe("");
    return { url: printed.url, sessionUrl, token };
  }

  /** `reef --no-autostart <args>`, parsed as JSON; throws with its output on failure. */
  async reef(args: string[], options: { agent?: string; quiet?: boolean } = {}): Promise<unknown> {
    const env = options.agent ? { ...this.env, REEF_AGENT: options.agent } : this.env;
    const argv = ["--no-autostart", ...args];
    let stdout: string;
    let stderr: string;
    try {
      ({ stdout, stderr } = await run(join(this.bin, "reef"), argv, {
        env,
        cwd: this.root,
        timeout: 60_000,
        killSignal: "SIGKILL",
      }));
    } catch (error) {
      const failed = error as { code?: number; stdout?: string; stderr?: string };
      const text = `$ reef ${argv.join(" ")}\nexit ${String(failed.code)}\n${failed.stdout ?? ""}${failed.stderr ?? ""}`;
      if (!options.quiet) this.transcript.push(text);
      throw new Error(redact(text), { cause: error });
    }
    if (!options.quiet) this.transcript.push(`$ reef ${argv.join(" ")}\n${stdout}${stderr}`);
    return stdout.trim() ? (JSON.parse(stdout) as unknown) : null;
  }

  private async git(...args: string[]) {
    await run("git", args, { env: this.env, cwd: this.fixture, timeout: 60_000 });
  }

  private async daemonLog(): Promise<string> {
    return readFile(join(this.root, `daemon-${String(this.starts)}.log`), "utf8").catch(
      () => "(no log)",
    );
  }

  /** What a failed test attaches: the daemons' logs, the op log and every call. */
  async evidence(): Promise<{ name: string; body: string }[]> {
    const found: { name: string; body: string }[] = [];
    for (let n = 1; n <= this.starts; n++) {
      const body = await readFile(join(this.root, `daemon-${String(n)}.log`), "utf8").catch(
        () => "",
      );
      found.push({ name: `daemon-${String(n)}.log`, body });
    }
    const logs = join(this.state, "logs");
    for (const name of await readdir(logs).catch(() => [])) {
      found.push({ name, body: await readFile(join(logs, name), "utf8") });
    }
    if (this.daemon && this.repo) {
      const ops = await this.reef(["op", "log", "--repo", this.repo, "--limit", "20"], {
        quiet: true,
      }).catch((error: unknown) => String(error));
      found.push({ name: "op-log.json", body: JSON.stringify(ops, null, 2) });
    }
    found.push({ name: "reef-calls.txt", body: this.transcript.join("\n") });
    return found.map(({ name, body }) => ({ name, body: redact(body) }));
  }

  async dispose() {
    try {
      // Released while the daemon runs: on Linux each workspace is a
      // fuse-overlayfs mount, which outlives a stop (workspaces survive restarts).
      if (this.daemon) {
        for (const ws of this.workspaces) {
          await this.reef(["ws", "release", ws.id], { quiet: true }).catch(() => undefined);
        }
      }
      await this.stop();
    } finally {
      await rm(this.root, { recursive: true, force: true }).catch(async () => {
        await this.unmount();
        await rm(this.root, { recursive: true, force: true });
      });
    }
  }

  /** Lazily unmounts what a daemon that did not release its workspaces left mounted. */
  private async unmount() {
    if (process.platform !== "linux") return;
    const repos = join(this.state, "repos");
    for (const repo of await readdir(repos).catch(() => [])) {
      for (const ws of await readdir(join(repos, repo, "ws")).catch(() => [])) {
        const path = join(repos, repo, "ws", ws);
        await run("fusermount3", ["-u", "-z", path])
          .catch(() => run("fusermount", ["-u", "-z", path]))
          .catch(() => undefined);
      }
    }
  }
}

export const test = base.extend<{ reef: Reef }>({
  reef: async ({}, use, testInfo) => {
    const reef = await Reef.create();
    // A setup failure is not yet the test's status when this teardown runs.
    const failed = { setup: false };
    try {
      await reef.setup().catch((error: unknown) => {
        failed.setup = true;
        throw error;
      });
      await use(reef);
    } finally {
      if (failed.setup || testInfo.status !== testInfo.expectedStatus) {
        for (const { name, body } of await reef.evidence()) {
          await testInfo.attach(name, { body, contentType: "text/plain" });
        }
      }
      await reef.dispose();
    }
  },
});

export { expect };
