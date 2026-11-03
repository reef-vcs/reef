// The installed tarball against a real daemon: what the release checks before publishing.
//
// Copy this file into a directory where only the packed reef-vcs is installed (not this
// checkout), and run it with REEF_BIN_DIR naming a directory with reef-daemon:
//
//     REEF_BIN_DIR=target/debug node smoke.mjs
//
// Starts a scratch daemon, connects, checks the version and API revision, registers a fixture
// repository, runs a workspace block with one edit, reads the edit back from the change, and
// stops the daemon. Exits non-zero on any failure.

import { execFileSync, spawn } from "node:child_process";
import { mkdirSync, mkdtempSync, openSync, rmSync, writeFileSync } from "node:fs";
import { createRequire } from "node:module";
import { tmpdir } from "node:os";
import { join } from "node:path";

const sdk = await import("reef-vcs");
const resolved = createRequire(import.meta.url).resolve("reef-vcs");
if (!resolved.includes(`${join("node_modules", "reef-vcs")}`)) {
  console.error(`reef-vcs resolves to ${resolved}, not an installed package`);
  process.exit(2);
}

const binDir = process.env.REEF_BIN_DIR;
if (!binDir) {
  console.error("set REEF_BIN_DIR to a directory with reef-daemon");
  process.exit(2);
}
const root = mkdtempSync(join(tmpdir(), "reef smoke "));
const state = join(root, "state");
mkdirSync(state, { mode: 0o700 });
writeFileSync(join(state, "config.toml"), '[watcher]\nbackend = "notify"\n');
const env = Object.fromEntries(Object.entries(process.env).filter(([k]) => !k.startsWith("REEF_")));
Object.assign(env, {
  REEF_STATE_DIR: state,
  HOME: root,
  GIT_CONFIG_GLOBAL: "/dev/null",
  GIT_AUTHOR_NAME: "smoke",
  GIT_AUTHOR_EMAIL: "smoke@example.com",
  GIT_COMMITTER_NAME: "smoke",
  GIT_COMMITTER_EMAIL: "smoke@example.com",
});
const log = openSync(join(root, "daemon.log"), "w");
const daemon = spawn(join(binDir, "reef-daemon"), ["--foreground"], {
  env,
  stdio: ["ignore", log, log],
});

let failed = false;
try {
  const deadline = Date.now() + 30_000;
  let client;
  for (;;) {
    try {
      client = await sdk.connect({ stateDir: state, connectTimeoutMs: 500 });
      break;
    } catch (error) {
      if (
        !(error instanceof sdk.DaemonNotRunningError) ||
        Date.now() > deadline ||
        daemon.exitCode !== null
      ) {
        throw error;
      }
      await new Promise((done) => setTimeout(done, 100));
    }
  }
  try {
    const status = await client.daemonStatus();
    console.log(
      `reef-vcs ${sdk.VERSION} (API revision ${sdk.API_REVISION}) against reef ${status.version} (API revision ${status.apiRevision})`,
    );
    if (status.apiRevision !== sdk.API_REVISION) throw new Error("API revisions differ");
    if (status.version !== sdk.VERSION) throw new Error("versions differ");

    const fixture = join(root, "fixture");
    mkdirSync(fixture);
    for (const args of [
      ["init", "-q", "-b", "main"],
      ["commit", "-q", "--allow-empty", "-m", "fixture"],
    ]) {
      execFileSync("git", args, { cwd: fixture, env, stdio: "ignore" });
    }
    const { IfNotJj } = await import("reef-vcs/api/v1");
    const repo = (
      await client.registerRepository({ path: fixture, ifNotJj: IfNotJj.INIT_ALONGSIDE_GIT })
    ).repository.id;
    const change = await client.withWorkspace({ repo, purpose: "smoke" }, (ws) => {
      writeFileSync(join(ws.path, "smoke.txt"), "from the tarball\n");
      return ws.changeId;
    });
    const read = await client.readFile({ repo, revision: change, path: "smoke.txt" });
    const text = Buffer.from(read.content).toString();
    if (text !== "from the tarball\n") throw new Error(`read back ${JSON.stringify(text)}`);
  } finally {
    await client.close();
  }
  console.log("smoke test passed");
} catch (error) {
  failed = true;
  console.error(error);
} finally {
  daemon.kill("SIGTERM");
  await new Promise((done) => {
    const timer = setTimeout(() => daemon.kill("SIGKILL"), 30_000);
    daemon.once("exit", () => {
      clearTimeout(timer);
      done();
    });
    if (daemon.exitCode !== null) done();
  });
  if (process.env.REEF_SMOKE_KEEP) {
    console.log(`kept ${root}`);
  } else {
    rmSync(root, { recursive: true, force: true });
  }
}
process.exit(failed ? 1 : 0);
