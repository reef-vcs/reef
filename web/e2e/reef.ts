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

/** The lines of `text` its writer has finished, without the last partial one. */
function complete(text: string): string[] {
  return text.split("\n").slice(0, -1);
}

function redact(text: string): string {
  return text.replace(SESSION, "#session=<redacted>");
}

export interface Workspace {
  id: string;
  /** The change's id as jj prints it, for the smartlog's `change <8>`. */
  change: string;
  /** Its directory, for writing a file into it. */
  path: string;
}

export interface Session {
  /** `http://localhost:<port>/`. */
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
  /** The default identity's name, once `setup` was given a policy. */
  identity = "";
  readonly workspaces: Workspace[] = [];
  private readonly env: NodeJS.ProcessEnv;
  private daemon: ChildProcess | null = null;
  /** The commands `started` left running, killed at teardown. */
  private readonly children: ChildProcess[] = [];
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

  /**
   * A configuration, a running daemon, the fixture and two workspaces. With
   * `policy`, the fixture's first commit holds `reef-policy.toml`, its text
   * given the default identity's name.
   */
  async setup(options: { policy?: (identity: string) => string } = {}) {
    await mkdir(this.state, { mode: 0o700 });
    await mkdir(join(this.root, "home"));
    await this.writeConfig("127.0.0.1:0");
    await this.start();

    await mkdir(this.fixture);
    await this.git("init", "-q", "-b", "main");
    await writeFile(join(this.fixture, "README.md"), "The smoke test's fixture.\n");
    await this.git("add", "README.md");
    if (options.policy) {
      this.identity = await this.defaultIdentity();
      await writeFile(join(this.fixture, "reef-policy.toml"), options.policy(this.identity));
      await this.git("add", "reef-policy.toml");
    }
    await this.git("commit", "-q", "-m", "fixture");
    const registered = (await this.reef(["repo", "register", this.fixture])) as {
      id?: string;
      repository?: { id: string };
    };
    this.repo = registered.repository?.id ?? registered.id ?? "";
    expect(this.repo, "repo register printed an id").not.toBe("");
    // Two workspaces, so that a describe of the older one would move it to
    // the top in jj's order: only the smartlog's ranks keep it in place.
    for (let i = 0; i < 2; i++) await this.workspace("e2e smoke");
  }

  /** Another git repository, registered now; its id. */
  async registerAnother(name: string): Promise<string> {
    const dir = join(this.root, name);
    await mkdir(dir);
    await this.git("-C", dir, "init", "-q", "-b", "main");
    await writeFile(join(dir, "README.md"), `The ${name} fixture.\n`);
    await this.git("-C", dir, "add", "README.md");
    await this.git("-C", dir, "commit", "-q", "-m", "fixture");
    const registered = (await this.reef(["repo", "register", dir])) as {
      id?: string;
      repository?: { id: string };
    };
    const id = registered.repository?.id ?? registered.id ?? "";
    expect(id, "repo register printed an id").not.toBe("");
    return id;
  }

  /** A new workspace on the repository's trunk (or `repo`'s), released at the end. */
  async workspace(purpose: string, repo = this.repo): Promise<Workspace> {
    const ws = (await this.reef(["ws", "create", "--repo", repo, "--purpose", purpose])) as {
      id: string;
      change_id_reverse_hex: string;
      path: string;
    };
    const workspace = { id: ws.id, change: ws.change_id_reverse_hex, path: ws.path };
    this.workspaces.push(workspace);
    return workspace;
  }

  /** Writes `files` into `ws`, snapshots it, and returns its change's commit id. */
  async edit(ws: Workspace, files: Record<string, string>): Promise<string> {
    for (const [name, text] of Object.entries(files)) await writeFile(join(ws.path, name), text);
    const snapshot = (await this.reef(["ws", "snapshot", ws.id])) as { commit_id: string };
    const commit = snapshot.commit_id;
    expect(commit, "ws snapshot printed the commit").toMatch(/^[0-9a-f]{40}$/);
    return commit;
  }

  /** The daemon's default identity's name. */
  private async defaultIdentity(): Promise<string> {
    const listed = (await this.reef(["identity", "list"])) as
      | { name: string; is_default: boolean }[]
      | { identities: { name: string; is_default: boolean }[] };
    const identities = Array.isArray(listed) ? listed : listed.identities;
    const name = identities.find((identity) => identity.is_default)?.name ?? "";
    expect(name, "the daemon has a default identity").not.toBe("");
    return name;
  }

  /**
   * `[ui] listen`, read by the next start. The watcher is notify's: Watchman,
   * if the machine runs one, is the user's daemon, not this test's. A 5 s
   * heartbeat lets a workspace's lease entry lapse within the test (R2.20):
   * 10 s after its issue or renewal, since nothing renews a `ws create`.
   */
  async writeConfig(listen: string) {
    await writeFile(
      join(this.state, "config.toml"),
      `[ui]\nlisten = "${listen}"\n\n[watcher]\nbackend = "notify"\n\n[lease]\nheartbeat = "5s"\n`,
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

  /**
   * `reef --no-autostart <args>` left running: its stderr lines as they
   * come, and its exit with stdout parsed as JSON. For a command that waits
   * on the browser, such as `evidence add --verified`.
   */
  started(args: string[]): {
    line: (match: RegExp) => Promise<string>;
    exit: Promise<{ code: number | null; json: unknown }>;
  } {
    const argv = ["--no-autostart", ...args];
    const child = spawn(join(this.bin, "reef"), argv, {
      env: this.env,
      cwd: this.root,
      stdio: ["ignore", "pipe", "pipe"],
    });
    let stdout = "";
    let stderr = "";
    const waiting: { match: RegExp; found: (line: string) => void }[] = [];
    child.stdout.setEncoding("utf8").on("data", (chunk: string) => (stdout += chunk));
    child.stderr.setEncoding("utf8").on("data", (chunk: string) => {
      stderr += chunk;
      for (const wait of [...waiting]) {
        const line = complete(stderr).find((each) => wait.match.test(each));
        if (line !== undefined) {
          waiting.splice(waiting.indexOf(wait), 1);
          wait.found(line);
        }
      }
    });
    // `close`, not `exit`: stdout and stderr are drained by then.
    this.children.push(child);
    const closed = once(child, "close");
    const exit = closed.then(([code]) => {
      this.transcript.push(`$ reef ${argv.join(" ")}\nexit ${String(code)}\n${stdout}${stderr}`);
      return {
        code: code as number | null,
        json: stdout.trim() ? (JSON.parse(stdout) as unknown) : null,
      };
    });
    const line = (match: RegExp) =>
      new Promise<string>((found, failed) => {
        const seen = complete(stderr).find((each) => match.test(each));
        if (seen !== undefined) {
          found(seen);
          return;
        }
        waiting.push({ match, found });
        const gone = () => {
          failed(
            new Error(
              redact(`reef ${argv.join(" ")} exited before printing ${String(match)}:\n${stderr}`),
            ),
          );
        };
        closed.then(gone, gone);
      });
    exit.catch(() => undefined);
    return { line, exit };
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
    for (const child of this.children) child.kill("SIGKILL");
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

export const test = base.extend<{
  reef: Reef;
  /**
   * The fixture's first `reef-policy.toml`, from the default identity's name;
   * none by default. In an object: Playwright takes a function for a fixture.
   */
  policy: { text: (identity: string) => string } | undefined;
}>({
  policy: [undefined, { option: true }],
  reef: async ({ policy }, use, testInfo) => {
    const reef = await Reef.create();
    // A setup failure is not yet the test's status when this teardown runs.
    const failed = { setup: false };
    try {
      await reef.setup(policy ? { policy: policy.text } : {}).catch((error: unknown) => {
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
