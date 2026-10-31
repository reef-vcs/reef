// W8: `reefError` decodes the daemon's `ErrorDetail`, including after it has
// crossed a transport as `grpc-status-details-bin`.
import { Code, ConnectError, createClient, createRouterTransport } from "@connectrpc/connect";
import { describe, expect, it } from "vitest";
import { DaemonService } from "../gen/reef/api/v1/daemon_pb";
import { ErrorKind } from "../gen/reef/api/v1/error_pb";
import { daemonError } from "../test/fakes";
import { isSessionInvalid, reefError } from "./errors";

const detail = {
  kind: ErrorKind.PERMISSION_DENIED,
  reason: "WEB_SESSION_INVALID",
  hint: "Run `reef ui` in a terminal.",
  next: ["reef ui"],
  metadata: { method: "/reef.api.v1.DaemonService/GetDaemonStatus" },
};

describe("reefError", () => {
  it("decodes an ErrorDetail from a ConnectError's details", () => {
    const error = reefError(daemonError(Code.PermissionDenied, detail, "session ended"));
    expect(error).toEqual({
      code: Code.PermissionDenied,
      kind: "permission_denied",
      reason: "WEB_SESSION_INVALID",
      message: "session ended",
      hint: "Run `reef ui` in a terminal.",
      next: ["reef ui"],
      metadata: { method: "/reef.api.v1.DaemonService/GetDaemonStatus" },
    });
    expect(isSessionInvalid(error)).toBe(true);
  });

  it("decodes it after a round trip through a transport", async () => {
    const transport = createRouterTransport(({ service }) => {
      service(DaemonService, {
        getDaemonStatus() {
          throw daemonError(Code.PermissionDenied, detail, "session ended");
        },
      });
    });
    const client = createClient(DaemonService, transport);
    const thrown = await client.getDaemonStatus({}).catch((e: unknown) => e);
    const error = reefError(thrown);
    expect(error.reason).toBe("WEB_SESSION_INVALID");
    expect(error.next).toEqual(["reef ui"]);
    expect(error.kind).toBe("permission_denied");
  });

  it("has empty fields without a detail", () => {
    const error = reefError(new ConnectError("plain", Code.Unavailable));
    expect(error).toMatchObject({
      code: Code.Unavailable,
      kind: "",
      reason: "",
      hint: "",
      next: [],
    });
    expect(isSessionInvalid(error)).toBe(false);
  });

  it("wraps a non-Connect error", () => {
    const error = reefError(new TypeError("Failed to fetch"));
    expect(error.code).toBe(Code.Unknown);
    expect(error.message).toBe("Failed to fetch");
  });
});
