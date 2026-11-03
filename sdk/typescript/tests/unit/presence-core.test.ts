// Ports of reef_client::presence's tests (R2.11's behaviors 1 to 7) against the reporter's core,
// driven as the Rust reporter's task drives it, on a simulated clock (test 8).

import { durationMs } from "@bufbuild/protobuf/wkt";
import { describe, expect, it } from "vitest";

import type { ErrorKind } from "../../src/errors.js";
import { PresenceKind, type ReportPresenceRequest } from "../../src/gen/reef/api/v1/presence_pb.js";
import { presenceAgent } from "../../src/identity.js";
import {
  BURST,
  BURST_TTL,
  CALL_DEADLINE,
  CLEAR_DEADLINE,
  Core,
  REFRESH,
  REFRESH_EVERY,
  REFRESH_TTL,
  type Cadence,
} from "../../src/presence-core.js";

interface Fail {
  kind: ErrorKind;
  reason?: string;
}
type Answer = "ok" | "hang" | Fail;

class Fake {
  answers: Answer[];
  reports: [number, ReportPresenceRequest][] = [];
  clears: string[] = [];
  ids = 0;
  constructor(answers: Answer[] = []) {
    this.answers = answers;
  }
  nextAnswer(): Answer {
    return this.answers.shift() ?? "ok";
  }
}

/** The reporter's loop on a simulated clock: what reef_client::presence::run does. */
class Sim {
  readonly fake: Fake;
  readonly core: Core;
  now = 0;
  inflightUntil: number | undefined;
  inflightReplacing = false;
  done = false;

  constructor(fake: Fake, cadence: Cadence) {
    this.fake = fake;
    this.core = new Core({ agent: "bot-1", kind: PresenceKind.ACTIVITY, workspace: "ws" }, cadence);
  }

  set(activity: string): void {
    this.core.set({ activity });
  }

  settle(): void {
    this.advance(0);
  }

  advance(seconds: number): void {
    const target = Math.round((this.now + seconds) * 1e6) / 1e6;
    while (!this.done) {
      if (this.inflightUntil !== undefined) {
        if (this.inflightUntil > target) {
          break;
        }
        this.now = this.inflightUntil;
        this.inflightUntil = undefined;
        this.core.failed(this.inflightReplacing, undefined, "");
        continue;
      }
      const action = this.core.next(this.now);
      if (action.type === "wait") {
        if (action.until !== undefined && action.until <= target) {
          this.now = action.until;
          continue;
        }
        break;
      }
      if (action.type === "close") {
        if (action.presenceId !== undefined) {
          this.fake.clears.push(action.presenceId);
        }
        this.done = true;
        break;
      }
      this.fake.reports.push([Math.round(this.now * 1000), action.request]);
      const answer = this.fake.nextAnswer();
      if (answer === "hang") {
        this.inflightUntil = this.now + CALL_DEADLINE;
        this.inflightReplacing = action.replacing;
        continue;
      }
      if (answer !== "ok") {
        this.core.failed(action.replacing, answer.kind, answer.reason ?? "");
        if (this.core.stopped) {
          this.done = true;
        }
        continue;
      }
      let id = action.request.presenceId;
      if (!id) {
        this.fake.ids += 1;
        id = this.fake.ids.toString(16).padStart(16, "0");
      }
      this.core.answered(this.now, id);
    }
    this.now = Math.max(this.now, target);
  }

  /** Closes, as Reporter.clear does; how long it waited, bounded by CLEAR_DEADLINE. */
  clear(): number {
    this.core.close();
    const start = this.now;
    if (this.inflightUntil !== undefined) {
      if (this.inflightUntil - this.now > CLEAR_DEADLINE) {
        this.done = true;
        return CLEAR_DEADLINE;
      }
      this.now = this.inflightUntil;
      this.inflightUntil = undefined;
      this.core.failed(this.inflightReplacing, undefined, "");
    }
    if (!this.done) {
      const action = this.core.next(this.now);
      expect(action.type).toBe("close");
      if (action.type === "close" && action.presenceId !== undefined) {
        this.fake.clears.push(action.presenceId);
        if (this.fake.nextAnswer() === "hang") {
          this.done = true;
          return CLEAR_DEADLINE;
        }
      }
      this.done = true;
    }
    return this.now - start;
  }

  isLive(): boolean {
    return this.core.isLive(this.now);
  }
}

function ttl(request: ReportPresenceRequest): number {
  return request.ttl === undefined ? 0 : durationMs(request.ttl) / 1000;
}

describe("presence core", () => {
  // 1
  it("refresh_reports_replaces_and_clears", () => {
    const fake = new Fake();
    const sim = new Sim(fake, REFRESH);
    sim.advance(5);
    expect(fake.reports).toEqual([]);
    sim.set("running make");
    sim.settle();
    sim.advance(10);
    sim.advance(10);
    expect(fake.reports).toHaveLength(3);
    const [at, first] = fake.reports[0]!;
    expect(at).toBe(5_000);
    expect(first.presenceId).toBe("");
    expect([first.agent, first.workspace, first.kind]).toEqual([
      "bot-1",
      "ws",
      PresenceKind.ACTIVITY,
    ]);
    expect(first.activity).toBe("running make");
    expect(ttl(first)).toBe(REFRESH_TTL);
    expect(fake.reports.slice(1).map(([when, r]) => [when, r.presenceId, r.activity])).toEqual([
      [15_000, "0000000000000001", "running make"],
      [25_000, "0000000000000001", "running make"],
    ]);
    expect(sim.isLive()).toBe(true);
    sim.clear();
    expect(fake.clears).toEqual(["0000000000000001"]);
    sim.advance(60);
    expect(fake.reports).toHaveLength(3);
  });

  // 2
  it("rereports_after_not_found", () => {
    const fake = new Fake(["ok", { kind: "not_found", reason: "PRESENCE_NOT_FOUND" }]);
    const sim = new Sim(fake, REFRESH);
    sim.set("a");
    sim.settle();
    sim.advance(REFRESH_EVERY);
    const reports = fake.reports;
    expect(reports).toHaveLength(3);
    expect(reports[1]![1].presenceId).toBe("0000000000000001");
    expect(reports[2]![0]).toBe(reports[1]![0]);
    expect(reports[2]![1].presenceId).toBe("");
    expect(reports[2]![1].activity).toBe("a");
    sim.advance(REFRESH_EVERY);
    expect(fake.reports[3]![1].presenceId).toBe("0000000000000002");
    sim.clear();
    expect(fake.clears).toEqual(["0000000000000002"]);
  });

  // 3
  it("coalesces_within_a_second", () => {
    const fake = new Fake();
    const sim = new Sim(fake, REFRESH);
    for (let n = 0; n < 10; n++) {
      sim.set(`step ${n}`);
      sim.settle();
      sim.advance(0.03);
    }
    sim.advance(1);
    expect(fake.reports.map(([when, r]) => [when, r.activity])).toEqual([
      [0, "step 0"],
      [1_000, "step 9"],
    ]);
    sim.advance(2);
    sim.set("later");
    sim.settle();
    expect(fake.reports).toHaveLength(3);
    expect([fake.reports[2]![0], fake.reports[2]![1].activity]).toEqual([3_300, "later"]);
  });

  // 4
  it("failures_do_not_stop_it", () => {
    let fake = new Fake([
      { kind: "unavailable" },
      { kind: "unavailable", reason: "TOO_MANY_PRESENCE_ENTRIES" },
      "ok",
    ]);
    let sim = new Sim(fake, REFRESH);
    sim.set("a");
    sim.settle();
    sim.advance(REFRESH_EVERY);
    sim.advance(REFRESH_EVERY);
    expect(fake.reports).toHaveLength(3);
    expect(fake.reports.every(([, r]) => r.presenceId === "")).toBe(true);
    expect(sim.isLive()).toBe(true);

    for (const failure of [
      { kind: "conflict" },
      { kind: "not_found", reason: "WORKSPACE_NOT_FOUND" },
      { kind: "unsupported" },
    ] as Fail[]) {
      fake = new Fake([failure]);
      sim = new Sim(fake, REFRESH);
      sim.set("a");
      sim.settle();
      sim.advance(REFRESH_EVERY * 3);
      sim.set("b");
      sim.settle();
      expect(fake.reports).toHaveLength(1);
      expect(sim.isLive()).toBe(false);
      sim.clear();
      expect(fake.clears).toEqual([]);
    }
  });

  // 4, with an entry held
  it("failures_with_an_entry_held", () => {
    const fake = new Fake(["ok", { kind: "unavailable" }, "ok", { kind: "conflict" }]);
    const sim = new Sim(fake, REFRESH);
    sim.set("a");
    sim.settle();
    for (let n = 0; n < 3; n++) {
      sim.advance(REFRESH_EVERY);
    }
    sim.advance(REFRESH_EVERY * 3);
    expect(fake.reports).toHaveLength(4);
    for (const [, replacement] of fake.reports.slice(1)) {
      expect(replacement.presenceId).toBe("0000000000000001");
    }
    expect(sim.isLive()).toBe(false);
    sim.clear();
    expect(fake.clears).toEqual([]);
  });

  // 4, bursts
  it("burst_drops_a_refused_report", () => {
    const fake = new Fake([{ kind: "unavailable", reason: "TOO_MANY_PRESENCE_ENTRIES" }]);
    const sim = new Sim(fake, BURST);
    sim.set("get_status");
    sim.settle();
    sim.advance(30);
    expect(fake.reports).toHaveLength(1);
    sim.set("list_files");
    sim.settle();
    expect(fake.reports).toHaveLength(2);
    expect(fake.reports[1]![1].presenceId).toBe("");
    expect(fake.reports[1]![1].activity).toBe("list_files");
  });

  // 5
  it("burst_does_not_refresh", () => {
    const fake = new Fake();
    const sim = new Sim(fake, BURST);
    sim.set("get_status");
    sim.settle();
    sim.advance(5);
    sim.set("list_files");
    sim.settle();
    sim.advance(50);
    expect(fake.reports).toHaveLength(2);
    expect(ttl(fake.reports[0]![1])).toBe(BURST_TTL);
    expect(fake.reports[1]![1].presenceId).toBe("0000000000000001");
    expect(sim.isLive()).toBe(true);
    sim.advance(11);
    expect(sim.isLive()).toBe(false);
    expect(fake.reports).toHaveLength(2);
  });

  // 6
  it("clear_is_bounded", () => {
    let fake = new Fake(["ok", "hang"]);
    let sim = new Sim(fake, REFRESH);
    sim.set("a");
    sim.settle();
    expect(sim.clear()).toBeLessThanOrEqual(CLEAR_DEADLINE);
    expect(fake.clears).toEqual(["0000000000000001"]);

    fake = new Fake(["hang"]);
    sim = new Sim(fake, REFRESH);
    sim.set("a");
    sim.settle();
    expect(sim.clear()).toBeLessThanOrEqual(CLEAR_DEADLINE);

    fake = new Fake();
    sim = new Sim(fake, REFRESH);
    sim.clear();
    expect(fake.reports).toEqual([]);
    expect(fake.clears).toEqual([]);

    fake = new Fake();
    sim = new Sim(fake, REFRESH);
    sim.set("a");
    sim.clear();
    expect(fake.reports).toEqual([]);
    expect(fake.clears).toEqual([]);
  });

  // 7
  it("agent_rule", () => {
    expect(presenceAgent("bot", "u@h:1", "u@h")).toBe("bot");
    expect(presenceAgent(undefined, "u@h:1", "u@h")).toBe("u@h:1");
    expect(presenceAgent("  ", "u@h:1", "u@h")).toBe("u@h:1");
    expect(presenceAgent(undefined, undefined, "u@h")).toBe("u@h");
    expect(presenceAgent("", "", "u@h")).toBe("u@h");
    expect(presenceAgent(undefined, "équipe", "u@h")).toBe("u@h");
    expect(presenceAgent(undefined, "x".repeat(300), "u@h")).toBe("u@h");
  });

  it("ignores a set after close", () => {
    const core = new Core({ agent: "a", kind: PresenceKind.ACTIVITY }, REFRESH);
    core.close();
    expect(core.set({ activity: "x" })).toBe(false);
    expect(core.next(0).type).toBe("close");
  });
});
