// Sign off on the change detail panel (R3.43 decision 3), over an in-memory
// daemon and a fake authenticator: disabled without a passkey, cancelled,
// approved and added, and refused with the daemon's hint.
import { create } from "@bufbuild/protobuf";
import { Code } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import { ChangeSchema } from "../gen/reef/api/v1/changes_pb";
import { EvidenceResult, EvidenceSchema } from "../gen/reef/api/v1/evidence_pb";
import { IdentityStatus, SessionPresence } from "../gen/reef/api/v1/identity_pb";
import { equal, sha256 } from "../sign/bytes";
import { daemonError, settle } from "../test/fakes";
import { CHANGE, COMMIT, IdentityDaemon, passkey, REPO, TREE } from "../test/identity-daemon";
import { act, cleanup, render, screen, waitFor } from "../test/testing-library";
import {
  installAuthenticator,
  removeAuthenticator,
  type FakeAuthenticator,
} from "../test/webauthn";
import { SignOff } from "./SignOff";

afterEach(() => {
  cleanup();
  removeAuthenticator();
});

const change = create(ChangeSchema, { changeId: CHANGE, commitId: COMMIT, treeId: TREE });

async function flush() {
  await act(async () => {
    await settle(30);
  });
}

async function setup(daemon: IdentityDaemon) {
  render(
    <SignOff
      connection={daemon.connection()}
      repo={REPO}
      change={change}
      onSessionInvalid={vi.fn()}
    />,
  );
  await flush();
}

function signOffButton(): HTMLButtonElement {
  return screen.getByRole("button", { name: "Sign off" });
}

/** Clicks Sign off and waits until the attempt ends: its outcome is on the panel. */
async function signOff() {
  await act(() => {
    signOffButton().click();
  });
  await waitFor(() => {
    expect(screen.queryByRole("status") ?? screen.queryByRole("alert")).not.toBeNull();
  });
  await flush();
}

function withPasskey(): { daemon: IdentityDaemon; authenticator: FakeAuthenticator } {
  const daemon = new IdentityDaemon();
  daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
  return { daemon, authenticator: installAuthenticator() };
}

describe("Sign off", () => {
  it("is disabled, with the reason and a link to add one, when the identity has no passkey", async () => {
    installAuthenticator();
    const daemon = new IdentityDaemon();
    await setup(daemon);
    expect(signOffButton().disabled).toBe(true);
    expect(screen.getByText(/arsood has no passkey/)).toBeTruthy();
    expect(screen.getByRole("link", { name: "Add one" }).getAttribute("href")).toBe("/passkeys");
    expect(screen.getByText("No sign-offs yet.")).toBeTruthy();
  });

  it("is disabled in a browser without passkeys", async () => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
    await setup(daemon);
    expect(signOffButton().disabled).toBe(true);
    expect(screen.getByText("This browser has no passkey support.")).toBeTruthy();
  });

  it("signs off: one approval for this statement and change, one touch, the same bytes added", async () => {
    const { daemon, authenticator } = withPasskey();
    await setup(daemon);
    await signOff();

    const [request] = daemon.requested;
    expect(request?.revision).toBe(COMMIT);
    expect(request?.identity).toBe("");
    const statement = JSON.parse(new TextDecoder().decode(request?.statement)) as {
      subject: { digest: { gitTree: string } }[];
      predicate: { kind: string; result: string };
    };
    expect(statement.subject[0]?.digest.gitTree).toBe(TREE);
    expect(statement.predicate).toMatchObject({ kind: "signoff", result: "pass" });

    // The touch signs the approval's own challenge, by one of the identity's passkeys.
    const approval = daemon.approvals.get("apr_0000000000000001");
    const get = authenticator.gets[0];
    expect(get?.userVerification).toBe("required");
    expect(
      equal(new Uint8Array(get?.challenge as ArrayBuffer), approval?.challenge ?? new Uint8Array()),
    ).toBe(true);
    expect(daemon.resolved[0]?.resolution.case).toBe("approve");

    // The evidence is that statement, signed by the approved key, for the approved commit.
    const [added] = daemon.added;
    const envelope = JSON.parse(new TextDecoder().decode(added?.envelope)) as {
      payload: string;
      signatures: { keyid: string }[];
    };
    expect(atob(envelope.payload)).toBe(new TextDecoder().decode(request?.statement));
    expect(added?.revision).toBe(COMMIT);
    expect(
      equal(added?.publicKey ?? new Uint8Array(), request?.publicKey ?? new Uint8Array()),
    ).toBe(true);
    const keyId = Array.from(await sha256(request?.publicKey ?? new Uint8Array()), (b) =>
      b.toString(16).padStart(2, "0"),
    ).join("");
    expect(envelope.signatures[0]?.keyid).toBe(keyId);

    expect(screen.getByRole("status").textContent).toBe(
      "Signed off: user verified, passkey “MacBook Touch ID”.",
    );
    expect(
      await screen.findByText(/arsood · pass · user verified, passkey “MacBook Touch ID”/),
    ).toBeTruthy();
  });

  it("says Cancelled when the person dismisses the passkey prompt, and adds nothing", async () => {
    const { daemon, authenticator } = withPasskey();
    authenticator.refuse("NotAllowedError");
    await setup(daemon);
    await signOff();
    expect(screen.getByRole("alert").textContent).toContain(
      "Cancelled: the passkey prompt was dismissed",
    );
    expect(daemon.resolved).toEqual([]);
    expect(daemon.added).toEqual([]);
    expect(signOffButton().disabled).toBe(false);
  });

  it("shows the daemon's refusal with its hint", async () => {
    const { daemon } = withPasskey();
    daemon.fail.resolveApproval = daemonError(
      Code.InvalidArgument,
      {
        reason: "PASSKEY_ASSERTION_INVALID",
        hint: "The authenticator did not verify the user: use one with a PIN or biometrics.",
        metadata: { check: "flags" },
      },
      "the passkey assertion is invalid: flags",
    );
    await setup(daemon);
    await signOff();
    const alert = screen.getByRole("alert");
    expect(alert.textContent).toContain("the passkey assertion is invalid: flags");
    expect(alert.textContent).toContain("use one with a PIN or biometrics");
    expect(daemon.added).toEqual([]);
  });

  it("lists the change's sign-offs, with whether a person was verified", async () => {
    const { daemon } = withPasskey();
    const signoff = (id: string, presence: SessionPresence, current: boolean) =>
      create(EvidenceSchema, {
        evidenceId: id.repeat(64),
        kind: "signoff",
        result: EvidenceResult.PASS,
        current,
        identity: {
          status: IdentityStatus.BOUND,
          key: {
            identity: "arsood",
            presence,
            ...(presence === SessionPresence.USER_VERIFIED
              ? { passkey: { label: "YubiKey", fingerprint: "ab".repeat(32) } }
              : {}),
          },
        },
      });
    daemon.evidence = [
      signoff("a", SessionPresence.USER_VERIFIED, true),
      signoff("b", SessionPresence.NONE, false),
    ];
    await setup(daemon);
    const rows = screen.getAllByRole("listitem").map((row) => row.textContent);
    expect(rows[0]).toMatch(/^arsood · pass · user verified, passkey “YubiKey” ·/);
    expect(rows[1]).toMatch(/^arsood · pass · not verified · on an earlier tree ·/);
  });
});
