// The proposal page (R3.31): a person reviewing what an agent proposed reads
// its intent, then its evidence, then what changed by definition, and
// accepts, declines or comments without leaving the page. "Approve, not
// read": most proposals are judged on intent and evidence, and the lines
// are one click away.
//
// The page holds the commit the person opened. When the agent amends the
// proposal, the page says so and Accept and Decline wait until the person
// moves to the new version: a sign-off is bound to one tree, and the page
// never signs or declines a tree nobody looked at. All text from the API is
// rendered as text.
import { useCallback, useEffect, useReducer, useState, useSyncExternalStore } from "react";
import { client, type Connection } from "../api/connection";
import { CommentKind, HelpState } from "../gen/reef/api/v1/comment_event_pb";
import { EvidenceResult, type Evidence } from "../gen/reef/api/v1/evidence_pb";
import { IdentityStatus } from "../gen/reef/api/v1/identity_pb";
import { ChangeService, type ProposalEntry } from "../gen/reef/api/v1/changes_pb";
import { LifecycleState, TransitionActor } from "../gen/reef/api/v1/lifecycle_pb";
import {
  PolicyRuleStatus,
  type PolicyEvaluation,
  type PolicyRuleResult,
} from "../gen/reef/api/v1/policy_pb";
import type { FrameScheduler } from "../live/frame";
import type { Live } from "../live/share";
import { failure, hasPasskeySupport } from "../sign/passkey";
import { isSessionInvalid } from "../api/errors";
import { changeState, lifecycleState } from "../state/states";
import { StateBadge } from "../state/StateBadge";
import { Copy } from "../ui/Copy";
import { useDevMode } from "../ui/dev-mode";
import { ErrorView } from "../ui/ErrorView";
import { FailureView } from "../ui/FailureView";
import { reverseHex, shortId } from "../ui/ids";
import { Link } from "../ui/router";
import { Time } from "../ui/TimeLabel";
import { plural } from "../ui/words";
import {
  accept,
  acceptBlocked,
  actionReducer,
  declineBlocked,
  STEP_WORDS,
  UNPROPOSED,
  type ActionName,
  type Step,
} from "./proposal-actions";
import { ProposalModel, type ProposalRead, type StackMember } from "./proposal-model";
import { inboxHref } from "./inbox-route";
import { proposalHref } from "./proposal-route";
import {
  automaticWords,
  changeLabel,
  DECISION_WORDS,
  heldWords,
  needWords,
  RULE_WORDS,
  transitionWords,
} from "./proposal-words";
import { ProposalComments, type RunAction } from "./ProposalComments";
import { ProposalDiff, type LineAnchor } from "./ProposalDiff";
import { presenceWords, useSigner } from "./SignOff";

/** The commit and tree the person is reviewing. */
interface Reviewed {
  commitId: string;
  treeId: string;
}

function reviewedOf(entry: ProposalEntry): Reviewed {
  return {
    commitId: entry.change?.commitId ?? entry.lifecycle?.commitId ?? "",
    treeId: entry.change?.treeId ?? "",
  };
}

function subjectOf(entry: ProposalEntry | undefined): string {
  return entry?.change?.description.split("\n")[0]?.trim() ?? "";
}

const RESULT_WORDS: Partial<Record<EvidenceResult, string>> = {
  [EvidenceResult.PASS]: "pass",
  [EvidenceResult.FAIL]: "fail",
  [EvidenceResult.ERROR]: "error",
};

const STATUS_WORDS: Partial<Record<IdentityStatus, string>> = {
  [IdentityStatus.EXPIRED]: "its key expired",
  [IdentityStatus.REVOKED]: "its key was revoked later",
  [IdentityStatus.INVALID]: "its certificate is invalid",
};

// ---- intent ----

function StackList({
  repo,
  stack,
  entry,
}: {
  repo: string;
  stack: StackMember[];
  entry: ProposalEntry;
}) {
  const members = [...stack].reverse();
  return (
    <>
      <p className="secondary">
        It is on {plural(stack.length, "change")} in review. Accepting it signs this change, and a
        sign-off counts for the changes below; it lands them with it. To accept only up to one of
        them, open it and accept it there.
      </p>
      <ol className="stack" aria-label="The stack, top first">
        <li aria-current="true">
          {entry.change && <StateBadge {...changeState(entry.change)} />}{" "}
          <strong>{subjectOf(entry) || "(no description)"}</strong>{" "}
          <span className="secondary">· this change</span>
        </li>
        {members.map((member) => {
          const dependency = entry.dependencies.find((d) => d.changeId === member.changeId);
          const state = member.entry?.change
            ? changeState(member.entry.change)
            : dependency?.lifecycle
              ? lifecycleState(dependency.lifecycle.state)
              : undefined;
          return (
            <li key={member.changeId}>
              {state && <StateBadge {...state} />}{" "}
              <Link to={proposalHref({ repo, change: member.changeId })}>
                {subjectOf(member.entry) || changeLabel(member.changeId)}
              </Link>
              {dependency?.proposedBy && (
                <span className="secondary"> · by {dependency.proposedBy}</span>
              )}
            </li>
          );
        })}
      </ol>
    </>
  );
}

function Intent({ repo, read }: { repo: string; read: ProposalRead }) {
  const entry = read.entry;
  const description = entry.change?.description.trimEnd() ?? "";
  const body = description.split("\n").slice(1).join("\n").trim();
  const context = read.provenance?.context;
  const held = heldWords(
    entry.dependencies,
    (id) =>
      read.stack.find((member) => member.changeId === id)?.entry?.lifecycle?.last?.reason ?? "",
  );
  return (
    <section className="proposal-section" aria-labelledby="proposal-intent">
      <h3 id="proposal-intent">Intent</h3>
      {body ? (
        <pre className="description">{body}</pre>
      ) : (
        <p className="secondary">The description has no body beyond its first line.</p>
      )}
      {context && (context.task || context.model || context.harness) && (
        <dl className="facts">
          {context.task && (
            <>
              <dt>Task</dt>
              <dd className="pre-wrap">{context.task}</dd>
            </>
          )}
          {(context.model || context.harness) && (
            <>
              <dt>Agent</dt>
              <dd>{[context.model, context.harness].filter(Boolean).join(" in ")}</dd>
            </>
          )}
          {context.instructionSha256 && (
            <>
              <dt>Instruction</dt>
              <dd>
                <span className="mono">{shortId(context.instructionSha256)}</span>
              </dd>
            </>
          )}
        </dl>
      )}
      {context && (
        <p className="secondary">
          The agent declared these; Reef records them and does not check them.
        </p>
      )}
      {held && (
        <p className="notice" role="status">
          {held}
        </p>
      )}
      {read.stack.length > 0 && <StackList repo={repo} stack={read.stack} entry={entry} />}
    </section>
  );
}

// ---- evidence ----

function Rule({ rule }: { rule: PolicyRuleResult }) {
  const signoff = rule.signoff;
  return (
    <li className={`rule ${rule.status === PolicyRuleStatus.SATISFIED ? "met" : "unmet"}`}>
      <span className="mono">{rule.name}</span>{" "}
      <span className="secondary">{RULE_WORDS[rule.status]}</span>
      {rule.message && <span>: {rule.message}</span>}
      {signoff && (
        <span className="secondary">
          {" "}
          · sign-offs {signoff.have} of {signoff.count}
          {signoff.presence && `, ${signoff.presence.replace("_", " ")}`}
          {signoff.signedBy.length > 0 && `, by ${signoff.signedBy.join(", ")}`}
        </span>
      )}
      {rule.missing.length > 0 && (
        <span className="secondary"> · missing {rule.missing.map(needWords).join(", ")}</span>
      )}
      {rule.failed.length > 0 && (
        <span className="secondary"> · failed {rule.failed.map(needWords).join(", ")}</span>
      )}
    </li>
  );
}

function EvidenceRow({ evidence, pinned }: { evidence: Evidence; pinned: boolean | undefined }) {
  const dev = useDevMode();
  const key = evidence.identity?.key;
  const who = key?.identity || `key ${shortId(evidence.keyId)}`;
  const status = evidence.identity?.status;
  const command = `reef evidence show ${evidence.evidenceId}`;
  return (
    <li>
      <span className="mono">{evidence.kind}</span>
      {evidence.target && <span className="mono"> {evidence.target}</span>} ·{" "}
      {RESULT_WORDS[evidence.result] ?? "unknown result"} · {who} · {presenceWords(evidence)}
      {pinned !== undefined &&
        (pinned
          ? ", pinned by this repository's policy"
          : ", not pinned by this repository's policy")}
      {status !== undefined && STATUS_WORDS[status] && ` · ${STATUS_WORDS[status]}`}
      <span className="secondary">
        {" · "}
        <Time at={evidence.addedAt} />
      </span>
      {dev && (
        <span className="command">
          {" "}
          <code>{command}</code> <Copy text={command} />
        </span>
      )}
    </li>
  );
}

function EvidenceSection({ read }: { read: ProposalRead }) {
  // A stack the gate stops below this change has no evaluation of its own:
  // the change that stops it is what the person needs to see.
  const stopped = read.evaluation ? undefined : read.members.find((member) => !member.passes);
  const evaluation: PolicyEvaluation | undefined = read.evaluation ?? stopped?.evaluation;
  const pins = new Map((evaluation?.verified ?? []).map((v) => [v.evidenceId, v.pinned]));
  const current = read.evidence.filter((e) => e.current);
  const earlier = read.evidence.filter((e) => !e.current);
  const rules = (evaluation?.rules ?? []).filter(
    (rule) => rule.status !== PolicyRuleStatus.NOT_APPLICABLE,
  );
  return (
    <section className="proposal-section" aria-labelledby="proposal-evidence">
      <h3 id="proposal-evidence">Evidence</h3>
      {read.policyError && <ErrorView error={read.policyError} />}
      {stopped && (
        <p>
          The stack stops below this change, at {changeLabel(stopped.changeId)}
          {stopped.message ? `: ${stopped.message}` : "."} Its rules:
        </p>
      )}
      {evaluation && <p>{DECISION_WORDS[evaluation.decision]}</p>}
      {rules.length > 0 && (
        <ul className="rules">
          {rules.map((rule) => (
            <Rule key={rule.name} rule={rule} />
          ))}
        </ul>
      )}
      {current.length === 0 ? (
        <p className="secondary">No evidence on this tree yet.</p>
      ) : (
        <ul className="activity" aria-label="Evidence on this tree">
          {current.map((evidence) => (
            <EvidenceRow
              key={evidence.evidenceId}
              evidence={evidence}
              pinned={pins.get(evidence.evidenceId)}
            />
          ))}
        </ul>
      )}
      {earlier.length > 0 && (
        <details>
          <summary>{plural(earlier.length, "object")} on earlier trees</summary>
          <ul className="activity">
            {earlier.map((evidence) => (
              <EvidenceRow key={evidence.evidenceId} evidence={evidence} pinned={undefined} />
            ))}
          </ul>
        </details>
      )}
    </section>
  );
}

// ---- actions ----

function DeclineForm({
  stacked,
  busy,
  onDecline,
  onCancel,
}: {
  stacked: boolean;
  busy: boolean;
  onDecline: (reason: string, stack: boolean) => void;
  onCancel: () => void;
}) {
  const [reason, setReason] = useState("");
  const [stack, setStack] = useState(false);
  return (
    <form
      className="decline-form"
      onSubmit={(event) => {
        event.preventDefault();
        onDecline(reason.trim(), stack);
      }}
    >
      <label>
        Why (the agent reads this as its next instruction)
        <textarea
          required
          value={reason}
          onChange={(event) => {
            setReason(event.target.value);
          }}
        />
      </label>
      {stacked && (
        <label className="check">
          <input
            type="checkbox"
            checked={stack}
            onChange={(event) => {
              setStack(event.target.checked);
            }}
          />
          Decline the changes below it too
        </label>
      )}
      <p className="actions">
        <button type="submit" disabled={busy || !reason.trim()}>
          Decline
        </button>
        <button type="button" onClick={onCancel} disabled={busy}>
          Keep it
        </button>
      </p>
    </form>
  );
}

export function Proposal({
  repo,
  change,
  connection,
  live,
  frame,
  onSessionInvalid,
}: {
  repo: string;
  change: string;
  connection: Connection;
  live: Live;
  frame: FrameScheduler;
  onSessionInvalid: () => void;
}) {
  const [model] = useState(
    () =>
      new ProposalModel(
        connection,
        live.operations,
        live.presence,
        frame,
        onSessionInvalid,
        repo,
        change,
      ),
  );
  useEffect(() => model.start(), [model]);
  const { read, error, fatal, repository } = useSyncExternalStore(
    model.subscribe,
    model.getSnapshot,
  );
  const signer = useSigner(connection, onSessionInvalid);
  const [action, dispatch] = useReducer(actionReducer, { kind: "idle" });
  const [reviewed, setReviewed] = useState<Reviewed | null>(null);
  const [declining, setDeclining] = useState(false);
  const [anchor, setAnchor] = useState<LineAnchor | null>(null);
  const repoId = repository?.id ?? repo;

  // The first read fixes what the person reviews.
  const current = read ? reviewedOf(read.entry) : null;
  if (current && reviewed === null) setReviewed(current);
  const subject = subjectOf(read?.entry);
  useEffect(() => {
    if (subject) document.title = `${subject} · Proposal — Reef`;
  }, [subject]);

  const busy = action.kind === "working";
  const run = useCallback(
    (name: ActionName, step: Step, write: (onStep: (step: Step) => void) => Promise<string>) => {
      dispatch({ type: "start", action: name, step });
      write((next) => {
        dispatch({ type: "step", step: next });
      }).then(
        (words) => {
          dispatch({ type: "done", words });
          model.refreshNow();
        },
        (thrown: unknown) => {
          const failed = failure(thrown);
          if (failed.kind === "daemon" && isSessionInvalid(failed.error)) onSessionInvalid();
          else dispatch({ type: "failed", failure: failed });
          model.refreshNow();
        },
      );
    },
    [model, onSessionInvalid],
  );
  const runComment: RunAction = useCallback(
    (write) => {
      run("comment", "writing", () => write());
    },
    [run],
  );

  if (fatal) {
    return (
      <div className="proposal">
        <ErrorView error={fatal} />
        <p>
          <Link to={inboxHref({ repo })}>The inbox</Link>
        </p>
      </div>
    );
  }
  if (!read || !reviewed || !current) {
    return (
      <div className="proposal">
        {error && <ErrorView error={error} />}
        <p className="secondary">Reading the proposal.</p>
      </div>
    );
  }

  const entry = read.entry;
  const pinned = reviewed;
  const state = entry.lifecycle?.state ?? LifecycleState.UNSPECIFIED;
  const amended = current.commitId !== reviewed.commitId;
  const unproposed = entry.lifecycle?.amended ?? false;
  const acceptOff = acceptBlocked({
    state,
    amended,
    unproposed,
    tree: reviewed.treeId,
    signer:
      signer.kind === "loaded"
        ? { name: signer.identity.name, passkeys: signer.passkeys }
        : undefined,
    supported: hasPasskeySupport(),
  });
  const declineOff = declineBlocked(state, amended);
  const badge = entry.change ? changeState(entry.change) : lifecycleState(state);
  const status = automaticWords(entry.automatic);
  const openHelp = read.comments.filter(
    (c) => c.kind === CommentKind.HELP_REQUEST && c.help?.state === HelpState.OPEN,
  ).length;

  function onAccept() {
    run("accept", "requesting", (onStep) =>
      accept(connection, { repo: repoId, changeId: entry.changeId, ...pinned }, onStep),
    );
  }

  function onDecline(reason: string, stack: boolean) {
    run("decline", "writing", async () => {
      const response = await client(connection, ChangeService).transitionChange({
        repo: repoId,
        // The change by its id, so an amend after the page's read refuses it.
        revision: reverseHex(entry.changeId),
        to: LifecycleState.DECLINED,
        reason,
        stack,
        expectedCommitId: pinned.commitId,
        expectedState: state,
        actor: TransitionActor.HUMAN,
        skipSnapshot: true,
      });
      setDeclining(false);
      const count = response.transitions.length;
      return count > 1 ? `Declined, with ${plural(count - 1, "change")} below it.` : "Declined.";
    });
  }

  return (
    <div className="proposal">
      <p className="secondary crumb-line">
        <Link to={inboxHref({ repo })}>Inbox</Link> ›{" "}
        <span className="mono">{changeLabel(entry.changeId)}</span>
      </p>
      <header className="proposal-head">
        <h2>
          {badge && <StateBadge {...badge} />}
          <span className={subject ? undefined : "secondary"}>{subject || "(no description)"}</span>
        </h2>
        <p className="secondary">
          {entry.proposedBy ? `Proposed by ${entry.proposedBy}` : "Proposed"}
          {entry.lifecycle?.destination && ` for ${entry.lifecycle.destination}`}
          {" · "}
          <Time at={entry.lifecycle?.last?.recordedAt} />
          {entry.lifecycle &&
            entry.lifecycle.round > 1 &&
            ` · round ${String(entry.lifecycle.round)}`}
        </p>
        {status && <p className="proposal-status">{status}</p>}
        {openHelp > 0 && (
          <p className="notice">
            The agent asked for help: {plural(openHelp, "question")} below waits for your answer.
          </p>
        )}
      </header>
      {error && <ErrorView error={error} />}
      {unproposed && <p className="notice">{UNPROPOSED}</p>}
      {amended && (
        <p className="notice" role="status">
          The agent changed this proposal since you opened it.{" "}
          <button
            type="button"
            onClick={() => {
              setReviewed(current);
              setAnchor(null);
            }}
          >
            Review the new version
          </button>
        </p>
      )}
      <div className="proposal-actions" role="group" aria-label="Decide">
        <button
          type="button"
          className="primary"
          disabled={busy || acceptOff !== null}
          title={acceptOff ?? undefined}
          onClick={onAccept}
        >
          {read.stack.length > 0
            ? `Accept with the ${plural(read.stack.length, "change")} below`
            : "Accept"}
        </button>
        <button
          type="button"
          disabled={busy || declineOff !== null}
          title={declineOff ?? undefined}
          aria-expanded={declining}
          onClick={() => {
            setDeclining((open) => !open);
          }}
        >
          Decline
        </button>
        <a className="button-link" href="#comment">
          Comment
        </a>
        {acceptOff && !unproposed && action.kind !== "working" && (
          <span className="secondary">
            {acceptOff}
            {signer.kind === "loaded" &&
              signer.passkeys === 0 &&
              state === LifecycleState.PROPOSED && (
                <>
                  {" "}
                  <Link to="/passkeys">Add one</Link> to accept with a touch.
                </>
              )}
          </span>
        )}
        {!acceptOff && action.kind === "idle" && (
          <span className="secondary">
            One passkey touch signs off tree{" "}
            <span className="mono">{shortId(reviewed.treeId)}</span>
            {signer.kind === "loaded" && ` as ${signer.identity.name}`}, then asks to land it.
          </span>
        )}
      </div>
      {signer.kind === "failed" && <ErrorView error={signer.error} />}
      {declining && (
        <DeclineForm
          stacked={read.stack.length > 0}
          busy={busy}
          onDecline={onDecline}
          onCancel={() => {
            setDeclining(false);
          }}
        />
      )}
      {action.kind === "working" && <p className="secondary">{STEP_WORDS[action.step]}</p>}
      {action.kind === "done" && <p role="status">{action.words}</p>}
      {action.kind === "failed" && <FailureView failure={action.failure} />}
      <Intent repo={repo} read={read} />
      <EvidenceSection read={read} />
      <ProposalDiff
        connection={connection}
        repo={repoId}
        commit={reviewed.commitId}
        onAnchor={amended ? undefined : setAnchor}
        onSessionInvalid={onSessionInvalid}
      />
      <ProposalComments
        repo={repoId}
        change={reverseHex(entry.changeId)}
        commit={reviewed.commitId}
        comments={read.comments}
        more={read.moreComments}
        anchor={anchor}
        onClearAnchor={() => {
          setAnchor(null);
        }}
        connection={connection}
        run={runComment}
        busy={busy}
        writable={!amended}
      />
      <section className="proposal-section" aria-labelledby="proposal-history">
        <details>
          <summary id="proposal-history">History ({read.transitions.length})</summary>
          <ul className="activity">
            {read.transitions.map((transition) => (
              <li key={transition.id}>
                {transitionWords(transition)}{" "}
                <span className="secondary">
                  · <Time at={transition.recordedAt} />
                </span>
              </li>
            ))}
          </ul>
        </details>
      </section>
    </div>
  );
}
