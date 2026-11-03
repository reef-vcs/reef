// The lease heartbeater's schedule and notes, as `reef ws run`'s (R1.21) (test 9).

import { create } from "@bufbuild/protobuf";
import { durationFromMs, timestampFromMs } from "@bufbuild/protobuf/wkt";
import { describe, expect, it } from "vitest";

import { LeaseSchema, type Lease } from "../../src/gen/reef/api/v1/workspaces_pb.js";
import { LeaseCore, leaseGone } from "../../src/lease-core.js";

const NOW = Date.UTC(2026, 10, 2, 12, 0);

function lease(every = 20, expiresIn = 60, grace = 300): Lease {
  return create(LeaseSchema, {
    id: "l1",
    heartbeatEvery: durationFromMs(every * 1000),
    grace: durationFromMs(grace * 1000),
    expiresAt: timestampFromMs(NOW + expiresIn * 1000),
  });
}

describe("lease core", () => {
  it("renews, backs off, and notes each change once", () => {
    const core = new LeaseCore("ws1", lease());
    expect(core.nextDelay()).toBe(20);

    const notes = core.unreachable("refused", NOW);
    expect(notes).toHaveLength(1);
    expect(notes[0]?.message).toContain("unreachable (refused)");
    expect(notes[0]?.code).toBe("REEF_LEASE_LINK_LOST");
    const delays = [core.nextDelay()];
    for (let n = 0; n < 6; n++) {
      expect(core.unreachable("refused", NOW)).toEqual([]);
      delays.push(core.nextDelay());
    }
    expect(delays).toEqual([1, 2, 4, 8, 16, 30, 30]);

    const late = NOW + 361_000;
    const lapsed = core.unreachable("refused", late);
    expect(lapsed).toHaveLength(1);
    expect(lapsed[0]?.message).toContain("has expired");
    expect(lapsed[0]?.code).toBe("REEF_LEASE_LAPSED");
    expect(core.unreachable("refused", late)).toEqual([]);

    expect(core.renewed(lease(10, 600)).map((n) => n.message)).toEqual([
      "reef: daemon reachable again; lease renewed",
    ]);
    expect(core.nextDelay()).toBe(10);
    expect(core.renewed(lease())).toEqual([]);
  });

  it("needs the lease's cadence", () => {
    expect(() => new LeaseCore("ws1", create(LeaseSchema, { id: "l1" }))).toThrow(/heartbeatEvery/);
  });

  it("knows when the lease is gone", () => {
    expect(leaseGone("conflict")).toBe(true);
    expect(leaseGone("not_found")).toBe(true);
    expect(leaseGone("internal")).toBe(false);
    expect(leaseGone("unavailable")).toBe(false);
  });

  it("changes nothing on an answer without a lease", () => {
    const core = new LeaseCore("ws1", lease());
    core.unreachable("refused", NOW);
    expect(core.renewed(undefined)).toEqual([]);
    expect(core.up).toBe(false);
  });
});
