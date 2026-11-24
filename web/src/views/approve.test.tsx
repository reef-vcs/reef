// The approval page (R3.43 decision 4) over an in-memory daemon and a fake
// authenticator: what it names before the button, that it approves nothing
// on load, Approve and Decline, expiry, and the ends it can meet.
import { Code } from "@connectrpc/connect";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { afterEach, describe, expect, it, vi } from "vitest";
import { DEVICE_READS } from "../device";
import { equal } from "../sign/bytes";
import { daemonError, settle } from "../test/fakes";
import { paired } from "../test/paired";
import { IdentityDaemon, passkey, REPO, TREE } from "../test/identity-daemon";
import { act, cleanup, render, screen, waitFor } from "../test/testing-library";
import { installAuthenticator, removeAuthenticator } from "../test/webauthn";
import { Approve, timeLeft } from "./Approve";

afterEach(() => {
  cleanup();
  removeAuthenticator();
  vi.useRealTimers();
});

async function flush() {
  await act(async () => {
    await settle(30);
  });
}

async function setup(daemon: IdentityDaemon, id: string) {
  render(<Approve connection={daemon.connection()} id={id} onSessionInvalid={vi.fn()} />);
  await flush();
}

function daemonWithPasskey() {
  const daemon = new IdentityDaemon();
  daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
  return daemon;
}

function button(name: string): HTMLButtonElement {
  return screen.getByRole("button", { name });
}

function fact(term: string): string {
  const dt = screen.getByText(term, { selector: "dt" });
  return dt.nextElementSibling?.textContent ?? "";
}

describe("the approval page", () => {
  it("on a paired phone, neither approves nor declines", async () => {
    paired();
    installAuthenticator();
    const daemon = daemonWithPasskey();
    const approval = await daemon.pending();
    await setup(daemon, approval.id);
    expect(button("Approve").disabled).toBe(true);
    expect(button("Decline").disabled).toBe(true);
    expect(screen.getByText(DEVICE_READS)).toBeTruthy();
  });

  it("names the identity, the sign-off, the change, its tree, who asked and the time left", async () => {
    const daemon = daemonWithPasskey();
    const authenticator = installAuthenticator();
    const approval = await daemon.pending();
    await setup(daemon, approval.id);
    expect(fact("Signs as")).toBe("arsood");
    expect(fact("Sign-off")).toBe("signoff, pass, the whole tree");
    expect(fact("Change")).toContain("Fix the lease renewal race");
    expect(fact("Author")).toBe("Ada Agent <ada@example.com>");
    expect(fact("Repository")).toBe(REPO);
    expect(fact("Tree")).toContain(TREE);
    expect(fact("Tree")).toContain("changes below this one that have not landed");
    expect(screen.queryByText("Also covers"), "no line without unlanded changes below").toBeNull();
    expect(fact("Asked by")).toBe(
      "a command that says it is claude@mac, in workspace 6ce393d29a64e06c",
    );
    expect(screen.getByRole("timer").textContent).toMatch(/^2 min left, until \d\d:\d\d:\d\d$/);
    expect(button("Approve").disabled).toBe(false);
    // Nothing is approved on load.
    expect(authenticator.gets).toEqual([]);
    expect(daemon.resolved).toEqual([]);
  });

  it("approves with one touch over the approval's challenge", async () => {
    const daemon = daemonWithPasskey();
    const authenticator = installAuthenticator();
    const approval = await daemon.pending();
    await setup(daemon, approval.id);
    await act(() => {
      button("Approve").click();
    });
    expect(await screen.findByRole("status")).toBeTruthy();
    expect(screen.getByRole("status").textContent).toMatch(
      /^Approved with the passkey “MacBook Touch ID”\. The sign-off is added by the command that asked\.$/,
    );
    const get = authenticator.gets[0];
    expect(equal(new Uint8Array(get?.challenge as ArrayBuffer), approval.challenge)).toBe(true);
    expect(get?.rpId).toBe("localhost");
    expect(get?.allowCredentials?.length).toBe(1);
    expect(daemon.resolved.map((r) => r.resolution.case)).toEqual(["approve"]);
    expect(screen.queryByRole("button", { name: "Approve" })).toBeNull();
  });

  it("declines without a touch", async () => {
    const daemon = daemonWithPasskey();
    const authenticator = installAuthenticator();
    const approval = await daemon.pending();
    await setup(daemon, approval.id);
    await act(() => {
      button("Decline").click();
    });
    expect((await screen.findByRole("status")).textContent).toBe("Declined. Nothing was approved.");
    expect(authenticator.gets).toEqual([]);
    expect(daemon.resolved.map((r) => r.resolution.case)).toEqual(["decline"]);
  });

  it("refuses to ask for a touch when the daemon's challenge is not the approval's", async () => {
    const daemon = daemonWithPasskey();
    const authenticator = installAuthenticator();
    const approval = await daemon.pending();
    approval.changeId = "0".repeat(32);
    await setup(daemon, approval.id);
    await act(() => {
      button("Approve").click();
    });
    await waitFor(() => {
      expect(screen.getByRole("alert").textContent).toContain("Nothing was approved.");
    });
    expect(authenticator.gets).toEqual([]);
    expect(daemon.resolved).toEqual([]);
  });

  it("shows a refused assertion's reason, and stays open for another try", async () => {
    const daemon = daemonWithPasskey();
    installAuthenticator();
    daemon.fail.resolveApproval = daemonError(
      Code.InvalidArgument,
      { reason: "PASSKEY_ASSERTION_INVALID", hint: "Use a passkey that verifies you." },
      "the passkey assertion is invalid: flags",
    );
    const approval = await daemon.pending();
    await setup(daemon, approval.id);
    await act(() => {
      button("Approve").click();
    });
    const alert = await screen.findByRole("alert");
    expect(alert.textContent).toContain("the passkey assertion is invalid: flags");
    expect(alert.textContent).toContain("Use a passkey that verifies you.");
    expect(button("Approve").disabled).toBe(false);
  });

  it("closes when the time runs out, and says so when the daemon expires it", async () => {
    vi.useFakeTimers({ toFake: ["Date", "setTimeout", "clearTimeout"] });
    const daemon = daemonWithPasskey();
    installAuthenticator();
    const approval = await daemon.pending({ expiresAt: timestampFromMs(Date.now() + 5_000) });
    await setup(daemon, approval.id);
    expect(screen.getByRole("timer").textContent).toMatch(/^5 s left/);
    for (let second = 0; second < 6; second++) {
      await act(() => {
        vi.advanceTimersByTime(1_000);
      });
    }
    expect(screen.getByRole("timer").textContent).toBe("Its time is over.");
    expect(button("Approve").disabled).toBe(true);
    expect(button("Decline").disabled).toBe(true);
    daemon.expire(approval.id);
    await flush();
    expect(screen.getByRole("status").textContent).toBe(
      "This approval expired. Nothing was approved.",
    );
  });

  it("says when the daemon holds no such approval", async () => {
    const daemon = new IdentityDaemon();
    daemon.fail.getApproval = daemonError(Code.NotFound, { reason: "APPROVAL_NOT_FOUND" });
    await setup(daemon, "apr_0000000000000bad");
    expect(screen.getByRole("heading", { name: "No such approval" })).toBeTruthy();
    expect(screen.queryByRole("button")).toBeNull();
  });

  it("says who asked from the web UI", async () => {
    const daemon = daemonWithPasskey();
    const approval = await daemon.pending({ requestedThrough: "web", requestedByAgent: "" });
    await setup(daemon, approval.id);
    expect(fact("Asked by")).toBe("a web UI session");
  });
});

describe("timeLeft", () => {
  it("counts down in minutes and seconds", () => {
    expect(timeLeft(120_000)).toBe("2 min");
    expect(timeLeft(100_400)).toBe("1 min 41 s");
    expect(timeLeft(9_001)).toBe("10 s");
    expect(timeLeft(-5)).toBe("0 s");
  });
  it("names the changes below that have not landed, which the tree holds too", async () => {
    const daemon = daemonWithPasskey();
    daemon.below = ["Add the payload\n\nbody", ""];
    installAuthenticator();
    const approval = await daemon.pending();
    await setup(daemon, approval.id);
    const covers = fact("Also covers");
    expect(covers).toContain("2 changes below that have not landed");
    expect(covers).toContain("Add the payload");
    expect(covers).toContain("(no description)");
  });
});
