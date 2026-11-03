// Error mapping (test 6): the daemon's detail, the code without one, and transport failures by
// cause, as Connect reports them.

import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { describe, expect, it, vi } from "vitest";

import { classify } from "../../src/client.js";
import {
  AlreadyExistsError,
  ConflictError,
  DaemonNotRunningError,
  InternalError,
  InvalidArgumentError,
  IoError,
  isReefError,
  NotFoundError,
  PermissionDeniedError,
  ReefError,
  RemoteConnectError,
  RemoteMisdirectedError,
  RemoteTlsError,
  UnavailableError,
  UnsupportedError,
} from "../../src/errors.js";
import { ErrorDetailSchema, ErrorKind } from "../../src/gen/reef/api/v1/error_pb.js";
import { RemoteUrl } from "../../src/remote.js";
import { PinMismatch } from "../../src/tls.js";
import type { Target } from "../../src/transport.js";

const LOCAL: Target = { label: "/tmp/d.sock", socket: "/tmp/d.sock", remote: undefined };
const REMOTE: Target = {
  label: "https://vm:7470",
  socket: undefined,
  remote: { url: new RemoteUrl("vm", 7470), token: "t", fingerprint: undefined, name: undefined },
};
const METHOD = "/reef.api.v1.WorkspaceService/GetWorkspace";

function withDetail(kind: ErrorKind, code: Code): ConnectError {
  const detail = create(ErrorDetailSchema, {
    kind,
    reason: "SOME_REASON",
    chain: ["outer", "inner"],
    hint: "Do this.",
    next: ["reef", "ws", "list"],
    metadata: { workspace: "abcd" },
  });
  return new ConnectError("outer: inner", code, undefined, [
    { desc: ErrorDetailSchema, value: detail },
  ]);
}

function nodeError(code: string, message = code): Error {
  return Object.assign(new Error(message), { code });
}

describe("error mapping", () => {
  const kinds: [ErrorKind, new (...args: never[]) => ReefError, string][] = [
    [ErrorKind.NOT_FOUND, NotFoundError, "not_found"],
    [ErrorKind.ALREADY_EXISTS, AlreadyExistsError, "already_exists"],
    [ErrorKind.INVALID_ARGUMENT, InvalidArgumentError, "invalid_argument"],
    [ErrorKind.CONFLICT, ConflictError, "conflict"],
    [ErrorKind.UNAVAILABLE, UnavailableError, "unavailable"],
    [ErrorKind.PERMISSION_DENIED, PermissionDeniedError, "permission_denied"],
    [ErrorKind.IO, IoError, "io"],
    [ErrorKind.INTERNAL, InternalError, "internal"],
    [ErrorKind.UNSUPPORTED, UnsupportedError, "unsupported"],
    [ErrorKind.UNSPECIFIED, InternalError, "internal"],
  ];

  it.each(kinds)("a detail of kind %s", (kind, cls, name) => {
    const mapped = classify(withDetail(kind, Code.FailedPrecondition), LOCAL, METHOD);
    expect(mapped).toBeInstanceOf(cls);
    expect(mapped).toMatchObject({
      kind: name,
      reason: "SOME_REASON",
      chain: ["outer", "inner"],
      hint: "Do this.",
      next: ["reef", "ws", "list"],
      metadata: { workspace: "abcd" },
      code: Code.FailedPrecondition,
      method: METHOD,
      message: "outer: inner",
    });
  });

  it("a code without a detail takes the code's kind", () => {
    expect(classify(new ConnectError("x", Code.NotFound), LOCAL, METHOD)).toBeInstanceOf(
      NotFoundError,
    );
    expect(classify(new ConnectError("x", Code.FailedPrecondition), LOCAL, METHOD)).toBeInstanceOf(
      ConflictError,
    );
    expect(classify(new ConnectError("x", Code.DataLoss), LOCAL, METHOD)).toBeInstanceOf(
      InternalError,
    );
    // A bare Unimplemented is left to the probe.
    expect(classify(new ConnectError("x", Code.Unimplemented), LOCAL, METHOD)).toBeUndefined();
  });

  it("a connection that failed is named by its cause", () => {
    for (const code of ["ENOENT", "ECONNREFUSED"]) {
      const failure = new ConnectError("x", Code.Internal, undefined, undefined, nodeError(code));
      const local = classify(failure, LOCAL, METHOD);
      expect(local).toBeInstanceOf(DaemonNotRunningError);
      expect(local?.next).toEqual(["reef", "daemon", "start"]);
      expect(classify(failure, REMOTE, METHOD)).toBeInstanceOf(RemoteConnectError);
    }
  });

  it("connect-node's own connection timeout is a connection failure", () => {
    const timedOut = new ConnectError("connection establishment timed out", Code.Unavailable);
    expect(classify(timedOut, LOCAL, METHOD)).toBeInstanceOf(DaemonNotRunningError);
    expect(classify(timedOut, REMOTE, METHOD)).toBeInstanceOf(RemoteConnectError);
  });

  it("the listener's 421 is misdirected", () => {
    const mapped = classify(new ConnectError("HTTP 421", Code.Unknown), REMOTE, METHOD);
    expect(mapped).toBeInstanceOf(RemoteMisdirectedError);
    expect((mapped as RemoteMisdirectedError).host).toBe("vm");
  });

  it("a request over writeMaxBytes is too large", () => {
    const failure = new ConnectError(
      "message size 5242885 is larger than configured writeMaxBytes 4194304",
      Code.ResourceExhausted,
    );
    const mapped = classify(failure, LOCAL, METHOD);
    expect(mapped).toBeInstanceOf(InvalidArgumentError);
    expect(mapped?.hint).toMatch(/split the edits/);
  });

  it("a pin refused in a handshake, and TLS failures", () => {
    const pin = new PinMismatch("sha256:" + "a".repeat(64), "sha256:" + "b".repeat(64));
    const pinned = classify(
      new ConnectError("x", Code.Internal, undefined, undefined, pin),
      REMOTE,
      METHOD,
    );
    expect(pinned).toBeInstanceOf(RemoteTlsError);
    expect(pinned).toMatchObject({
      failure: "pin_mismatch",
      presented: pin.presented,
      expected: pin.expected,
    });
    const verify = new ConnectError(
      "x",
      Code.Internal,
      undefined,
      undefined,
      nodeError("SELF_SIGNED_CERT_IN_CHAIN"),
    );
    expect(classify(verify, REMOTE, METHOD, "sha256:x")).toMatchObject({
      failure: "untrusted",
      presented: "sha256:x",
    });
    const ssl = new ConnectError(
      "x",
      Code.Internal,
      undefined,
      undefined,
      nodeError("ERR_SSL_WRONG_VERSION_NUMBER"),
    );
    expect(classify(ssl, REMOTE, METHOD)).toMatchObject({ failure: "handshake" });
  });

  it("recognizes an error from another copy of the module", async () => {
    vi.resetModules();
    const copy = await import("../../src/errors.js");
    const other = new copy.NotFoundError("elsewhere");
    expect(other instanceof NotFoundError).toBe(false);
    expect(isReefError(other)).toBe(true);
    expect(isReefError(new Error("x"))).toBe(false);
    expect(isReefError(undefined)).toBe(false);
  });

  it("keeps its own fields and name", () => {
    const error = new DaemonNotRunningError("/s.sock", "ENOENT");
    expect(error.name).toBe("DaemonNotRunningError");
    expect(error.chain).toEqual(["connect to the daemon at /s.sock", "ENOENT"]);
    expect(error.kind).toBe("unavailable");
    expect(error).toBeInstanceOf(UnavailableError);
  });
});
