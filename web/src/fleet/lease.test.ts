// R2.20 test 3: lease health from events and reads, never the clock; a lease
// never renewed is not stuck.
import { create } from "@bufbuild/protobuf";
import { durationFromMs, timestampFromMs } from "@bufbuild/protobuf/wkt";
import { afterEach, describe, expect, it, vi } from "vitest";
import { LeaseSchema, LeaseState, type Lease } from "../gen/reef/api/v1/workspaces_pb";
import { expiry, foldLease, leaseHealth, renewed, restartWindowMs } from "./lease";

const T = Date.UTC(2026, 10, 1, 12, 0, 0);

function lease(init: {
  issued?: number;
  heartbeat?: number;
  expires?: number;
  state?: LeaseState;
  everyMs?: number;
  graceMs?: number;
}): Lease {
  return create(LeaseSchema, {
    id: "l1",
    owner: "ann",
    issuedAt: timestampFromMs(init.issued ?? T),
    lastHeartbeatAt: timestampFromMs(init.heartbeat ?? init.issued ?? T),
    expiresAt: timestampFromMs(init.expires ?? T + 3_600_000),
    state: init.state ?? LeaseState.ACTIVE,
    heartbeatEvery: durationFromMs(init.everyMs ?? 60_000),
    grace: durationFromMs(init.graceMs ?? 300_000),
  });
}

afterEach(() => {
  vi.restoreAllMocks();
});

describe("foldLease", () => {
  it("keeps the later expiry from either source, and a tie keeps the listed one", () => {
    const listed = lease({ expires: T + 1000 });
    const later = lease({ expires: T + 2000, heartbeat: T + 500 });
    expect(foldLease(listed, [later])).toBe(later);
    expect(foldLease(later, [listed])).toBe(later);
    const same = lease({ expires: T + 1000 });
    expect(foldLease(listed, [same])).toBe(listed);
    expect(foldLease(undefined, [same])).toBe(same);
    expect(foldLease(undefined, [])).toBeUndefined();
  });
});

describe("leaseHealth", () => {
  const renewedLease = lease({ heartbeat: T + 60_000 });
  const fresh = lease({});

  it("reads heartbeating while the daemon's lease entry lives", () => {
    const health = leaseHealth({
      lease: fresh,
      heartbeating: true,
      synced: true,
      restarting: false,
    });
    expect(health.word).toBe("heartbeating");
    expect(health.title).toMatch(/every 60 s$/);
  });

  it("reads not heartbeating for a lease never renewed, inside the restart window or not", () => {
    expect(renewed(fresh)).toBe(false);
    for (const restarting of [false, true]) {
      const health = leaseHealth({ lease: fresh, heartbeating: false, synced: true, restarting });
      expect(health).toMatchObject({ word: "not heartbeating", stuck: false });
    }
  });

  it("reads missed heartbeats for a renewed lease without an entry, and waits after a restart", () => {
    expect(renewed(renewedLease)).toBe(true);
    expect(
      leaseHealth({ lease: renewedLease, heartbeating: false, synced: true, restarting: false }),
    ).toMatchObject({ word: "missed heartbeats", stuck: true });
    expect(
      leaseHealth({ lease: renewedLease, heartbeating: false, synced: true, restarting: true }),
    ).toMatchObject({ word: "waiting for a heartbeat", stuck: false });
  });

  it("reads - while the presence set is not synced", () => {
    expect(
      leaseHealth({ lease: renewedLease, heartbeating: false, synced: false, restarting: false })
        .word,
    ).toBe("-");
  });

  it("never reads the clock", () => {
    vi.spyOn(Date, "now").mockImplementation(() => {
      throw new Error("the clock was read");
    });
    leaseHealth({ lease: renewedLease, heartbeating: false, synced: true, restarting: false });
    leaseHealth({ lease: fresh, heartbeating: true, synced: true, restarting: false });
    expiry(lease({ state: LeaseState.IN_GRACE }));
    expiry(lease({ state: LeaseState.EXPIRED }));
    expiry(fresh);
    restartWindowMs(1000, [fresh]);
  });
});

describe("expiry", () => {
  it("reads the reported state, never predicting it", () => {
    expect(expiry(lease({})).text).toMatch(/^expires \d\d:\d\d:\d\d$/);
    // Grace ends at expiry plus grace.
    const grace = expiry(lease({ state: LeaseState.IN_GRACE, expires: T, graceMs: 300_000 }));
    expect(grace.text).toMatch(/^in grace until \d\d:\d\d:\d\d$/);
    expect(grace.title).toBe(new Date(T + 300_000).toISOString());
    expect(expiry(lease({ state: LeaseState.EXPIRED })).text).toBe("expired, to be reclaimed");
  });

  it("gives the date when the expiry is not on the last heartbeat's day", () => {
    expect(expiry(lease({ expires: T + 86_400_000 })).text).toMatch(
      /^expires \d{4}-\d\d-\d\d \d\d:\d\d$/,
    );
  });
});

describe("restartWindowMs", () => {
  it("is two of the largest heartbeat intervals less the daemon's uptime", () => {
    const leases = [lease({ everyMs: 5000 }), lease({ everyMs: 60_000 })];
    expect(restartWindowMs(10_000, leases)).toBe(110_000);
    expect(restartWindowMs(200_000, leases)).toBe(0);
    expect(restartWindowMs(0, [])).toBe(0);
  });
});
