// What each page a phone opens does after a dead connection (R3.40): its
// first read is tried again until it is answered, with nothing done by the
// person, and what it holds is read again when the streams are live again.
import { create } from "@bufbuild/protobuf";
import { Code, ConnectError } from "@connectrpc/connect";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { App } from "../App";
import type { Connection } from "../api/connection";
import { ChangeSchema } from "../gen/reef/api/v1/changes_pb";
import {
  InboxService,
  WatchInboxResponseSchema,
  type WatchInboxResponse,
} from "../gen/reef/api/v1/inbox_pb";
import { OperationService } from "../gen/reef/api/v1/operations_pb";
import { RepositoryService } from "../gen/reef/api/v1/repositories_pb";
import { sleep } from "../live/backoff";
import { InboxStore } from "../live/inbox";
import { OperationStore } from "../live/operations";
import { PresenceStore } from "../live/presence";
import type { Live } from "../live/share";
import { Dying } from "../test/dying";
import {
  daemonError,
  environment,
  FakeStreams,
  fakeTransport,
  immediateFrames,
  operation,
  operationsFrame,
  presenceFrame,
  Pushable,
  settle,
} from "../test/fakes";
import { change, FakeFleetDaemon, workspace } from "../test/fleet-daemon";
import { CHANGE, COMMIT, IdentityDaemon, passkey, TREE } from "../test/identity-daemon";
import { InboxDaemon } from "../test/inbox-daemon";
import { proposal, ProposalDaemon, REPO } from "../test/proposal-daemon";
import { entries, FakeRepoDaemon, hexId } from "../test/smartlog-daemon";
import { act, cleanup, render, screen } from "../test/testing-library";
import { installAuthenticator, removeAuthenticator } from "../test/webauthn";
import { reverseHex } from "../ui/ids";
import { Approve } from "./Approve";
import { Fleet } from "./Fleet";
import { fleetParams } from "./fleet-route";
import { Inbox } from "./Inbox";
import { OperationCache, OperationDetail } from "./OperationDetail";
import { Overview } from "./Overview";
import { Passkeys } from "./Passkeys";
import { Proposal } from "./Proposal";
import { SignOff } from "./SignOff";
import { Smartlog, smartlogParams } from "./Smartlog";

const HOUR = 60 * 60_000;

beforeEach(() => {
  vi.useFakeTimers({ toFake: ["setTimeout", "clearTimeout", "Date"] });
  installAuthenticator();
});

afterEach(() => {
  cleanup();
  removeAuthenticator();
  vi.useRealTimers();
  window.history.replaceState(null, "", "/");
});

async function flush(ms = 0) {
  await act(async () => {
    await settle(60);
    if (ms) await vi.advanceTimersByTimeAsync(ms);
    await settle(60);
  });
}

/** Time in which effects set timers: they run between its steps. */
async function pass(ms: number) {
  for (let at = 0; at < ms; at += 500) await flush(500);
}

function liveStores() {
  const operations = new OperationStore(immediateFrames);
  operations.applyFrame(operationsFrame({ live: true, cursor: "c0" }));
  const presence = new PresenceStore(immediateFrames);
  presence.beginResync();
  presence.applyFrame(presenceFrame([], true));
  const inbox = new InboxStore(immediateFrames);
  inbox.applyFrame(create(WatchInboxResponseSchema, { synced: true }));
  return { operations, presence, inbox };
}

const ended = vi.fn();

interface Page {
  connection: Connection;
  show: (connection: Connection) => void;
  /** On the page once its first read is answered. */
  has: RegExp;
  /** What it reads again. */
  reads: string[];
}

const common = { frame: immediateFrames, onSessionInvalid: ended };

const pages: Record<string, () => Page> = {
  overview: () => ({
    connection: {
      label: "test",
      transport: fakeTransport({
        [RepositoryService.typeName]: {
          listRepositories: () => ({ repositories: [{ id: "a1", root: "/src/alpha" }] }),
        },
        [OperationService.typeName]: { listOperations: () => ({ operations: [] }) },
      }),
    },
    show: (connection) =>
      render(
        <Overview connection={connection} live={liveStores() as unknown as Live} {...common} />,
      ),
    has: /alpha/,
    reads: ["listRepositories"],
  }),
  proposal: () => ({
    connection: new ProposalDaemon().connection(),
    show: (connection) =>
      render(
        <Proposal
          repo={REPO}
          change={CHANGE}
          connection={connection}
          live={liveStores() as unknown as Live}
          {...common}
        />,
      ),
    has: /Fix the lease renewal race/,
    reads: [
      "listProposals",
      "evaluatePolicy",
      "listEvidence",
      "listComments",
      "listTransitions",
      "listProvenance",
      "getIdentity",
      "listPasskeys",
    ],
  }),
  smartlog: () => {
    const daemon = new FakeRepoDaemon();
    daemon.log = entries("a2 a1\na1 t2\nt2! ~t1\n");
    return {
      connection: daemon.connection(),
      show: (connection) =>
        render(
          <Smartlog
            connection={connection}
            live={liveStores() as unknown as Live}
            params={smartlogParams(REPO, `change=${reverseHex(hexId("a1"))}`)}
            {...common}
          />,
        ),
      has: /a1/,
      reads: [
        "listChanges",
        "getStatus",
        "queryChanges",
        // Its sign-offs.
        "evaluatePolicy",
        "getIdentity",
        "listEvidence",
        "listPasskeys",
      ],
    };
  },
  workspaces: () => {
    const daemon = new FakeFleetDaemon();
    daemon.workspaces = [workspace("w1", "c1".padEnd(32, "0"))];
    daemon.changes = [change("c1".padEnd(32, "0"))];
    return {
      connection: daemon.connection(),
      show: (connection) =>
        render(
          <Fleet
            connection={connection}
            live={
              {
                ...liveStores(),
                state: "live",
                subscribeState: () => () => undefined,
              } as unknown as Live
            }
            params={fleetParams(REPO, "workspace=w1")}
            {...common}
          />,
        ),
      has: /w1/,
      reads: ["listWorkspaces", "listChanges", "getWorkspace"],
    };
  },
  passkeys: () => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
    return {
      connection: daemon.connection(),
      show: (connection) => render(<Passkeys connection={connection} onSessionInvalid={ended} />),
      has: /MacBook Touch ID/,
      reads: ["listIdentities", "listPasskeys", "getDaemonStatus"],
    };
  },
  "repository's inbox": () => ({
    connection: {
      label: "test",
      transport: fakeTransport({
        [RepositoryService.typeName]: { getRepository: () => ({ repository: { id: REPO } }) },
      }),
    },
    show: (connection) => {
      const daemon = new InboxDaemon();
      const stores = liveStores();
      stores.inbox.beginResync();
      stores.inbox.applyFrame(daemon.frame());
      render(
        <Inbox
          repo="reef"
          repoNames={new Map()}
          connection={connection}
          live={stores as unknown as Live}
          {...common}
        />,
      );
    },
    has: /Nothing needs you now/,
    reads: [],
  }),
};

describe.each(Object.entries(pages))("the %s page", (_, made) => {
  it("reads until it is answered, and holds no failed read then", async () => {
    const page = made();
    const line = new Dying(page.connection);
    line.state = "failing";
    page.show(line.connection);
    await flush();
    await flush(10_000);
    expect(document.body.textContent).not.toMatch(page.has);
    expect(line.calls.length).toBeGreaterThan(5);
    line.state = "alive";
    await flush(30_000);
    expect(document.body.textContent).toMatch(page.has);
    expect(screen.queryByText("Load failed")).toBeNull();
  });

  it("reads what it holds again when the streams are live again, once", async () => {
    const page = made();
    const line = new Dying(page.connection);
    page.show(line.connection);
    await flush();
    expect(document.body.textContent).toMatch(page.has);
    line.calls.length = 0;
    line.back();
    await flush();
    expect([...new Set(line.calls)].sort()).toEqual([...page.reads].sort());
    cleanup();
    line.calls.length = 0;
    line.back();
    await flush();
    expect(line.calls, "a page left reads nothing").toEqual([]);
    expect(line.held).toBe(0);
  });
});

describe("a proposal", () => {
  function open(daemon: ProposalDaemon) {
    const line = new Dying(daemon.connection());
    const stores = liveStores();
    const show = () =>
      render(
        <Proposal
          repo={REPO}
          change={CHANGE}
          connection={line.connection}
          live={stores as unknown as Live}
          {...common}
        />,
      );
    return { line, stores, show };
  }

  it("changed while the phone slept for an hour is shown once the streams are live", async () => {
    const daemon = new ProposalDaemon();
    const { line, show } = open(daemon);
    show();
    await flush();
    line.state = "hung";
    line.calls.length = 0;
    await flush(10_000);
    expect(line.calls, "a read hangs").toEqual(["listProposals"]);
    vi.setSystemTime(Date.now() + HOUR);
    daemon.entries = [proposal(CHANGE, "d".repeat(40), "Fix the race another way")];
    line.lose();
    line.giveUp();
    await flush();
    expect(screen.getByText("Load failed")).toBeTruthy();
    line.state = "alive";
    line.back();
    await flush();
    expect(screen.getByRole("status").textContent).toContain("The agent changed this proposal");
    expect(screen.queryByText("Load failed")).toBeNull();
  });

  it("is read again soon after a read that took an hour, its streams never lost", async () => {
    const daemon = new ProposalDaemon();
    const { line, show } = open(daemon);
    show();
    await flush();
    line.state = "hung";
    await flush(10_000);
    vi.setSystemTime(Date.now() + HOUR);
    line.giveUp();
    daemon.entries = [proposal(CHANGE, "d".repeat(40), "Fix the race another way")];
    line.state = "alive";
    await flush(1_000);
    expect(screen.getByRole("status").textContent).toContain("The agent changed this proposal");
  });

  it("that was not one is shown once it is proposed", async () => {
    const daemon = new ProposalDaemon();
    const entry = daemon.entries;
    daemon.entries = [];
    const { line, stores, show } = open(daemon);
    show();
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("was never proposed");
    line.calls.length = 0;
    await flush(60_000);
    expect(line.calls, "the daemon's refusal is not tried again").toEqual([]);
    daemon.entries = entry;
    stores.operations.applyFrame(
      operationsFrame({ operations: [operation("o1", REPO, 1)], live: true, cursor: "c1" }),
    );
    await flush(1_000);
    expect(screen.getByText("Fix the lease renewal race")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("whose policy no answer came for is read again", async () => {
    const daemon = new ProposalDaemon();
    const { line, show } = open(daemon);
    line.only = "evaluatePolicy";
    line.state = "failing";
    show();
    await flush();
    expect(screen.getByText("Load failed")).toBeTruthy();
    line.state = "alive";
    await flush(1_000);
    expect(screen.queryByText("Load failed")).toBeNull();
  });

  it("of a repository that is not known is not tried again", async () => {
    const daemon = new ProposalDaemon();
    daemon.failures.listProposals = daemonError(
      Code.NotFound,
      { reason: "REPOSITORY_NOT_FOUND" },
      "no such repository",
    );
    const { line, show } = open(daemon);
    show();
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("no such repository");
    expect(screen.queryByText("Reading the proposal.")).toBeNull();
    line.calls.length = 0;
    await flush(60_000);
    expect(line.calls).toEqual([]);
  });

  it("that a busy daemon refused is tried again, and the refusal shown until then", async () => {
    const daemon = new ProposalDaemon();
    daemon.failures.listProposals = daemonError(
      Code.Unavailable,
      { reason: "REMOTE_BUSY" },
      "too many open",
    );
    const { show } = open(daemon);
    show();
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("too many open");
    expect(screen.getByText("Reading the proposal.")).toBeTruthy();
    delete daemon.failures.listProposals;
    await flush(2_000);
    expect(screen.getByText("Fix the lease renewal race")).toBeTruthy();
    expect(screen.queryByRole("alert")).toBeNull();
  });
});

describe("a page that failed and holds that", () => {
  it.each([
    ["the sign-offs of a change", "listEvidence"],
    ["its signer", "getIdentity"],
  ])("%s: reads again when the streams are live again", async (_, method) => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
    const line = new Dying(daemon.connection());
    line.state = "failing";
    render(
      <SignOff
        connection={line.connection}
        repo={REPO}
        change={create(ChangeSchema, { changeId: CHANGE, commitId: COMMIT, treeId: TREE })}
        onSessionInvalid={ended}
      />,
    );
    await flush();
    expect(line.calls).toContain(method);
    line.state = "alive";
    line.calls.length = 0;
    line.back();
    await flush();
    expect(line.calls).toContain(method);
    expect(screen.getByText("No sign-offs yet.")).toBeTruthy();
    expect(screen.queryByText("Load failed")).toBeNull();
  });

  it.each([
    ["a proposal's diff", pages.proposal, "getDiff"],
    ["a change's files", pages.smartlog, "getDiff"],
    [
      "an operation",
      (): Pick<Page, "connection" | "show"> => ({
        connection: {
          label: "test",
          transport: fakeTransport({
            [OperationService.typeName]: {
              getOperation: () => ({ operation: operation("op1", REPO, 1) }),
            },
          }),
        },
        show: (connection) =>
          render(
            <OperationDetail
              repo={REPO}
              op="op1"
              at=""
              row={undefined}
              inWindow={false}
              connection={connection}
              cache={new OperationCache()}
              onSessionInvalid={ended}
            />,
          ),
      }),
      "getOperation",
    ],
  ])("%s, which does not change: reads again once, until it has it", async (_, made, method) => {
    const page = made?.();
    if (!page) throw new Error("no page");
    const line = new Dying(page.connection);
    line.state = "failing";
    line.only = method;
    page.show(line.connection);
    await flush();
    expect(screen.getByText("Load failed")).toBeTruthy();
    const reads = () => line.calls.filter((call) => call === method).length;
    expect(reads()).toBe(1);
    line.state = "alive";
    line.back();
    await flush();
    expect(screen.queryByText("Load failed")).toBeNull();
    expect(reads()).toBe(2);
    line.back();
    await flush();
    expect(reads()).toBe(2);
  });

  it("workspaces of a repository that was not known, once it is", async () => {
    const daemon = new FakeFleetDaemon();
    daemon.failRepository = daemonError(
      Code.NotFound,
      { reason: "REPOSITORY_NOT_FOUND" },
      "no such repository",
    );
    const line = new Dying(daemon.connection());
    render(
      <Fleet
        connection={line.connection}
        live={
          {
            ...liveStores(),
            state: "live",
            subscribeState: () => () => undefined,
          } as unknown as Live
        }
        params={fleetParams(REPO, "")}
        {...common}
      />,
    );
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("no such repository");
    daemon.failRepository = undefined;
    line.back();
    await flush();
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it("an approval: its request, its change and what is below it", async () => {
    const daemon = new IdentityDaemon();
    const approval = await daemon.pending();
    const line = new Dying(daemon.connection());
    line.state = "failing";
    const ui = <Approve connection={line.connection} id={approval.id} onSessionInvalid={ended} />;
    render(ui);
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("Load failed");
    line.state = "alive";
    line.back();
    await flush();
    expect(screen.queryByText("Load failed")).toBeNull();
    expect(screen.getByRole("button", { name: "Approve" })).toBeTruthy();
    line.calls.length = 0;
    line.back();
    await flush();
    expect([...new Set(line.calls)].sort()).toEqual(["getApproval", "getChange", "listChanges"]);
  });

  it("a frozen smartlog and frozen workspaces read again only what failed", async () => {
    const repos = new FakeRepoDaemon();
    repos.log = entries("a2 a1\na1 t2\nt2! ~t1\n");
    const fleets = new FakeFleetDaemon();
    const live = { ...liveStores(), state: "live", subscribeState: () => () => undefined };
    for (const [connection, page] of [
      [
        repos.connection(),
        (connection: Connection) => (
          <Smartlog
            connection={connection}
            live={live as unknown as Live}
            params={smartlogParams(REPO, "at=op1")}
            {...common}
          />
        ),
      ],
      [
        fleets.connection(),
        (connection: Connection) => (
          <Fleet
            connection={connection}
            live={live as unknown as Live}
            params={fleetParams(REPO, "at=op1")}
            {...common}
          />
        ),
      ],
    ] as const) {
      const line = new Dying(connection);
      line.state = "failing";
      render(page(line.connection));
      await flush();
      expect(screen.getByText("Load failed")).toBeTruthy();
      line.state = "alive";
      line.back();
      await flush();
      expect(screen.queryByText("Load failed")).toBeNull();
      line.calls.length = 0;
      line.back();
      await flush();
      expect(line.calls).toEqual([]);
      cleanup();
    }
  });
});

describe("while the streams are live, what the daemon is too busy to answer", () => {
  /** Answers UNAVAILABLE for a while, as a token with too many calls does. */
  function busy(connection: Connection) {
    const line = new Dying(connection);
    line.error = daemonError(Code.Unavailable, { reason: "REMOTE_BUSY" }, "too many open");
    return line;
  }

  it("is read again, less and less often, and what is held stays", async () => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
    const line = busy(daemon.connection());
    render(<Passkeys connection={line.connection} onSessionInvalid={ended} />);
    await flush();
    expect(screen.getByText("MacBook Touch ID")).toBeTruthy();
    line.state = "failing";
    line.back();
    await flush();
    line.calls.length = 0;
    await pass(60_000);
    expect(screen.getByText("MacBook Touch ID")).toBeTruthy();
    expect(screen.queryByText("too many open")).toBeNull();
    const minute = line.calls.length / 3;
    expect(minute).toBeGreaterThan(5);
    expect(minute).toBeLessThan(12);
    daemon.passkeys = [passkey(daemon.arsood, "YubiKey", 1)];
    line.state = "alive";
    await pass(31_000);
    expect(screen.getByText("YubiKey")).toBeTruthy();
    line.calls.length = 0;
    await flush(HOUR);
    expect(line.calls).toEqual([]);
    // Nothing held, and a refusal: both are said.
    line.state = "failing";
    line.error = daemonError(
      Code.PermissionDenied,
      { reason: "REMOTE_ACCESS_DENIED" },
      "not yours",
    );
    line.back();
    await flush();
    expect(screen.getByText("not yours")).toBeTruthy();
    expect(screen.queryByText("YubiKey")).toBeNull();
    line.calls.length = 0;
    await flush(HOUR);
    expect(line.calls).toEqual([]);
  });

  it.each([
    ["a proposal's diff", "getDiff", () => pages.proposal?.()],
    ["a proposal's signer", "getIdentity", () => pages.proposal?.()],
    ["a change's files", "getDiff", () => pages.smartlog?.()],
    ["a change's history", "queryChanges", () => pages.smartlog?.()],
    ["a change's sign-offs", "listEvidence", () => pages.smartlog?.()],
  ])("%s, is read again until it is answered, and then not", async (_, method, made) => {
    const page = made();
    if (!page) throw new Error("no page");
    const line = busy(page.connection);
    line.state = "failing";
    line.only = method;
    page.show(line.connection);
    await flush();
    expect(screen.getAllByText("too many open").length).toBeGreaterThan(0);
    const reads = () => line.calls.filter((call) => call === method).length;
    await pass(60_000);
    expect(reads()).toBeGreaterThan(4);
    line.state = "alive";
    await pass(31_000);
    expect(screen.queryByText("too many open")).toBeNull();
    const answered = reads();
    await flush(HOUR);
    expect(reads()).toBe(answered);
    line.state = "failing";
    line.back();
    await flush();
    cleanup();
    expect(vi.getTimerCount(), "a page left reads nothing").toBe(0);
  });

  it("the sign-offs that are shown stay, and the daemon's status is read again", async () => {
    const daemon = new IdentityDaemon();
    daemon.passkeys = [passkey(daemon.arsood, "MacBook Touch ID", 1)];
    const line = busy(daemon.connection());
    render(
      <SignOff
        connection={line.connection}
        repo={REPO}
        change={create(ChangeSchema, { changeId: CHANGE, commitId: COMMIT, treeId: TREE })}
        onSessionInvalid={ended}
      />,
    );
    await flush();
    expect(screen.getByText("No sign-offs yet.")).toBeTruthy();
    line.state = "failing";
    line.back();
    await flush();
    expect(screen.getByText("No sign-offs yet.")).toBeTruthy();
    expect(screen.queryByText("too many open")).toBeNull();
    cleanup();

    const app = busy(new FakeStreams().connection());
    app.state = "failing";
    app.only = "getDaemonStatus";
    render(
      <App
        session={{ connection: app.connection, token: "tok" }}
        environment={environment(immediateFrames)}
      />,
    );
    await flush();
    expect(screen.getByText("too many open")).toBeTruthy();
    app.state = "alive";
    await pass(2_000);
    expect(screen.queryByText("too many open")).toBeNull();
  });

  it("an approval that is shown stays, and is read again", async () => {
    const daemon = new IdentityDaemon();
    const approval = await daemon.pending();
    const line = busy(daemon.connection());
    render(<Approve connection={line.connection} id={approval.id} onSessionInvalid={ended} />);
    await flush();
    line.state = "failing";
    line.only = "getApproval";
    line.back();
    await flush();
    expect(screen.getByRole("button", { name: "Approve" })).toBeTruthy();
    line.calls.length = 0;
    await pass(10_000);
    expect(line.calls.length).toBeGreaterThan(3);
    expect(screen.getByRole("button", { name: "Approve" })).toBeTruthy();
  });

  it.each(["proposal", "smartlog", "workspaces"])(
    "a %s page of a repository that is not open is read every 30 s, and says so all along",
    async (name) => {
      const page = pages[name]?.();
      if (!page) throw new Error("no page");
      const line = busy(page.connection);
      line.error = daemonError(Code.Unavailable, { reason: "" }, "it is not open");
      line.state = "failing";
      page.show(line.connection);
      await flush();
      for (let i = 0; i < 60; i++) {
        await flush(1_000);
        expect(screen.getAllByText("it is not open").length).toBeGreaterThan(0);
      }
      line.calls.length = 0;
      await pass(5 * 60_000);
      // At most four things read on their own, each every 30 s.
      expect(line.calls.length).toBeLessThanOrEqual(44);
      expect(line.calls.length).toBeGreaterThan(5);
    },
  );
});

describe("the app", () => {
  it("shows why the daemon refuses its streams, until they are live", async () => {
    const streams = new FakeStreams();
    const busy = daemonError(
      Code.Unavailable,
      { reason: "REMOTE_BUSY", hint: "Close some." },
      "this remote token has too many streams open",
    );
    let refused = true;
    const inbox: Pushable<WatchInboxResponse>[] = [];
    const line = new Dying(
      streams.connection({
        services: {
          [RepositoryService.typeName]: { listRepositories: () => ({ repositories: [] }) },
          [InboxService.typeName]: {
            watchInbox: (_: never, options: { signal: AbortSignal | undefined }) => {
              const stream = new Pushable<WatchInboxResponse>(options.signal);
              if (refused) stream.fail(busy);
              else stream.push(create(WatchInboxResponseSchema, { synced: true }));
              inbox.push(stream);
              return stream;
            },
          },
        },
      }),
    );
    const env = environment(immediateFrames);
    render(
      <App
        session={{ connection: line.connection, token: "tok" }}
        environment={{ ...env, sleep: (ms, signal) => sleep(ms, signal) }}
      />,
    );
    await flush();
    streams.lastOperations().push(operationsFrame({ live: true, cursor: "c1" }));
    streams.lastPresence().push(presenceFrame([], true));
    await flush();
    expect(document.querySelector(".connection")?.textContent).toBe("Connecting");
    expect(screen.getByRole("alert").textContent).toContain("too many streams open");
    expect(screen.getByRole("alert").textContent).toContain("Close some.");
    refused = false;
    await flush(5_000);
    expect(document.querySelector(".connection")?.textContent).toBe("Live");
    expect(screen.queryByRole("alert")).toBeNull();
    // Nor has one that is refused for good, or starts over: it is asked for again at once.
    streams
      .lastOperations()
      .fail(daemonError(Code.FailedPrecondition, { reason: "TOO_FAR_BEHIND" }, "too far behind"));
    await flush();
    expect(screen.queryByRole("alert")).toBeNull();
    // A stream that fails as connections do has no words of the daemon's.
    inbox.at(-1)?.fail(new ConnectError("down", Code.Unavailable));
    await flush();
    expect(document.querySelector(".connection")?.textContent).toBe("Reconnecting");
    expect(screen.queryByRole("alert")).toBeNull();
  });

  it.each([
    ["http:", 0],
    ["https:", 1],
  ])("over %s loads its pages ahead, or not", async (protocol, ahead) => {
    vi.stubGlobal("location", { protocol, pathname: "/states", search: "", hash: "" });
    const line = new Dying(new FakeStreams().connection());
    const before = line.held;
    render(
      <App
        session={{ connection: line.connection, token: "tok" }}
        environment={environment(immediateFrames)}
      />,
    );
    await flush();
    expect(line.held - before).toBe(1 + ahead);
    cleanup();
    vi.unstubAllGlobals();
  });

  it("reads the daemon's status again when the streams are live again", async () => {
    const line = new Dying(new FakeStreams().connection());
    line.state = "failing";
    render(
      <App
        session={{ connection: line.connection, token: "tok" }}
        environment={environment(immediateFrames)}
      />,
    );
    await flush();
    expect(screen.getByRole("alert").textContent).toContain("Load failed");
    line.state = "alive";
    line.back();
    await flush();
    expect(screen.queryByText("Load failed")).toBeNull();
  });
});
