// The SDK against a scratch daemon on its socket (R2.07's tests 15 to 24).

import { execFileSync } from "node:child_process";
import { mkdtempSync, rmSync, writeFileSync } from "node:fs";
import { tmpdir } from "node:os";
import { join } from "node:path";

import { afterAll, beforeAll, describe, expect, it } from "vitest";

import {
  IfNotJj,
  LeaseState,
  PresenceEndReason,
  PresenceKind,
  WorkspaceState,
  type Presence,
  type SubscribePresenceResponse,
} from "../../src/api/v1.js";
import {
  API_REVISION,
  type Client,
  connect,
  DaemonNotRunningError,
  instructionSha256,
  InvalidArgumentError,
  NotFoundError,
  Reporter,
  VERSION,
} from "../../src/index.js";
import { Daemon, eventually, makeRepo, registerRepo } from "./harness.js";

let daemon: Daemon;
let repo: string;
let client: Client;

beforeAll(async () => {
  daemon = new Daemon();
  await daemon.start();
  repo = await registerRepo(daemon);
  client = await daemon.client();
});

afterAll(async () => {
  await client.close();
  await daemon.remove();
});

const nodeMajor = Number(process.versions.node.split(".")[0]);

describe("local daemon", () => {
  // 15
  it("connects through discovery", async () => {
    const saved = { ...process.env };
    try {
      for (const name of ["REEF_SOCKET", "REEF_CONFIG", "REEF_REMOTE"]) {
        Reflect.deleteProperty(process.env, name);
      }
      process.env["REEF_STATE_DIR"] = daemon.state;
      const found = await connect();
      expect(found.target).toBe(daemon.socket);
      expect(found.target).toContain(" ");
      expect((await found.getDaemonStatus({ skipBackends: true })).version).toBe(VERSION);
      await found.close();
    } finally {
      process.env = saved;
    }
  });

  // 16
  it("api revision matches the daemon", async () => {
    const status = await client.getDaemonStatus({ skipBackends: true });
    expect(status.version).toBe(VERSION);
    expect(status.apiRevision).toBe(API_REVISION);
    expect((await client.daemonStatus()).apiRevision).toBe(API_REVISION);
    await client.requireRevision(API_REVISION);
    await expect(client.requireRevision(API_REVISION + 1)).rejects.toMatchObject({
      reason: "API_VERSION_MISMATCH",
    });
  });

  // 17
  it("daemon not running", async () => {
    const empty = mkdtempSync(join(tmpdir(), "reef-none-"));
    try {
      const started = Date.now();
      const error: unknown = await connect({ stateDir: empty, connectTimeoutMs: 2_000 }).catch(
        (e: unknown) => e,
      );
      expect(error).toBeInstanceOf(DaemonNotRunningError);
      expect((error as DaemonNotRunningError).next).toEqual(["reef", "daemon", "start"]);
      expect((error as DaemonNotRunningError).kind).toBe("unavailable");
      expect(Date.now() - started).toBeLessThan(3_000);
    } finally {
      rmSync(empty, { recursive: true, force: true });
    }
  });

  // 18
  it("errors carry the detail", async () => {
    const missing: unknown = await client
      .getWorkspace({ workspace: "0000000000000000" })
      .catch((e: unknown) => e);
    expect(missing).toBeInstanceOf(NotFoundError);
    expect(missing).toMatchObject({
      kind: "not_found",
      reason: "WORKSPACE_NOT_FOUND",
      next: ["reef", "ws", "list", "--all"],
      metadata: { workspace: "0000000000000000" },
      method: "/reef.api.v1.WorkspaceService/GetWorkspace",
    });
    expect((missing as NotFoundError).hint).not.toBe("");
    const revset: unknown = await client
      .listChanges({ repo, revset: "((" })
      .catch((e: unknown) => e);
    expect(revset).toBeInstanceOf(InvalidArgumentError);
    expect(revset).toMatchObject({ reason: "INVALID_REVSET" });
  });

  // 19
  it("pages follow to the end", async () => {
    const ws = await client.workspace({ repo, purpose: "pages" });
    try {
      for (let n = 0; n < 25; n++) {
        writeFileSync(join(ws.path, `f${n}.txt`), `${n}\n`);
        await ws.snapshot();
      }
    } finally {
      await ws.finish(undefined);
    }
    const ids: string[] = [];
    for await (const op of client.listOperationsAll({ repo, pageSize: 2 })) {
      ids.push(op.id);
    }
    const paged: string[] = [];
    for await (const page of client.listOperationsPages({ repo, pageSize: 2 })) {
      paged.push(...page.operations.map((op) => op.id));
    }
    expect(ids.length).toBeGreaterThanOrEqual(25);
    expect(new Set(ids).size).toBe(ids.length);
    expect(ids).toEqual(paged);
    let pages = 0;
    for await (const page of client.listWorkspacesPages({ repo, pageSize: 1 })) {
      pages += 1;
      if (page.nextPageToken === "") {
        break;
      }
    }
    expect(pages).toBeGreaterThan(0);
  });

  // 20
  it("workspace block with withWorkspace", async () => {
    const watcher = client.subscribePresence({});
    const seen: SubscribePresenceResponse[] = [];
    const collecting = (async () => {
      for await (const frame of watcher) {
        seen.push(frame);
      }
    })();
    let id = "";
    let path = "";
    await client.withWorkspace({ repo, purpose: "the block", activity: "testing" }, async (ws) => {
      id = ws.id;
      path = ws.path;
      const first = ws.workspace.lease?.lastHeartbeatAt?.seconds ?? 0n;
      await eventually(async () => {
        const lease = (await client.getWorkspace({ workspace: ws.id })).workspace?.lease;
        const at = lease?.lastHeartbeatAt?.seconds ?? 0n;
        return at > first ? at : undefined;
      }, 4_000);
      const entry = await eventually(() =>
        live(seen).find((p) => p.workspaceId === ws.id && p.kind === PresenceKind.ACTIVITY),
      );
      expect(entry.agent).toBe("sdk-test");
      expect(entry.activity).toBe("testing");
      ws.activity("still testing");
      await eventually(() =>
        live(seen).find((p) => p.workspaceId === ws.id && p.activity === "still testing"),
      );
      writeFileSync(join(ws.path, "block.txt"), "from the block\n");
    });
    const after = (await client.getWorkspace({ workspace: id })).workspace;
    expect(after?.state).toBe(WorkspaceState.RELEASED);
    expect(path).not.toBe("");
    const file = await client.readFileToEnd({
      repo,
      revision: after?.changeId ?? "",
      path: "block.txt",
    });
    expect(Buffer.from(file.content).toString()).toBe("from the block\n");
    await eventually(() => ended(seen).find((p) => p.workspaceId === id));
    watcher.cancel();
    await collecting;
  });

  it.skipIf(nodeMajor < 24)("workspace block with await using", async () => {
    // `await using` is syntax Node 22 cannot parse, so the block lives in a module of its own.
    const { usingBlock } = await import("./using-block.js");
    const id = await usingBlock(client, repo);
    const after = (await client.getWorkspace({ workspace: id })).workspace;
    expect(after?.state).toBe(WorkspaceState.RELEASED);
  });

  it("keep leaves the workspace active, and a throwing block still releases", async () => {
    let kept = "";
    await client.withWorkspace({ repo, keep: true }, (ws) => {
      kept = ws.id;
    });
    expect((await client.getWorkspace({ workspace: kept })).workspace?.state).toBe(
      WorkspaceState.ACTIVE,
    );
    await client.releaseWorkspace({ workspace: kept });

    let thrown = "";
    const failure = new Error("the block failed");
    await expect(
      client.withWorkspace({ repo }, (ws) => {
        thrown = ws.id;
        throw failure;
      }),
    ).rejects.toBe(failure);
    expect((await client.getWorkspace({ workspace: thrown })).workspace?.state).toBe(
      WorkspaceState.RELEASED,
    );
  });

  // 21
  it("attach", async () => {
    const made = await client.createWorkspace({
      repo,
      owner: "other@machine",
      leaseLength: { seconds: 3n },
    });
    const id = made.workspace?.id ?? "";
    await expect(client.attach(id)).rejects.toMatchObject({ kind: "conflict" });
    await client.withAttached(id, { owner: "other@machine" }, async () => {
      await new Promise((done) => setTimeout(done, 4_000));
      const lease = (await client.getWorkspace({ workspace: id })).workspace?.lease;
      expect(lease?.state).toBe(LeaseState.ACTIVE);
    });
    expect((await client.getWorkspace({ workspace: id })).workspace?.state).toBe(
      WorkspaceState.ACTIVE,
    );
    await client.withAttached(id, { owner: "other@machine", release: true }, () => undefined);
    expect((await client.getWorkspace({ workspace: id })).workspace?.state).toBe(
      WorkspaceState.RELEASED,
    );
  });

  // R3.44
  it("workspace provenance", async () => {
    const first = "fix the parser\n";
    await client.withWorkspace(
      {
        repo,
        purpose: "provenance",
        reportPresence: false,
        provenance: {
          model: "example/model-1",
          task: "T-ts",
          instruction: first,
          clientSession: "s-1",
        },
      },
      async (ws) => {
        const context = ws.workspace.provenance?.context;
        expect(context?.model).toBe("example/model-1");
        expect(context?.task).toBe("T-ts");
        expect(context?.clientSession).toBe("s-1");
        expect(context?.instructionSha256).toBe(instructionSha256(first));
        writeFileSync(join(ws.path, "a.txt"), "a\n");
        const second = "now the tests";
        const declared = await ws.declare({ instruction: second, task: "" });
        expect(declared.operation).toBeDefined();
        expect(declared.session?.context?.instructionSha256).toBe(instructionSha256(second));
        expect(declared.session?.context?.task).toBe("");
        const records = await client.listProvenance({ repo, workspaceId: ws.id });
        expect(records.records.map((record) => record.context?.instructionSha256)).toEqual([
          instructionSha256(first),
        ]);
        const quiet = await ws.declare({ model: "m-2" }, { snapshot: false });
        expect(quiet.operation).toBeUndefined();
        await expect(ws.declare({ model: "two words" })).rejects.toMatchObject({
          kind: "invalid_argument",
        });
      },
    );
  });

  // 22
  it("files without a workspace", async () => {
    const edited = await client.editFiles({
      repo,
      target: { case: "newOn", value: "main" },
      edits: [{ path: "new.txt", action: { case: "content", value: Buffer.from("hello\n") } }],
      description: "an edit without a workspace",
    });
    const change = edited.change?.changeId ?? "";
    expect(change).not.toBe("");
    const read = await client.readFile({ repo, revision: change, path: "new.txt" });
    expect(Buffer.from(read.content).toString()).toBe("hello\n");

    const big = Buffer.alloc(6 << 20, "x");
    const path = makeRepo(daemon, "big repo");
    writeFileSync(join(path, "big.bin"), big);
    execGit(path, "add", "big.bin");
    execGit(path, "commit", "-q", "-m", "big");
    const head = execFileSync("git", ["rev-parse", "HEAD"], { cwd: path, encoding: "utf8" }).trim();
    const bigRepo =
      (await client.registerRepository({ path, ifNotJj: IfNotJj.INIT_ALONGSIDE_GIT })).repository
        ?.id ?? "";
    const one = await client.readFile({ repo: bigRepo, revision: head, path: "big.bin" });
    expect(BigInt(one.content.length)).toBeLessThan(one.size);
    const whole = await client.readFileToEnd({ repo: bigRepo, revision: head, path: "big.bin" });
    expect(whole.content.length).toBe(big.length);
    expect(Buffer.from(whole.content).equals(big)).toBe(true);
  });

  // R3.02: an upload in two chunks, then ReadArtifact's server stream.
  it("artifact reads back through the stream", async () => {
    const { createHash } = await import("node:crypto");
    const data = Buffer.alloc((1 << 20) + 4321, "artifact\n");
    const sha256 = createHash("sha256").update(data).digest("hex");
    const size = BigInt(data.length);
    const chunk = 1 << 20;
    const probe = await client.uploadArtifact({
      repo,
      sha256,
      size,
      offset: 0n,
      data: new Uint8Array(),
    });
    expect(probe.received).toBe(0n);
    await client.uploadArtifact({ repo, sha256, size, offset: 0n, data: data.subarray(0, chunk) });
    const last = await client.uploadArtifact({
      repo,
      sha256,
      size,
      offset: BigInt(chunk),
      data: data.subarray(chunk),
    });
    expect(last.created).toBe(true);
    const parts: Uint8Array[] = [];
    let recordSize = 0n;
    for await (const frame of client.readArtifact({ repo, sha256: sha256.slice(0, 12) })) {
      if (frame.artifact) {
        recordSize = frame.artifact.size;
      }
      parts.push(frame.data);
    }
    expect(recordSize).toBe(size);
    expect(Buffer.concat(parts).equals(data)).toBe(true);
  });

  // 23
  it("subscription streams and cancels", async () => {
    const ws = await client.workspace({ repo, reportPresence: false });
    try {
      const stream = client.subscribeOperations({ repos: [repo] });
      const got: string[] = [];
      const reading = (async () => {
        for await (const frame of stream) {
          got.push(...frame.operations.map((op) => op.id));
        }
      })();
      await new Promise((done) => setTimeout(done, 500));
      writeFileSync(join(ws.path, "streamed.txt"), "x\n");
      const snap = await ws.snapshot();
      const target = snap.operation?.id ?? "";
      await eventually(() => (got.includes(target) ? true : undefined));
      // Leaving `await using` disposes it; cancel() is the same on Node 22.
      await stream[Symbol.asyncDispose]();
      await reading;
      expect(stream.cancelled).toBe(true);
      let more = 0;
      for await (const _ of stream) {
        more += 1;
      }
      expect(more).toBe(0);
    } finally {
      await ws.finish(undefined);
    }
  });

  // 24
  it("presence reporter", async () => {
    const watcher = client.subscribePresence({});
    const seen: SubscribePresenceResponse[] = [];
    const collecting = (async () => {
      for await (const frame of watcher) {
        seen.push(frame);
      }
    })();
    const reporter = new Reporter(client, { agent: "editor-1", kind: PresenceKind.EDITING, repo });
    reporter.set({ paths: ["src/a.ts"] });
    const entry = await eventually(() =>
      live(seen).find((p) => p.agent === "editor-1" && p.paths.includes("src/a.ts")),
    );
    expect(reporter.isLive()).toBe(true);
    // A stale id is reported anew: the daemon forgets the entry, then the next set replaces it.
    await client.clearPresence({ presenceId: entry.id });
    await new Promise((done) => setTimeout(done, 1_100));
    reporter.set({ paths: ["src/b.ts"] });
    await eventually(() =>
      live(seen).find(
        (p) => p.agent === "editor-1" && p.paths.includes("src/b.ts") && p.id !== entry.id,
      ),
    );
    await reporter.clear();
    await eventually(() =>
      ended(seen).find((p) => p.agent === "editor-1" && p.paths.includes("src/b.ts")),
    );
    watcher.cancel();
    await collecting;
  });
});

/** The entries the frames reported live. */
function live(frames: SubscribePresenceResponse[]): Presence[] {
  return frames.flatMap((frame) =>
    frame.events.filter((e) => !e.ended && e.presence).map((e) => e.presence as Presence),
  );
}

/** The entries the frames reported cleared. */
function ended(frames: SubscribePresenceResponse[]): Presence[] {
  return frames.flatMap((frame) =>
    frame.events
      .filter((e) => e.ended && e.endReason === PresenceEndReason.CLEARED && e.presence)
      .map((e) => e.presence as Presence),
  );
}

function execGit(cwd: string, ...args: string[]): void {
  execFileSync("git", args, { cwd, env: daemon.env, stdio: "ignore" });
}
