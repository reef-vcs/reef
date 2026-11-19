// The Passkeys page (R3.43) over an in-memory daemon and a fake
// authenticator: each person's passkeys, the policy snippet, adding a first
// and a later passkey, removing one, and the browser's refusals in words.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { afterEach, describe, expect, it, vi } from "vitest";
import { equal } from "../sign/bytes";
import { removalChallenge } from "../sign/key";
import { settle } from "../test/fakes";
import { IdentityDaemon, passkey, person } from "../test/identity-daemon";
import { act, cleanup, fireEvent, render, screen, waitFor, within } from "../test/testing-library";
import { installAuthenticator, removeAuthenticator } from "../test/webauthn";
import { Passkeys, policySnippet } from "./Passkeys";

afterEach(() => {
  cleanup();
  removeAuthenticator();
});

async function setup(daemon: IdentityDaemon) {
  render(<Passkeys connection={daemon.connection()} onSessionInvalid={vi.fn()} />);
  await act(async () => {
    await settle(30);
  });
}

function card(name: string): HTMLElement {
  return screen.getByRole("region", { name: new RegExp(`^${name}`) });
}

async function add(name: string, label: string) {
  const form = card(name);
  fireEvent.input(within(form).getByLabelText("Label"), { target: { value: label } });
  await act(() => {
    within(form).getByRole("button", { name: "Add a passkey" }).click();
  });
}

describe("the Passkeys page", () => {
  it("lists each person identity's passkeys with the snippet that pins them", async () => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [
      passkey(daemon.arsood, "MacBook Touch ID", 1),
      { ...passkey(daemon.arsood, "YubiKey", 2), lastUsedAt: timestampFromMs(1_795_100_000_000) },
    ];
    installAuthenticator();
    await setup(daemon);
    expect(screen.queryByRole("region", { name: /^ci-bot/ })).toBeNull();
    const rows = within(card("arsood")).getAllByRole("row").slice(1);
    expect(rows.map((row) => row.querySelector("td")?.textContent)).toEqual([
      "MacBook Touch ID",
      "YubiKey",
    ]);
    expect(within(rows[0]!).getByText("1".repeat(64))).toBeTruthy();
    expect(within(rows[0]!).getByText("never")).toBeTruthy();
    const snippet = `[passkeys]\narsood = ["${"1".repeat(64)}", "${"2".repeat(64)}"]\n`;
    expect(card("arsood").querySelector("pre")?.textContent).toBe(snippet);

    const writeText = vi.fn(() => Promise.resolve());
    Object.defineProperty(navigator, "clipboard", { configurable: true, value: { writeText } });
    await act(() => {
      within(card("arsood")).getByRole("button", { name: "Copy policy snippet" }).click();
    });
    expect(writeText).toHaveBeenCalledWith(snippet);
  });

  it("adds a first passkey with no other touch", async () => {
    const daemon = new IdentityDaemon();
    const authenticator = installAuthenticator();
    await setup(daemon);
    expect(within(card("arsood")).getByText(/^No passkeys\./)).toBeTruthy();
    await add("arsood", "MacBook Touch ID");
    await waitFor(() => {
      expect(within(card("arsood")).getByRole("status").textContent).toMatch(
        /^Added “MacBook Touch ID”/,
      );
    });
    expect(authenticator.gets).toEqual([]);
    const options = authenticator.creates[0];
    expect(new TextDecoder().decode(options?.user.id as ArrayBuffer)).toBe(daemon.arsood.id);
    expect(options?.user.name).toBe("Reef: arsood");
    expect(options?.rp.id).toBe("localhost");
    expect(options?.authenticatorSelection?.userVerification).toBe("required");
    expect(options?.attestation).toBe("none");
    expect(options?.pubKeyCredParams.map((p) => p.alg)).toEqual([-8, -7, -257]);
    const [registered] = daemon.registered;
    expect(registered?.label).toBe("MacBook Touch ID");
    expect(registered?.algorithm).toBe(-7);
    expect(registered?.approval).toBeUndefined();
    expect(await within(card("arsood")).findByText("MacBook Touch ID")).toBeTruthy();
  });

  it("adds a later passkey only with a touch of an existing one", async () => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
    const authenticator = installAuthenticator();
    await setup(daemon);
    await add("arsood", "YubiKey");
    await waitFor(() => {
      expect(within(card("arsood")).getByRole("status").textContent).toMatch(/^Added “YubiKey”/);
    });
    const approval = authenticator.gets[0];
    expect(
      equal(new Uint8Array(approval?.challenge as ArrayBuffer), new Uint8Array(32).fill(0xad)),
    ).toBe(true);
    expect(authenticator.creates[0]?.excludeCredentials?.length).toBe(1);
    expect(daemon.registered[0]?.approval?.signature).toEqual(new Uint8Array([0x51]));
  });

  it("removes a passkey with a touch over its removal challenge", async () => {
    const daemon = new IdentityDaemon();
    const gone = passkey(daemon.arsood, "Old laptop", 1);
    daemon.passkeys = [gone, passkey(daemon.arsood, "YubiKey", 2)];
    const authenticator = installAuthenticator();
    await setup(daemon);
    await act(() => {
      screen.getByRole("button", { name: "Remove Old laptop" }).click();
    });
    await waitFor(() => {
      expect(within(card("arsood")).getByRole("status").textContent).toMatch(
        /^Removed “Old laptop”/,
      );
    });
    const get = authenticator.gets[0];
    expect(
      equal(new Uint8Array(get?.challenge as ArrayBuffer), await removalChallenge(gone.id)),
    ).toBe(true);
    expect(get?.allowCredentials?.length).toBe(2);
    expect(daemon.removed.map((r) => r.id)).toEqual([gone.id]);
    await waitFor(() => {
      expect(within(card("arsood")).queryByText("Old laptop")).toBeNull();
    });
  });

  it("says Cancelled when the person dismisses the prompt", async () => {
    const daemon = new IdentityDaemon();
    const authenticator = installAuthenticator();
    authenticator.refuse("NotAllowedError");
    await setup(daemon);
    await add("arsood", "MacBook Touch ID");
    await waitFor(() => {
      expect(within(card("arsood")).getByRole("alert").textContent).toMatch(/^Cancelled: /);
    });
    expect(daemon.registered).toEqual([]);
  });

  it("says when the browser has no passkey support", async () => {
    await setup(new IdentityDaemon());
    expect(screen.getByRole("alert").textContent).toBe("This browser has no passkey support.");
  });
});

describe("policySnippet", () => {
  it("quotes a name TOML cannot leave bare", () => {
    const alice = person("alice.smith");
    expect(policySnippet({ identity: alice, passkeys: [passkey(alice, "a", 3)] })).toBe(
      `[passkeys]\n"alice.smith" = ["${"3".repeat(64)}"]\n`,
    );
  });
});
