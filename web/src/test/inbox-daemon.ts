// An in-memory daemon for the inbox (R3.33): `GetInbox` answers with the
// sections the test holds, `MarkInboxSeen` records each mark, and
// `CreateComment` records an answer.
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { createRouterTransport } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import { CommentKind, HelpState } from "../gen/reef/api/v1/comment_event_pb";
import { CommentService, type CreateCommentRequest } from "../gen/reef/api/v1/comments_pb";
import {
  InboxItemSchema,
  InboxReason,
  InboxSectionSchema,
  InboxService,
  LandedDigestSchema,
  type GetInboxRequest,
  type InboxItem,
  type InboxMark,
  type InboxSection,
} from "../gen/reef/api/v1/inbox_pb";
import { OperationKind } from "../gen/reef/api/v1/operations_pb";
import { PresenceKind } from "../gen/reef/api/v1/presence_pb";

export const REPO = "a3dda866c7e8f343";
export const OTHER = "b4ee000000000001";

/** A help request item of `repo`, asked at `ms`. */
export function helpItem(id: string, ms: number, repo = REPO): InboxItem {
  return create(InboxItemSchema, {
    key: `help:${repo}:${id}`,
    repoId: repo,
    reason: InboxReason.HELP,
    headline: "claude@a asks for help with a decision",
    since: timestampFromMs(ms),
    changeId: "c".repeat(32),
    record: {
      case: "comment",
      value: {
        id,
        changeId: "c".repeat(32),
        kind: CommentKind.HELP_REQUEST,
        text: "Which config format?",
        help: { state: HelpState.OPEN, question: "TOML or YAML?", tried: "read the docs" },
      },
    },
  });
}

/** A proposal item of `repo` with `reason`, at `ms`. */
export function proposalItem(
  change: string,
  reason: InboxReason,
  ms: number,
  headline: string,
  repo = REPO,
): InboxItem {
  return create(InboxItemSchema, {
    key: `proposal:${repo}:${change}`,
    repoId: repo,
    reason,
    headline,
    since: timestampFromMs(ms),
    changeId: change,
    record: {
      case: "proposal",
      value: { changeId: change, change: { description: `Change ${change.slice(0, 4)}\n` } },
    },
  });
}

export function section(
  repo: string,
  init: {
    needsYouNow?: InboxItem[];
    needsYouNowTotal?: number;
    watching?: InboxItem[];
    watchingTotal?: number;
    landed?: MessageInitShape<typeof LandedDigestSchema>;
  } = {},
): InboxSection {
  return create(InboxSectionSchema, {
    repoId: repo,
    landed: init.landed ?? { fromSeq: 0n, throughSeq: 0n, count: 0n },
    needsYouNow: init.needsYouNow ?? [],
    needsYouNowTotal: init.needsYouNowTotal ?? 0,
    watching: init.watching ?? [],
    watchingTotal: init.watchingTotal ?? 0,
  });
}

export class InboxDaemon {
  sections: InboxSection[] = [section(REPO)];
  reads: GetInboxRequest[] = [];
  marks: InboxMark[] = [];
  answers: CreateCommentRequest[] = [];
  /** Thrown by `MarkInboxSeen`, as a remote token the daemon keeps no mark for gets. */
  refuseMarks: Error | null = null;
  markCalls = 0;

  connection(): Connection {
    const transport = createRouterTransport((router) => {
      router.service(InboxService, {
        getInbox: (request) => {
          this.reads.push(request);
          return {
            sections: this.sections,
            refreshOperations: [OperationKind.COMMENT, OperationKind.TRANSITION],
            refreshPresence: [PresenceKind.AUTOMATIC, PresenceKind.ATTENTION],
          };
        },
        markInboxSeen: (request) => {
          this.markCalls += 1;
          if (this.refuseMarks) throw this.refuseMarks;
          this.marks.push(...request.marks);
          return { marks: request.marks };
        },
      });
      router.service(CommentService, {
        createComment: (request) => {
          this.answers.push(request);
          return { comment: { id: "cmt_answer", kind: CommentKind.INSTRUCTION } };
        },
      });
    });
    return { transport, label: "test" };
  }
}
