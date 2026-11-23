// Pairing a phone (R3.40): the form in a Home Screen app and in a tab, the
// code as it is sent, what a refusal says, the token kept as the session, and
// what a paired phone's pages say of it.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code, createRouterTransport } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import type { Connection } from "./api/connection";
import { reefError } from "./api/errors";
import { App } from "./App";
import { device, endDevice, keepDevice, openSession } from "./device";
import { RemoteScope, RemoteService } from "./gen/reef/api/v1/remote_pb";
import { typedCode } from "./pair";
import { SESSION_KEY, storeSession } from "./session";
import { daemonError, environment, FakeStreams, manualFrames } from "./test/fakes";
import { PAIRED, paired, PAIRED_TOKEN } from "./test/paired";
import { act, cleanup, fireEvent, render, screen } from "./test/testing-library";
import { Settings } from "./ui/Settings";
import { NoSession } from "./views/NoSession";

const SECRET = `reef_00112233aabbccdd_${"x".repeat(43)}`;

afterEach(() => {
  cleanup();
  window.history.replaceState(null, "", "/");
  vi.unstubAllGlobals();
});

function daemon(refused?: string) {
  const codes: string[] = [];
  const transport = createRouterTransport(({ service }) => {
    service(RemoteService, {
      redeemPairing: () => {
        if (refused !== undefined) {
          throw refused
            ? daemonError(Code.PermissionDenied, { reason: refused })
            : new TypeError("Failed to fetch");
        }
        return {
          token: {
            id: "00112233aabbccdd",
            name: "iphone",
            scope: RemoteScope.DEVICE,
            expiresAt: timestampFromMs(1_800_000_000_000),
          },
          secret: SECRET,
        };
      },
    });
  });
  const connect = (code: string): Connection => {
    codes.push(code);
    return { transport, label: "test" };
  };
  return { codes, connect };
}

function standalone() {
  vi.stubGlobal("matchMedia", (query: string) => ({
    matches: query === "(display-mode: standalone)",
  }));
}

async function type(code: string) {
  fireEvent.input(screen.getByLabelText("Pairing code"), { target: { value: code } });
  await act(() => {
    screen.getByRole("button", { name: "Pair" }).click();
  });
}

describe("the pairing form", () => {
  it("in a tab, shows the Home Screen steps first", async () => {
    render(<NoSession remote />);
    expect(screen.getByText("Add to Home Screen")).toBeTruthy();
    expect(screen.queryByLabelText("Pairing code")).toBeNull();
    await act(() => {
      screen.getByRole("button", { name: "Pair this browser instead" }).click();
    });
    expect(screen.getByLabelText("Pairing code")).toBeTruthy();
  });

  it("in the Home Screen app, pairs with what was typed", async () => {
    standalone();
    const { codes, connect } = daemon();
    const onPaired = vi.fn();
    render(<NoSession remote onPaired={onPaired} connect={connect} />);
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Pair" }).disabled).toBe(true);
    await type(" k7qx-9m2d ");
    expect(codes).toEqual(["K7QX9M2D"]);
    expect(onPaired).toHaveBeenCalledWith(SECRET);
    expect(localStorage.getItem(SESSION_KEY)).toBe(SECRET);
    expect(device()).toEqual({
      id: "00112233aabbccdd",
      name: "iphone",
      expiresAt: 1_800_000_000_000,
    });
  });

  it.each([
    ["PAIRING_CODE_INVALID", /not valid: it is mistyped, used or expired/],
    ["REMOTE_TOKEN_INVALID", /Too many wrong codes/],
    ["", /did not answer/],
  ])("says what a refusal means (%s)", async (reason, words) => {
    standalone();
    const { connect } = daemon(reason);
    render(<NoSession remote connect={connect} />);
    await type("K7QX-9M2D");
    expect((await screen.findByRole("alert")).textContent).toMatch(words);
    expect(localStorage.getItem(SESSION_KEY)).toBeNull();
    expect(screen.getByRole<HTMLButtonElement>("button", { name: "Pair" }).disabled).toBe(false);
  });

  it("sends letters and digits alone, in upper case", () => {
    expect(typedCode("k7qx–9m2d\n")).toBe("K7QX9M2D");
  });

  it("is not on the local page", () => {
    render(<NoSession remote={false} />);
    expect(screen.getByText("reef ui")).toBeTruthy();
    expect(screen.queryByText("Add to Home Screen")).toBeNull();
  });
});

describe("a paired phone", () => {
  it("is one only while its own token is the session", () => {
    keepDevice(PAIRED);
    expect(device()).toBeNull();
    storeSession(PAIRED_TOKEN);
    expect(device()).toEqual(PAIRED);
    storeSession(`reef_ffeeddccbbaa9988_${"y".repeat(43)}`);
    expect(device()).toBeNull();
    localStorage.setItem("reef.device", "{");
    expect(device()).toBeNull();
  });

  it("keeps its session when a link brings another", () => {
    paired();
    window.history.replaceState(null, "", "/#session=other");
    expect(openSession()).toBe(PAIRED_TOKEN);
    expect(localStorage.getItem(SESSION_KEY)).toBe(PAIRED_TOKEN);
    expect(window.location.hash).toBe("");
    expect(device()).toEqual(PAIRED);
    // A page that is no phone's takes it.
    localStorage.clear();
    window.history.replaceState(null, "", "/#session=other");
    expect(openSession()).toBe("other");
    window.history.replaceState(null, "", "/");
  });

  it("is forgotten with its own token, not with an older one", () => {
    paired();
    endDevice(`reef_ffeeddccbbaa9988_${"y".repeat(43)}`);
    expect(device()).toEqual(PAIRED);
    endDevice(PAIRED_TOKEN);
    expect(localStorage.getItem("reef.device")).toBeNull();
  });

  it("ends in every tab once it is unpaired in one", async () => {
    paired();
    window.history.replaceState(null, "", "/states");
    render(
      <App
        session={{ connection: new FakeStreams().connection(), token: PAIRED_TOKEN }}
        environment={environment(manualFrames().frame)}
      />,
    );
    expect(screen.queryByRole("heading", { name: "Not connected" })).toBeNull();
    localStorage.clear();
    await act(() => {
      window.dispatchEvent(new StorageEvent("storage", { key: SESSION_KEY, newValue: null }));
    });
    expect(screen.getByRole("heading", { name: "Not connected" })).toBeTruthy();
  });

  it("says its name and end in Settings, and unpairs", async () => {
    paired();
    const onUnpair = vi.fn();
    render(<Settings status={null} onUnpair={onUnpair} />);
    await act(() => {
      screen.getByRole("button", { name: "Settings" }).click();
    });
    expect(screen.getByText(/^Paired as iphone until .*2027.*only reads/)).toBeTruthy();
    expect(screen.queryByRole("link", { name: "Passkeys" })).toBeNull();
    await act(() => {
      screen.getByRole("button", { name: "Unpair this phone" }).click();
    });
    expect(onUnpair).not.toHaveBeenCalled();
    expect(document.activeElement).toBe(screen.getByRole("button", { name: "Keep it" }));
    expect(screen.getByText("reef remote token revoke iphone")).toBeTruthy();
    await act(() => {
      screen.getByRole("button", { name: "Unpair" }).click();
    });
    expect(onUnpair).toHaveBeenCalledOnce();
  });
});

describe("the daemon's refusal of a moved device", () => {
  it("ends the session", async () => {
    const { isSessionInvalid } = await import("./api/errors");
    const moved = daemonError(Code.PermissionDenied, { reason: "REMOTE_ORIGIN_CHANGED" });
    expect(isSessionInvalid(reefError(moved))).toBe(true);
  });
});
