// #413: a page whose session the daemon refuses reports it, so that in the
// desktop app the shell mints a new one (`leaveEndedSession` leaves only an
// embedded page).
import { Code } from "@connectrpc/connect";
import { afterEach, expect, it, vi } from "vitest";
import { App } from "./App";
import { DaemonService } from "./gen/reef/api/v1/daemon_pb";
import { leaveEndedSession } from "./session";
import { daemonError, environment, FakeStreams, manualFrames } from "./test/fakes";
import { cleanup, render, screen } from "./test/testing-library";

vi.mock("./session", async (original) => ({
  ...(await original<typeof import("./session")>()),
  leaveEndedSession: vi.fn(),
}));

afterEach(() => {
  cleanup();
  localStorage.clear();
});

it("reports a session the daemon refused", async () => {
  window.history.replaceState(null, "", "/states");
  const streams = new FakeStreams();
  render(
    <App
      session={{
        connection: streams.connection({
          services: {
            [DaemonService.typeName]: {
              getDaemonStatus: () => {
                throw daemonError(Code.PermissionDenied, { reason: "WEB_SESSION_INVALID" });
              },
            },
          },
        }),
        token: "tok",
      }}
      environment={environment(manualFrames().frame)}
    />,
  );
  await screen.findByRole("heading", { name: "Not connected" });
  expect(leaveEndedSession).toHaveBeenCalled();
});
