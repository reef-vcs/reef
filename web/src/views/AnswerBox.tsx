// A help request's Answer box (R3.32), on the proposal page and in the inbox
// (R3.33): the answer is an instruction replying to the request, which the
// daemon routes to the session that asked and confirms as written, so the
// agent gets it as its next instruction. Through the web listener it is the
// `comment` row's person's answer (ADR 0086).
import { useState, type SubmitEvent } from "react";
import { client, type Connection } from "../api/connection";
import { CommentKind, InstructionState } from "../gen/reef/api/v1/comment_event_pb";
import { CommentService, type Comment } from "../gen/reef/api/v1/comments_pb";
import { TransitionActor } from "../gen/reef/api/v1/lifecycle_pb";

/** Runs a write as the page's one action; resolves to the words it ended with. */
export type RunAction = (write: () => Promise<string>) => void;

export function Answer({
  repo,
  change,
  commit,
  help,
  connection,
  run,
  busy,
}: {
  repo: string;
  change: string;
  commit: string;
  help: Comment;
  connection: Connection;
  run: RunAction;
  busy: boolean;
}) {
  const [text, setText] = useState("");
  function submit(event: SubmitEvent<HTMLFormElement>) {
    event.preventDefault();
    run(async () => {
      const { comment } = await client(connection, CommentService).createComment({
        repo,
        revision: change,
        expectedCommitId: commit,
        kind: CommentKind.INSTRUCTION,
        replyTo: help.id,
        text,
        actor: TransitionActor.HUMAN,
        skipSnapshot: true,
      });
      setText("");
      return comment?.instruction?.state === InstructionState.AWAITING_CONFIRMATION
        ? "This question was already answered: your reply waits for your Confirm below."
        : "Answer sent: the agent gets it as its next instruction.";
    });
  }
  return (
    <form className="comment-form" onSubmit={submit}>
      <label>
        Your answer
        <textarea
          required
          value={text}
          onChange={(event) => {
            setText(event.target.value);
          }}
        />
      </label>
      <button type="submit" disabled={busy || !text.trim()}>
        Answer
      </button>
    </form>
  );
}
