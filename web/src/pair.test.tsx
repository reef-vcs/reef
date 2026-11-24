// Pairing a phone (R3.40): the form in a Home Screen app and in a tab, the
// code as it is sent, what a refusal says, the token kept as the session, and
// what a paired phone's pages say of it.
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { Code, createRouterTransport } from "@connectrpc/connect";
import { afterEach, describe, expect, it, vi } from "vitest";
import { client, noteRefusals, type Connection } from "./api/connection";
import { App } from "./App";
import { device, endDevice, keepDevice, openSession } from "./device";
import { DaemonService } from "./gen/reef/api/v1/daemon_pb";
import { OperationService } from "./gen/reef/api/v1/operations_pb";
import { RemoteScope, RemoteService } from "./gen/reef/api/v1/remote_pb";
import { typedCode } from "./pair";
import { embedded, SESSION_KEY, storeSession } from "./session";
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
  it("is first in a Home Screen app of an iOS that says so only in its own way", () => {
    vi.stubGlobal(
      "navigator",
      Object.assign(Object.create(navigator) as object, { standalone: true }),
    );
    render(<NoSession remote />);
    expect(screen.getByLabelText("Pairing code")).toBeTruthy();
  });

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
    // A page that is no phone's takes it, whatever it holds.
    localStorage.clear();
    storeSession("older");
    window.history.replaceState(null, "", "/#session=other");
    expect(openSession()).toBe("other");
    window.history.replaceState(null, "", "/");
  });

  it("takes nothing else from such a link", () => {
    paired();
    window.history.replaceState(null, "", "/#session=other&embedded=1");
    expect(openSession()).toBe(PAIRED_TOKEN);
    expect(window.location.hash).toBe("");
    expect(embedded()).toBe(false);
    expect(sessionStorage.getItem("reef.embedded")).toBeNull();
  });

  it("is one no more once its pairing has ended", () => {
    vi.useFakeTimers({ now: PAIRED.expiresAt - 1, toFake: ["Date"] });
    paired();
    expect(device()).toEqual(PAIRED);
    vi.setSystemTime(PAIRED.expiresAt);
    expect(device()).toBeNull();
    window.history.replaceState(null, "", "/#session=other");
    expect(openSession()).toBe("other");
    vi.useRealTimers();
  });

  it("is forgotten with its own token, not with an older one", () => {
    paired();
    endDevice(`reef_ffeeddccbbaa9988_${"y".repeat(43)}`);
    expect(device()).toEqual(PAIRED);
    endDevice(PAIRED_TOKEN);
    expect(localStorage.getItem("reef.device")).toBeNull();
  });

  /** The app with its streams open; their signals. */
  function app(
    token = PAIRED_TOKEN,
    getDaemonStatus: () => unknown = () => new Promise(() => undefined),
  ): AbortSignal[] {
    const signals: AbortSignal[] = [];
    const streams = new FakeStreams();
    const { subscribeOperations } = streams;
    streams.subscribeOperations = (request, options) => {
      if (options.signal) signals.push(options.signal);
      return subscribeOperations(request, options);
    };
    window.history.replaceState(null, "", "/states");
    render(
      <App
        session={{
          connection: streams.connection({
            services: { [DaemonService.typeName]: { getDaemonStatus } },
          }),
          token,
        }}
        environment={environment(manualFrames().frame)}
      />,
    );
    return signals;
  }
  const notConnected = () => screen.queryByRole("heading", { name: "Not connected" });
  const changed = (key: string | null) =>
    act(() => {
      window.dispatchEvent(new StorageEvent("storage", { key }));
    });

  it.each([SESSION_KEY, null])("ends in every tab once it is unpaired in one (%s)", async (key) => {
    paired();
    const signals = app();
    expect(notConnected()).toBeNull();
    localStorage.clear();
    await changed(key);
    expect(notConnected()).toBeTruthy();
    expect(signals.map((signal) => signal.aborted)).toEqual([true]);
  });

  it("goes on while a tab opens at a link's token and puts the phone's back", async () => {
    paired();
    const signals = app();
    storeSession("other");
    await changed(SESSION_KEY);
    storeSession(PAIRED_TOKEN);
    await changed(SESSION_KEY);
    expect(notConnected()).toBeNull();
    expect(signals.map((signal) => signal.aborted)).toEqual([false]);
  });

  it("is the only page that ends with the storage", async () => {
    storeSession("another");
    app("another");
    localStorage.clear();
    await changed(SESSION_KEY);
    expect(notConnected()).toBeNull();
  });

  it("ends its streams when a call is refused", async () => {
    paired();
    const signals = app(PAIRED_TOKEN, () => {
      throw daemonError(Code.PermissionDenied, { reason: "REMOTE_TOKEN_INVALID" });
    });
    await screen.findByRole("heading", { name: "Not connected" });
    expect(signals.map((signal) => signal.aborted)).toEqual([true]);
    expect(localStorage.length).toBe(0);
  });

  it("ends its streams when Settings unpairs it", async () => {
    paired();
    const signals = app();
    for (const name of ["Settings", "Unpair this phone", "Unpair"]) {
      await act(() => {
        screen.getByRole("button", { name }).click();
      });
    }
    expect(notConnected()).toBeTruthy();
    expect(localStorage.length).toBe(0);
    expect(signals.map((signal) => signal.aborted)).toEqual([true]);
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

describe("a phone whose daemon's address moved", () => {
  /** The app over a daemon that refuses every call, as the gate does. */
  async function refused(origin: string, reason = "REMOTE_ORIGIN_CHANGED") {
    const made: { connection?: Connection } = {};
    const refuse = () => {
      throw daemonError(Code.PermissionDenied, {
        reason,
        hint: `Open ${origin} on the device and pair it again.`,
        metadata: { device_origin: "https://old.example:7470", origin },
      });
    };
    const transport = createRouterTransport(
      ({ service }) => {
        service(DaemonService, { getDaemonStatus: () => new Promise(() => undefined) });
        service(OperationService, {
          // eslint-disable-next-line require-yield
          async *subscribeOperations() {
            refuse();
          },
        });
      },
      { transport: { interceptors: [noteRefusals(() => made.connection)] } },
    );
    made.connection = { transport, label: "test" };
    paired();
    window.history.replaceState(null, "", "/states");
    render(
      <App
        session={{ connection: made.connection, token: PAIRED_TOKEN }}
        environment={environment(manualFrames().frame)}
      />,
    );
    await screen.findByRole("heading", { name: "Not connected" });
  }

  it("says where it is now, offers no code there, and keeps its pairing", async () => {
    await refused("https://new.example:7470");
    expect(screen.getByText(/Open https:\/\/new\.example:7470 on the device/)).toBeTruthy();
    expect(screen.getByText(/was paired at https:\/\/old\.example:7470/)).toBeTruthy();
    expect(screen.getByRole("button", { name: "Try again" })).toBeTruthy();
    expect(screen.queryByLabelText("Pairing code")).toBeNull();
    expect(screen.queryByRole("button", { name: "Pair this browser instead" })).toBeNull();
    expect(localStorage.getItem(SESSION_KEY)).toBe(PAIRED_TOKEN);
    expect(device()).toEqual(PAIRED);
  });

  it("shows the daemon's words as words", async () => {
    await refused("<img src=x>");
    expect(screen.getByText(/Open <img src=x> on the device/)).toBeTruthy();
    expect(document.querySelector("img")).toBeNull();
  });

  it("offers a code where the daemon is at this address", async () => {
    await refused(window.location.origin);
    expect(screen.getByRole("button", { name: "Pair this browser instead" })).toBeTruthy();
  });

  it("forgets a pairing that was revoked", async () => {
    await refused("", "REMOTE_TOKEN_INVALID");
    expect(localStorage.length).toBe(0);
  });

  it("is told by a unary call too", async () => {
    const made: { connection?: Connection } = {};
    const transport = createRouterTransport(
      ({ service }) => {
        service(DaemonService, {
          getDaemonStatus: () => {
            throw daemonError(Code.PermissionDenied, { reason: "REMOTE_ORIGIN_CHANGED" });
          },
        });
      },
      { transport: { interceptors: [noteRefusals(() => made.connection)] } },
    );
    made.connection = { transport, label: "test" };
    await expect(client(made.connection, DaemonService).getDaemonStatus({})).rejects.toThrow();
    expect(made.connection.refused?.reason).toBe("REMOTE_ORIGIN_CHANGED");
  });
});
