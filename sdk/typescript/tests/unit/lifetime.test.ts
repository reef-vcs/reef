// A script that never calls close() still exits, and an awaited call keeps it alive (test 14).
// The child runs the built package (dist/), which `npm test` and `just typescript-check` build
// first; run alone without a build, it is skipped.

import { execFile } from "node:child_process";
import { existsSync } from "node:fs";
import { join, resolve } from "node:path";
import { pathToFileURL } from "node:url";

import type { ConnectRouter } from "@connectrpc/connect";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { DaemonService } from "../../src/gen/reef/api/v1/daemon_pb.js";
import { WorkspaceService } from "../../src/gen/reef/api/v1/workspaces_pb.js";
import { type Fake, fakeDaemon } from "./fake.js";

const DIST = join(resolve(import.meta.dirname, "..", ".."), "dist", "index.js");

let fake: Fake;
beforeAll(async () => {
  fake = await fakeDaemon((router: ConnectRouter) => {
    router.service(DaemonService, { getDaemonStatus: () => ({ version: "x" }) });
    router.service(WorkspaceService, {
      listWorkspaces: async () => {
        await new Promise((done) => setTimeout(done, 1_000));
        return { nextPageToken: "late" };
      },
    });
  });
});
afterAll(async () => {
  await fake.close();
});

function run(script: string): Promise<{ code: number | null; out: string; ms: number }> {
  const started = Date.now();
  return new Promise((done) => {
    const child = execFile(
      process.execPath,
      ["--input-type=module", "-e", script],
      {
        timeout: 10_000,
        env: { ...process.env, SOCKET: fake.socket, SDK: pathToFileURL(DIST).href },
      },
      (error, stdout) => {
        done({ code: error ? (child.exitCode ?? -1) : 0, out: stdout, ms: Date.now() - started });
      },
    );
  });
}

describe.skipIf(!existsSync(DIST))("process lifetime", () => {
  it("exits without close()", async () => {
    const result = await run(
      'const { connect } = await import(process.env.SDK); const c = await connect({ socket: process.env.SOCKET }); await c.getDaemonStatus({}); console.log("done");',
    );
    expect(result.out.trim()).toBe("done");
    expect(result.code).toBe(0);
    expect(result.ms).toBeLessThan(3_000);
  });

  it("stays alive for an awaited call", async () => {
    const result = await run(
      "const { connect } = await import(process.env.SDK); const c = await connect({ socket: process.env.SOCKET }); const r = await c.listWorkspaces({}); console.log(r.nextPageToken);",
    );
    expect(result.out.trim()).toBe("late");
    expect(result.code).toBe(0);
  });
});
