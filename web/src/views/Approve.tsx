// `/approve/:id` (R3.43 decision 4): one sign-off a person approves with a
// passkey touch. Everything the touch approves is on the page before the
// button: the identity, this page's address, the kind, result and target, the change and its
// tree, who asked, and the time left. Nothing is approved on load; the only
// way to approve is Approve, which needs the passkey.
import { durationFromMs, timestampMs } from "@bufbuild/protobuf/wkt";
import { useEffect, useState } from "react";
import { client, type Connection } from "../api/connection";
import { isPassing, isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { useAgain } from "../api/streams";
import { device } from "../device";
import { after } from "../live/backoff";
import { ChangeService, type Change } from "../gen/reef/api/v1/changes_pb";
import { ApprovalState, IdentityService, type Approval } from "../gen/reef/api/v1/identity_pb";
import { approve, decline } from "../sign/approval";
import { failure, type Failure } from "../sign/passkey";
import { ErrorView } from "../ui/ErrorView";
import { FailureView } from "../ui/FailureView";
import { reverseHex, shortId } from "../ui/ids";
import { clock } from "../ui/time";
import { hasPasskeySupport } from "../sign/passkey";
import { NoPasskeysHere } from "./Handoff";

export const APPROVAL_NOT_FOUND = "APPROVAL_NOT_FOUND";

type Load =
  | { kind: "loading" }
  | { kind: "missing" }
  | { kind: "failed"; error: ReefError }
  | { kind: "loaded"; approval: Approval };

type Action =
  | { kind: "idle" }
  | { kind: "approving" }
  | { kind: "declining" }
  | { kind: "failed"; failure: Failure };

/** The approval, then each change of its state, by long polls until it is no longer pending. */
function useApproval(connection: Connection, id: string, onSessionInvalid: () => void) {
  const [load, setLoad] = useState<Load>({ kind: "loading" });
  const [error, setError] = useState<ReefError | null>(null);
  // One that ended changes no more, and the daemon forgets it after 2 minutes.
  const over = load.kind === "loaded" && load.approval.state !== ApprovalState.PENDING;
  const again = useAgain(connection, error, true);
  useEffect(() => {
    if (over) return;
    const controller = new AbortController();
    const identities = client(connection, IdentityService);
    void (async () => {
      let wait = 0;
      for (;;) {
        const { approval } = await identities.getApproval(
          { id, ...(wait > 0 ? { wait: durationFromMs(wait) } : {}) },
          { signal: controller.signal },
        );
        if (controller.signal.aborted || !approval) return;
        setError(null);
        setLoad({ kind: "loaded", approval });
        if (approval.state !== ApprovalState.PENDING) return;
        wait = Math.max(1_000, msLeft(approval, Date.now()));
      }
    })().catch((thrown: unknown) => {
      if (controller.signal.aborted) return;
      const error = reefError(thrown);
      if (isSessionInvalid(error)) onSessionInvalid();
      else if (error.reason === APPROVAL_NOT_FOUND) setLoad({ kind: "missing" });
      else {
        setError(error);
        setLoad((held) =>
          held.kind === "loaded" && isPassing(error) ? held : { kind: "failed", error },
        );
      }
    });
    return () => {
      controller.abort();
    };
  }, [connection, again, over, id, onSessionInvalid]);
  return [load, setLoad] as const;
}

/** The change the approval is for, for its description and author. */
function useChange(connection: Connection, approval: Approval | null) {
  const repo = approval?.repoId ?? "";
  const commit = approval?.commitId ?? "";
  const [change, setChange] = useState<{ commit: string; change: Change | null } | null>(null);
  const [error, setError] = useState<ReefError | null>(null);
  const again = useAgain(connection, error, true);
  useEffect(() => {
    if (!commit) return;
    let current = true;
    client(connection, ChangeService)
      .getChange({ repo, revision: commit, skipSnapshot: true })
      .then(
        (response) => {
          if (!current) return;
          setError(null);
          setChange({ commit, change: response.change ?? null });
        },
        (thrown: unknown) => {
          if (!current) return;
          const error = reefError(thrown);
          setError(error);
          setChange((held) =>
            held?.commit === commit && isPassing(error) ? held : { commit, change: null },
          );
        },
      );
    return () => {
      current = false;
    };
  }, [connection, again, repo, commit]);
  return change?.commit === commit ? change.change : undefined;
}

/** At most this many unlanded changes below are named on the page. */
const BELOW = 20;

/**
 * The changes below the approval's that have not landed on `trunk()`: the
 * tree the sign-off covers holds their work too. `undefined` while
 * reading, `null` when the log could not be read.
 */
function useBelow(connection: Connection, approval: Approval | null) {
  const repo = approval?.repoId ?? "";
  const commit = approval?.commitId ?? "";
  const [below, setBelow] = useState<{ commit: string; changes: Change[] | null } | null>(null);
  const [error, setError] = useState<ReefError | null>(null);
  const again = useAgain(connection, error, true);
  useEffect(() => {
    if (!commit) return;
    let current = true;
    client(connection, ChangeService)
      .listChanges({
        repo,
        revset: `trunk()..${commit} ~ ${commit}`,
        skipSnapshot: true,
        pageSize: BELOW,
      })
      .then(
        ({ entries }) => {
          const changes = entries.flatMap((entry) => (entry.change ? [entry.change] : []));
          if (!current) return;
          setError(null);
          setBelow({ commit, changes });
        },
        (thrown: unknown) => {
          if (!current) return;
          const error = reefError(thrown);
          setError(error);
          setBelow((held) =>
            held?.commit === commit && isPassing(error) ? held : { commit, changes: null },
          );
        },
      );
    return () => {
      current = false;
    };
  }, [connection, again, repo, commit]);
  return below?.commit === commit ? below.changes : undefined;
}

function msLeft(approval: Approval, now: number): number {
  return approval.expiresAt ? timestampMs(approval.expiresAt) - now : 0;
}

/** Now, at each second while `ticking`: the time left is what the person decides by. */
function useNow(ticking: boolean): number {
  const [now, setNow] = useState(Date.now);
  useEffect(() => {
    if (!ticking) return;
    return after(1_000 - (now % 1_000), () => {
      setNow(Date.now());
    });
  }, [ticking, now]);
  return now;
}

export function timeLeft(ms: number): string {
  const seconds = Math.max(0, Math.ceil(ms / 1_000));
  const minutes = Math.floor(seconds / 60);
  const rest = seconds % 60;
  if (minutes === 0) return `${String(rest)} s`;
  return rest === 0 ? `${String(minutes)} min` : `${String(minutes)} min ${String(rest)} s`;
}

/**
 * Who asked, as far as the daemon can tell: the web listener or the socket,
 * and the agent name and workspace a command declared, which it may make up.
 */
function askedBy(approval: Approval): string {
  if (approval.requestedThrough === "web") return "a web UI session";
  const agent = approval.requestedByAgent
    ? `a command that says it is ${approval.requestedByAgent}`
    : "a command on this machine";
  return approval.requestedWorkspace
    ? `${agent}, in workspace ${approval.requestedWorkspace}`
    : agent;
}

function Facts({
  approval,
  change,
  below,
}: {
  approval: Approval;
  change: Change | null | undefined;
  below: Change[] | null | undefined;
}) {
  const subject = change?.description.split("\n")[0]?.trim() ?? "";
  return (
    <dl className="facts">
      <dt>Signs as</dt>
      <dd>{approval.identity}</dd>
      <dt>At</dt>
      <dd className="mono">{window.location.origin}</dd>
      <dt>Sign-off</dt>
      <dd>
        {approval.kind}, {approval.result}
        {approval.target ? (
          <>
            {" "}
            on target <span className="mono">{approval.target}</span>
          </>
        ) : (
          ", the whole tree"
        )}
      </dd>
      <dt>Change</dt>
      <dd>
        {change === undefined ? (
          <span className="secondary">Reading the change.</span>
        ) : (
          <span className={subject ? undefined : "secondary"}>
            {subject || (change ? "(no description)" : "The change could not be read.")}
          </span>
        )}
        <br />
        <span className="mono secondary">
          {shortId(reverseHex(approval.changeId))} · commit {shortId(approval.commitId)}
        </span>
      </dd>
      {change?.author && (
        <>
          <dt>Author</dt>
          <dd>
            {change.author.name} &lt;{change.author.email}&gt;
          </dd>
        </>
      )}
      <dt>Repository</dt>
      <dd className="mono">{approval.repoId}</dd>
      <dt>Tree</dt>
      <dd>
        <span className="mono">{approval.treeId}</span>
        <br />
        <span className="secondary">
          The sign-off is about this whole tree, changes below this one that have not landed
          included.
        </span>
      </dd>
      {below === undefined && (
        <>
          <dt>Also covers</dt>
          <dd className="secondary">Reading the changes below.</dd>
        </>
      )}
      {below === null && (
        <>
          <dt>Also covers</dt>
          <dd>
            The changes below could not be read; the sign-off covers any that have not landed.
          </dd>
        </>
      )}
      {below && below.length > 0 && (
        <>
          <dt>Also covers</dt>
          <dd>
            {below.length === BELOW ? `${String(BELOW)} or more` : below.length} change
            {below.length === 1 ? "" : "s"} below that have not landed:
            <ul className="below">
              {below.map((each) => (
                <li key={each.commitId}>
                  <span className="mono">{shortId(reverseHex(each.changeId))}</span>{" "}
                  {each.description.split("\n")[0]?.trim() || "(no description)"}
                </li>
              ))}
            </ul>
          </dd>
        </>
      )}
      <dt>Asked by</dt>
      <dd>{askedBy(approval)}</dd>
    </dl>
  );
}

function Ended({ approval }: { approval: Approval }) {
  switch (approval.state) {
    case ApprovalState.APPROVED:
      return (
        <p role="status">
          Approved{approval.passkey ? ` with the passkey “${approval.passkey.label}”` : ""}. The
          sign-off is added by{" "}
          {approval.requestedThrough === "web" ? "the page that asked" : "the command that asked"}.
        </p>
      );
    case ApprovalState.DECLINED:
      return (
        <p role="status">
          Declined{approval.declinedReason ? `: ${approval.declinedReason}` : ""}. Nothing was
          approved.
        </p>
      );
    default:
      return <p role="status">This approval expired. Nothing was approved.</p>;
  }
}

export function Approve({
  connection,
  id,
  onSessionInvalid,
}: {
  connection: Connection;
  id: string;
  onSessionInvalid: () => void;
}) {
  const [load, setLoad] = useApproval(connection, id, onSessionInvalid);
  const approval = load.kind === "loaded" ? load.approval : null;
  const change = useChange(connection, approval);
  const below = useBelow(connection, approval);
  const [action, setAction] = useState<Action>({ kind: "idle" });
  const pending = approval?.state === ApprovalState.PENDING;
  const now = useNow(pending);

  if (load.kind === "loading") return <p className="secondary">Reading the approval.</p>;
  if (load.kind === "missing") {
    return (
      <section className="approval">
        <h1>No such approval</h1>
        <p>
          The daemon holds no approval <span className="mono">{id}</span>. It ended over 2 minutes
          ago, the daemon restarted, or it was never requested.
        </p>
      </section>
    );
  }
  if (load.kind === "failed") return <ErrorView error={load.error} />;
  const shown = load.approval;
  const left = msLeft(shown, now);
  const open = pending && left > 0;
  const busy = action.kind === "approving" || action.kind === "declining";

  async function run(kind: "approving" | "declining") {
    setAction({ kind });
    try {
      const resolved =
        kind === "approving"
          ? await approve(connection, shown)
          : await decline(connection, shown.id);
      setLoad({ kind: "loaded", approval: resolved });
      setAction({ kind: "idle" });
    } catch (thrown) {
      const failed = failure(thrown);
      if (failed.kind === "daemon" && isSessionInvalid(failed.error)) onSessionInvalid();
      else setAction({ kind: "failed", failure: failed });
    }
  }

  return (
    <section className="approval">
      <h1>Approve a sign-off</h1>
      <p className="secondary">
        One passkey touch approves exactly this sign-off, on this tree of this change, and nothing
        else.
      </p>
      <Facts approval={shown} change={change} below={below} />
      {pending ? (
        <>
          <p role="timer">
            {open
              ? `${timeLeft(left)} left, until ${clock(shown.expiresAt).text}`
              : "Expired. Nothing was approved."}
          </p>
          <p className="actions">
            <button
              type="button"
              className="primary"
              disabled={!open || busy || !hasPasskeySupport()}
              onClick={() => void run("approving")}
            >
              Approve
            </button>
            <button
              type="button"
              disabled={!open || busy || device() !== null}
              onClick={() => void run("declining")}
            >
              Decline
            </button>
          </p>
          {action.kind === "approving" && (
            <p className="secondary">Touch your passkey to approve.</p>
          )}
          {open && !hasPasskeySupport() && <NoPasskeysHere />}
        </>
      ) : (
        <Ended approval={shown} />
      )}
      {action.kind === "failed" && <FailureView failure={action.failure} />}
    </section>
  );
}
