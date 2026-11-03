// Requests as objects of fields or as messages, and call options (test 13).

import { create, toBinary } from "@bufbuild/protobuf";
import type { ConnectRouter, HandlerContext } from "@connectrpc/connect";
import { afterAll, beforeAll, describe, expect, it } from "vitest";

import { type Client, connect } from "../../src/client.js";
import { InvalidArgumentError, TOO_LARGE_HINT } from "../../src/errors.js";
import { DaemonService } from "../../src/gen/reef/api/v1/daemon_pb.js";
import {
  type CreateWorkspaceRequest,
  CreateWorkspaceRequestSchema,
  WorkspaceService,
} from "../../src/gen/reef/api/v1/workspaces_pb.js";
import { type Fake, fakeDaemon } from "./fake.js";

let fake: Fake;
let client: Client;
const seen: {
  request: CreateWorkspaceRequest;
  timeoutMs: number | undefined;
  agent: string | null;
}[] = [];

beforeAll(async () => {
  fake = await fakeDaemon((router: ConnectRouter) => {
    router.service(DaemonService, { getDaemonStatus: () => ({ version: "x" }) });
    router.service(WorkspaceService, {
      createWorkspace: (request: CreateWorkspaceRequest, context: HandlerContext) => {
        seen.push({
          request,
          timeoutMs: context.timeoutMs(),
          agent: context.requestHeader.get("reef-agent"),
        });
        return {};
      },
      listWorkspaces: async (_request, context: HandlerContext) => {
        await new Promise((done) => setTimeout(done, 2_000));
        context.signal.throwIfAborted();
        return {};
      },
    });
  });
  client = await connect({ socket: fake.socket, agent: "req-test" });
});

afterAll(async () => {
  await client.close();
  await fake.close();
});

describe("requests", () => {
  it("sends the same bytes for an object of fields and a message", async () => {
    seen.length = 0;
    const fields = { repo: "r", owner: "o", start: { case: "newOn" as const, value: "trunk()" } };
    await client.createWorkspace(fields);
    await client.createWorkspace(create(CreateWorkspaceRequestSchema, fields));
    const [a, b] = seen;
    expect(a && toBinary(CreateWorkspaceRequestSchema, a.request)).toEqual(
      b && toBinary(CreateWorkspaceRequestSchema, b.request),
    );
    expect(a?.request.start).toEqual({ case: "newOn", value: "trunk()" });
    expect(a?.agent).toBe("req-test");
  });

  it("passes timeoutMs and signal to the call", async () => {
    seen.length = 0;
    await client.createWorkspace({ repo: "r" }, { timeoutMs: 4_321 });
    expect(seen[0]?.timeoutMs).toBeGreaterThan(4_000);
    expect(seen[0]?.timeoutMs).toBeLessThanOrEqual(4_321);

    const controller = new AbortController();
    const call = client.listWorkspaces({}, { signal: controller.signal });
    controller.abort();
    await expect(call).rejects.toMatchObject({ code: 1 });

    await expect(client.listWorkspaces({}, { timeoutMs: 100 })).rejects.toMatchObject({ code: 4 });
  });

  it("refuses a request over 4 MiB before sending it, with the size hint", async () => {
    seen.length = 0;
    const error: unknown = await client
      .createWorkspace({ repo: "r", purpose: "x".repeat(5 << 20) })
      .catch((e: unknown) => e);
    expect(error).toBeInstanceOf(InvalidArgumentError);
    expect((error as InvalidArgumentError).hint).toBe(TOO_LARGE_HINT);
    expect(seen).toHaveLength(0);
  });

  it("withOptions shares the connection with another agent", async () => {
    seen.length = 0;
    const other = client.withOptions({ agent: "other" });
    await other.createWorkspace({});
    expect(seen[0]?.agent).toBe("other");
    const none = client.withOptions({ agent: undefined });
    await none.createWorkspace({});
    expect(seen[1]?.agent).toBeNull();
    expect(other.target).toBe(client.target);
  });
});
