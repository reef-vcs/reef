// The incompatible-daemon probe against a fake daemon (test 7).

import { Code, ConnectError, type ConnectRouter } from "@connectrpc/connect";
import { afterEach, describe, expect, it } from "vitest";

import { connect } from "../../src/client.js";
import { IncompatibleError, UnsupportedError } from "../../src/errors.js";
import { VERSION } from "../../src/gen/build.js";
import { DaemonService } from "../../src/gen/reef/api/v1/daemon_pb.js";
import { WorkspaceService } from "../../src/gen/reef/api/v1/workspaces_pb.js";
import { Daemon as V0Daemon } from "../../src/gen/reef/daemon/v0/daemon_pb.js";
import { create } from "@bufbuild/protobuf";
import { ErrorDetailSchema, ErrorKind } from "../../src/gen/reef/api/v1/error_pb.js";
import { type Fake, fakeDaemon } from "./fake.js";

let fake: Fake | undefined;
afterEach(async () => {
  await fake?.close();
  fake = undefined;
});

function notImplemented(): ConnectError {
  const detail = create(ErrorDetailSchema, {
    kind: ErrorKind.UNSUPPORTED,
    reason: "NOT_IMPLEMENTED",
  });
  return new ConnectError("not implemented here", Code.Unimplemented, undefined, [
    { desc: ErrorDetailSchema, value: detail },
  ]);
}

describe("incompatible probe", () => {
  it("names a v1 daemon of another version", async () => {
    let statusCalls = 0;
    fake = await fakeDaemon((router: ConnectRouter) => {
      router.service(DaemonService, {
        getDaemonStatus: () => {
          statusCalls += 1;
          return { version: "9.9.9", apiRevision: 99, activeWorkspaceCount: 3 };
        },
      });
      router.service(WorkspaceService, {
        listWorkspaces: () => {
          throw new ConnectError("unknown", Code.Unimplemented);
        },
      });
    });
    const client = await connect({ socket: fake.socket });
    statusCalls = 0;
    const error: unknown = await client.listWorkspaces({}).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(IncompatibleError);
    expect(error).toMatchObject({
      daemonApi: "v1",
      daemonVersion: "9.9.9",
      activeWorkspaces: 3,
      reason: "API_VERSION_MISMATCH",
    });
    expect(statusCalls).toBe(1);
    await client.close();
  });

  it("names a v0 daemon at connect", async () => {
    let calls = 0;
    fake = await fakeDaemon((router: ConnectRouter) => {
      router.service(DaemonService, {
        getDaemonStatus: () => {
          calls += 1;
          throw new ConnectError("unknown", Code.Unimplemented);
        },
      });
      router.service(V0Daemon, {
        status: () => {
          calls += 1;
          return { version: "0.1.0", activeWorkspaces: 2 };
        },
      });
    });
    const error: unknown = await connect({ socket: fake.socket }).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(IncompatibleError);
    expect(error).toMatchObject({ daemonApi: "v0", daemonVersion: "0.1.0", activeWorkspaces: 2 });
    expect(error).toMatchObject({ next: ["reef", "daemon", "stop"] });
    // The connect call, then at most two probe calls.
    expect(calls).toBeLessThanOrEqual(3);
  });

  it("keeps NOT_IMPLEMENTED from the same version", async () => {
    fake = await fakeDaemon((router: ConnectRouter) => {
      router.service(DaemonService, { getDaemonStatus: () => ({ version: VERSION }) });
      router.service(WorkspaceService, {
        listWorkspaces: () => {
          throw notImplemented();
        },
      });
    });
    const client = await connect({ socket: fake.socket });
    const error: unknown = await client.listWorkspaces({}).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(UnsupportedError);
    expect(error).not.toBeInstanceOf(IncompatibleError);
    expect(error).toMatchObject({ reason: "NOT_IMPLEMENTED" });
    await client.close();
  });

  it("keeps the error when the daemon answers neither status", async () => {
    let answer = true;
    fake = await fakeDaemon((router: ConnectRouter) => {
      router.service(DaemonService, {
        getDaemonStatus: () => {
          if (answer) {
            return { version: VERSION };
          }
          throw new ConnectError("no", Code.Unimplemented);
        },
      });
      router.service(WorkspaceService, {
        listWorkspaces: () => {
          throw new ConnectError("bare", Code.Unimplemented);
        },
      });
    });
    const client = await connect({ socket: fake.socket });
    answer = false;
    const error: unknown = await client.listWorkspaces({}).catch((e: unknown) => e);
    expect(error).toBeInstanceOf(UnsupportedError);
    expect(error).not.toBeInstanceOf(IncompatibleError);
    await client.close();
  });
});
