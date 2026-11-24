// The proposal page's comments (R3.31, R3.12, R3.32): the thread, oldest
// first with answers under what they answer; an instruction waiting for the
// person's confirmation with its estimate; a help request's Answer box; and
// a composer whose default is a note. "Send as an instruction" asks the
// agent to act, which spends compute, so the estimate comes back first and
// the instruction waits for Confirm, as on the CLI.
import { useState, type SubmitEvent } from "react";
import { client, type Connection } from "../api/connection";
import { device, DEVICE_READS } from "../device";
import { CommentKind, HelpState, InstructionState } from "../gen/reef/api/v1/comment_event_pb";
import {
  CommentService,
  LineSide,
  type Comment,
  type InstructionEstimate,
} from "../gen/reef/api/v1/comments_pb";
import { TransitionActor } from "../gen/reef/api/v1/lifecycle_pb";
import { Time } from "../ui/TimeLabel";
import { plural } from "../ui/words";
import { Answer, type RunAction } from "./AnswerBox";
import type { LineAnchor } from "./ProposalDiff";

export type { RunAction } from "./AnswerBox";

const INSTRUCTION_WORDS: Record<InstructionState, string> = {
  [InstructionState.UNSPECIFIED]: "unknown",
  [InstructionState.AWAITING_CONFIRMATION]: "waiting for your confirmation",
  [InstructionState.PENDING]: "waiting for the agent",
  [InstructionState.DELIVERED]: "delivered to the agent",
  [InstructionState.RUNNING]: "the agent is working on it",
  [InstructionState.DONE]: "done",
  [InstructionState.FAILED]: "failed",
  [InstructionState.CANCELLED]: "cancelled",
};

const HELP_WORDS: Record<HelpState, string> = {
  [HelpState.UNSPECIFIED]: "unknown",
  [HelpState.OPEN]: "waiting for an answer",
  [HelpState.ANSWERED]: "answered",
  [HelpState.WITHDRAWN]: "withdrawn",
};

function seconds(ms: bigint): string {
  const s = Number(ms) / 1000;
  return s < 90 ? `${s.toFixed(0)} s` : `${(s / 60).toFixed(0)} min`;
}

/** The estimate as sentences: what Reef recorded that predicts its cost. */
export function estimateWords(estimate: InstructionEstimate | undefined): string[] {
  if (!estimate) return ["Reef has no estimate for it."];
  const words: string[] = [];
  if (estimate.checksKnown) {
    words.push(
      `Its checks: ${plural(estimate.checks, "job")}, ${seconds(estimate.checkWallMs)} of wall time.`,
    );
  }
  if (estimate.agentKnown) {
    words.push(
      `The agent: about ${seconds(estimate.agentWallMs)}, the median of ${plural(estimate.agentSamples, "earlier instruction")}.`,
    );
  }
  return [...words, ...estimate.unknown];
}

function author(comment: Comment): string {
  const who = comment.author?.identity || comment.author?.agent || "someone";
  return comment.author?.web ? `${who}, from the web UI` : who;
}

function anchorWords(comment: Comment): string {
  const anchor = comment.anchor;
  if (!anchor?.path) return "";
  const side = anchor.side === LineSide.OLD ? " (old side)" : "";
  const lines =
    anchor.startLine > 0
      ? anchor.endLine > anchor.startLine
        ? `:${String(anchor.startLine)}-${String(anchor.endLine)}`
        : `:${String(anchor.startLine)}`
      : "";
  return `${anchor.path}${lines}${side}`;
}

function CommentItem({
  comment,
  replies,
  repo,
  change,
  commit,
  connection,
  run,
  busy,
}: {
  comment: Comment;
  replies: Comment[];
  repo: string;
  change: string;
  commit: string;
  connection: Connection;
  run: RunAction;
  busy: boolean;
}) {
  const instruction = comment.instruction;
  const help = comment.help;
  const kind =
    comment.kind === CommentKind.INSTRUCTION
      ? comment.replyTo
        ? "answered"
        : "instructed"
      : comment.kind === CommentKind.HELP_REQUEST
        ? "asked for help"
        : "commented";
  const awaiting =
    instruction?.state === InstructionState.AWAITING_CONFIRMATION ? instruction : undefined;
  function move(to: InstructionState, words: string) {
    run(async () => {
      await client(connection, CommentService).updateInstruction({
        repo,
        id: comment.id,
        to,
        expectedState: InstructionState.AWAITING_CONFIRMATION,
      });
      return words;
    });
  }
  const where = anchorWords(comment);
  return (
    <li className="comment">
      <p className="comment-head">
        <strong>{author(comment)}</strong> {kind}
        {where && (
          <>
            {" "}
            on <span className="mono">{where}</span>
          </>
        )}
        <span className="secondary">
          {" · "}
          <Time at={comment.createdAt} />
          {comment.outdated && " · outdated: the lines changed since"}
          {instruction && ` · ${INSTRUCTION_WORDS[instruction.state]}`}
          {help && ` · ${HELP_WORDS[help.state]}`}
        </span>
      </p>
      {help?.question && <p className="pre-wrap">{help.question}</p>}
      {help?.tried && <p className="pre-wrap secondary">Tried: {help.tried}</p>}
      {comment.text && <p className="pre-wrap">{comment.text}</p>}
      {awaiting && (
        <div className="estimate" role="group" aria-label="Confirm the instruction">
          {estimateWords(awaiting.estimate).map((line) => (
            <p key={line} className="secondary">
              {line}
            </p>
          ))}
          <p className="actions">
            <button
              type="button"
              className="primary"
              disabled={busy}
              onClick={() => {
                move(InstructionState.PENDING, "Instruction confirmed: the agent gets it next.");
              }}
            >
              Confirm
            </button>
            <button
              type="button"
              disabled={busy}
              onClick={() => {
                move(InstructionState.CANCELLED, "Instruction cancelled.");
              }}
            >
              Cancel
            </button>
          </p>
        </div>
      )}
      {help?.state === HelpState.OPEN && device() && <p className="secondary">{DEVICE_READS}</p>}
      {help?.state === HelpState.OPEN && !device() && (
        <Answer
          repo={repo}
          change={change}
          commit={commit}
          help={comment}
          connection={connection}
          run={run}
          busy={busy}
        />
      )}
      {replies.length > 0 && (
        <ul className="comment-replies">
          {replies.map((reply) => (
            <CommentItem
              key={reply.id}
              comment={reply}
              replies={[]}
              repo={repo}
              change={change}
              commit={commit}
              connection={connection}
              run={run}
              busy={busy}
            />
          ))}
        </ul>
      )}
    </li>
  );
}

export function ProposalComments({
  repo,
  change,
  commit,
  comments,
  more,
  anchor,
  onClearAnchor,
  connection,
  run,
  busy,
  writable,
}: {
  repo: string;
  /** The change in jj's reverse hex: a comment names it and expects the commit shown. */
  change: string;
  /** The commit the person reviewed: comments are written on it. */
  commit: string;
  comments: Comment[];
  /** More comments than the page read. */
  more: boolean;
  anchor: LineAnchor | null;
  onClearAnchor: () => void;
  connection: Connection;
  run: RunAction;
  busy: boolean;
  /** False when the page is not on the proposal's current commit. */
  writable: boolean;
}) {
  const [text, setText] = useState("");
  const [instruct, setInstruct] = useState(false);
  const ids = new Set(comments.map((c) => c.id));
  const top = comments
    .filter((c) => !c.replyTo || !ids.has(c.replyTo))
    .sort((a, b) => Number(a.createdAt?.seconds ?? 0) - Number(b.createdAt?.seconds ?? 0));
  const replies = (id: string) => comments.filter((c) => c.replyTo === id);
  function submit(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    const at = anchor;
    run(async () => {
      await client(connection, CommentService).createComment({
        repo,
        revision: change,
        expectedCommitId: commit,
        kind: instruct ? CommentKind.INSTRUCTION : CommentKind.NOTE,
        text,
        actor: TransitionActor.HUMAN,
        skipSnapshot: true,
        anchor: at
          ? { path: at.path, side: at.side, startLine: at.line, endLine: at.line }
          : undefined,
      });
      setText("");
      onClearAnchor();
      return instruct
        ? "Instruction written. Check its estimate below, then confirm it."
        : "Comment added.";
    });
  }
  return (
    <section className="proposal-section" aria-labelledby="proposal-comments">
      <h3 id="proposal-comments">Comments</h3>
      {top.length === 0 && <p className="secondary">No comments yet.</p>}
      {more && (
        <p className="secondary">
          Showing the first {comments.length.toLocaleString("en-US")} comments;{" "}
          <code>reef comment list</code> has the rest.
        </p>
      )}
      {top.length > 0 && (
        <ul className="comments">
          {top.map((comment) => (
            <CommentItem
              key={comment.id}
              comment={comment}
              replies={replies(comment.id)}
              repo={repo}
              change={change}
              commit={commit}
              connection={connection}
              run={run}
              busy={busy || !writable}
            />
          ))}
        </ul>
      )}
      {writable && (
        <form className="comment-form" id="comment" onSubmit={submit}>
          <label>
            {anchor ? (
              <>
                Comment on{" "}
                <span className="mono">
                  {anchor.path}:{anchor.line}
                  {anchor.side === LineSide.OLD ? " (old side)" : ""}
                </span>
              </>
            ) : (
              "Comment"
            )}
            <textarea
              required
              value={text}
              onChange={(event) => {
                setText(event.target.value);
              }}
            />
          </label>
          {anchor && (
            <button type="button" className="link-button" onClick={onClearAnchor}>
              Comment on the whole change instead
            </button>
          )}
          <label className="check">
            <input
              type="checkbox"
              checked={instruct}
              onChange={(event) => {
                setInstruct(event.target.checked);
              }}
            />
            Send as an instruction: the agent acts on it, after you confirm its estimate
          </label>
          <p className="actions">
            <button type="submit" className="primary" disabled={busy || !text.trim()}>
              {instruct ? "Send the instruction" : "Comment"}
            </button>
          </p>
        </form>
      )}
    </section>
  );
}
