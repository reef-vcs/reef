// The agent name and a workspace's owner (test 10).

import { userInfo } from "node:os";

import { describe, expect, it } from "vitest";

import { InvalidArgumentError } from "../../src/errors.js";
import { agentFrom, checkAgent, defaultOwner, processOwner } from "../../src/identity.js";
import { mayAttach } from "../../src/workspace.js";

describe("agent and owner", () => {
  it("checks an agent as the daemon would", () => {
    expect(checkAgent("  bot-1 ")).toBe("bot-1");
    expect(checkAgent("")).toBeUndefined();
    expect(checkAgent("   ")).toBeUndefined();
    expect(checkAgent("a b")).toBe("a b");
    expect(checkAgent("x".repeat(256))).toBe("x".repeat(256));
    for (const bad of ["x".repeat(257), "a\u0007b", "équipe"]) {
      let thrown: unknown;
      try {
        checkAgent(bad);
      } catch (error) {
        thrown = error;
      }
      expect(thrown).toBeInstanceOf(InvalidArgumentError);
      expect((thrown as InvalidArgumentError).reason).toBe("INVALID_AGENT");
    }
  });

  it("reads REEF_AGENT without an option", () => {
    const saved = process.env["REEF_AGENT"];
    try {
      process.env["REEF_AGENT"] = " from-env ";
      expect(agentFrom(undefined)).toBe("from-env");
      expect(agentFrom("given")).toBe("given");
      delete process.env["REEF_AGENT"];
      expect(agentFrom(undefined)).toBeUndefined();
    } finally {
      if (saved === undefined) {
        delete process.env["REEF_AGENT"];
      } else {
        process.env["REEF_AGENT"] = saved;
      }
    }
  });

  it("owns a workspace as <user>@<host>:<pid>", () => {
    expect(defaultOwner().startsWith(`${userInfo().username}@`)).toBe(true);
    expect(processOwner()).toBe(`${defaultOwner()}:${process.pid}`);
  });

  it("attaches under ws run --workspace's rule", () => {
    expect(mayAttach("u@h", undefined, "u@h")).toBe(true);
    expect(mayAttach("u@h:42", undefined, "u@h")).toBe(true);
    expect(mayAttach("u@hx", undefined, "u@h")).toBe(false);
    expect(mayAttach("other@h", undefined, "u@h")).toBe(false);
    expect(mayAttach("other@h", "other@h", "u@h")).toBe(true);
    expect(mayAttach("other@h:1", "other@h", "u@h")).toBe(false);
  });
});
