// An in-memory daemon for the inbox (R3.33, #372): `frame()` is what
// `WatchInbox` would send next for the sections the test holds (every item
// again, the keys that left, and repositories no longer held as removed),
// `MarkInboxSeen` records each mark, `GetRepository` resolves a tab's
// repository, and `CreateComment` records an answer.
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromMs } from "@bufbuild/protobuf/wkt";
import { createRouterTransport } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import { CommentKind, HelpState } from "../gen/reef/api/v1/comment_event_pb";
import { CommentService, type CreateCommentRequest } from "../gen/reef/api/v1/comments_pb";
import {
  InboxBucket,
  InboxListing,
  InboxItemSchema,
  InboxReason,
  InboxSectionChangeSchema,
  InboxSectionSchema,
  InboxService,
  LandedDigestSchema,
  WatchInboxResponseSchema,
  type InboxItem,
  type InboxMark,
  type InboxSection,
  type InboxSectionChange,
  type WatchInboxResponse,
} from "../gen/reef/api/v1/inbox_pb";
import { RepositoryService } from "../gen/reef/api/v1/repositories_pb";

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
  marks: InboxMark[] = [];
  /** The keys last sent, by repository. */
  private sent = new Map<string, Set<string>>();
  answers: CreateCommentRequest[] = [];
  /** An answer waits on this before it resolves: a response slower than the stream. */
  holdAnswer: Promise<void> | null = null;
  /** Thrown by `MarkInboxSeen`, as a remote token the daemon keeps no mark for gets. */
  refuseMarks: Error | null = null;
  markCalls = 0;

  /** The next `WatchInbox` frame: the sections now, against what was sent. */
  frame(): WatchInboxResponse {
    const changes: InboxSectionChange[] = [];
    const held = new Set(this.sections.map((section) => section.repoId));
    for (const repoId of [...this.sent.keys()]) {
      if (held.has(repoId)) continue;
      changes.push(
        create(InboxSectionChangeSchema, {
          repoId,
          left: [...(this.sent.get(repoId) ?? [])],
          removed: true,
        }),
      );
      this.sent.delete(repoId);
    }
    for (const section of this.sections) {
      const items = [
        ...section.needsYouNow.map((item) => ({ ...item, bucket: InboxBucket.NEEDS_YOU_NOW })),
        ...section.watching.map((item) => ({ ...item, bucket: InboxBucket.WATCHING })),
      ];
      const keys = new Set(items.map((item) => item.key));
      const sent = this.sent.get(section.repoId);
      const left = [...(sent ?? [])].filter((key) => !keys.has(key));
      this.sent.set(section.repoId, keys);
      changes.push(
        create(InboxSectionChangeSchema, {
          repoId: section.repoId,
          items,
          left,
          needsYouNowTotal: section.needsYouNowTotal,
          watchingTotal: section.watchingTotal,
          landed: section.landed,
          error: section.error,
          listing: sent === undefined ? InboxListing.END : InboxListing.UNSPECIFIED,
        }),
      );
    }
    return create(WatchInboxResponseSchema, { sections: changes, synced: true });
  }

  connection(): Connection {
    const transport = createRouterTransport((router) => {
      router.service(RepositoryService, {
        getRepository: (request) => ({ repository: { id: request.repo } }),
      });
      router.service(InboxService, {
        markInboxSeen: (request) => {
          this.markCalls += 1;
          if (this.refuseMarks) throw this.refuseMarks;
          this.marks.push(...request.marks);
          return { marks: request.marks };
        },
      });
      router.service(CommentService, {
        createComment: async (request) => {
          if (this.holdAnswer) await this.holdAnswer;
          this.answers.push(request);
          return { comment: { id: "cmt_answer", kind: CommentKind.INSTRUCTION } };
        },
      });
    });
    return { transport, label: "test" };
  }
}
