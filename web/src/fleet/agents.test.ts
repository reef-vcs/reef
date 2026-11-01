// R2.20 test 2: one join of client and daemon presence entries, so an agent
// and its workspace's owner count once.
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { describe, expect, it } from "vitest";
import { PresenceKind, PresenceSchema, type Presence } from "../gen/reef/api/v1/presence_pb";
import { agentCounts, agentsOfWorkspace, joinAgents } from "./agents";

const REPO = "a3dda866c7e8f343";

function entry(init: MessageInitShape<typeof PresenceSchema> & { id: string }): Presence {
  return create(PresenceSchema, { repoId: REPO, ...init });
}

describe("joinAgents", () => {
  it("gives a named client and the daemon's entries on its workspace one agent", () => {
    const { agents, byEntry } = joinAgents(
      [
        entry({ id: "a", agent: "reviewer", workspaceId: "w1", kind: PresenceKind.ACTIVITY }),
        entry({
          id: "l",
          agent: "ann@host:42",
          workspaceId: "w1",
          byDaemon: true,
          kind: PresenceKind.LEASE,
        }),
        entry({
          id: "e",
          agent: "ann@host:42",
          workspaceId: "w1",
          byDaemon: true,
          kind: PresenceKind.EDITING,
        }),
      ],
      REPO,
    );
    expect([...agents.keys()]).toEqual(["reviewer"]);
    expect(agents.get("reviewer")!.entries.map((p) => p.id)).toEqual(["a", "l", "e"]);
    expect(byEntry.get("l")).toEqual(["reviewer"]);
  });

  it("gives daemon entries alone to the owner they name", () => {
    const { agents } = joinAgents(
      [entry({ id: "l", agent: "ann@host:42", workspaceId: "w1", byDaemon: true })],
      REPO,
    );
    expect([...agents.keys()]).toEqual(["ann@host:42"]);
    expect(agents.get("ann@host:42")!.workspaceIds).toEqual(["w1"]);
  });

  it("gives every client agent on a workspace its daemon entries", () => {
    const join = joinAgents(
      [
        entry({ id: "a", agent: "ann", workspaceId: "w1" }),
        entry({ id: "b", agent: "bob", workspaceId: "w1" }),
        entry({ id: "l", agent: "owner", workspaceId: "w1", byDaemon: true }),
      ],
      REPO,
    );
    expect([...join.agents.keys()]).toEqual(["ann", "bob"]);
    expect(join.byEntry.get("l")).toEqual(["ann", "bob"]);
    expect(agentsOfWorkspace(join, "w1")).toEqual(["ann", "bob"]);
  });

  it("keeps an entry with only a repository, without a workspace", () => {
    const { agents } = joinAgents([entry({ id: "r", agent: "planner", changeId: "abcd" })], REPO);
    expect(agents.get("planner")!.workspaceIds).toEqual([]);
    expect(agents.get("planner")!.changeIds).toEqual(["abcd"]);
  });

  it("leaves out other repositories' entries and entries with no repository", () => {
    const { agents } = joinAgents(
      [
        entry({ id: "x", agent: "other", repoId: "ffff" }),
        entry({ id: "y", agent: "none", repoId: "" }),
      ],
      REPO,
    );
    expect(agents.size).toBe(0);
  });

  it("counts each repository's agents once", () => {
    const counts = agentCounts([
      entry({ id: "a", agent: "reviewer", workspaceId: "w1" }),
      entry({ id: "l", agent: "owner", workspaceId: "w1", byDaemon: true }),
      entry({ id: "o", agent: "other", repoId: "ffff" }),
    ]);
    expect(counts.get(REPO)).toBe(1);
    expect(counts.get("ffff")).toBe(1);
  });
});
