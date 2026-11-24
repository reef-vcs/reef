// An in-memory daemon for the proposal view's tests (R3.31): R3.43's
// identity daemon (passkeys, approvals, evidence) plus a repository with
// proposals, their transitions, a symbol diff, comments, landings and a
// policy verdict, recording every write the pages make.
import { create, type MessageInitShape } from "@bufbuild/protobuf";
import { timestampFromMs, timestampNow } from "@bufbuild/protobuf/wkt";
import { createRouterTransport } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import {
  ChangeService,
  ProposalEntrySchema,
  type ProposalEntry,
  type TransitionChangeRequest,
} from "../gen/reef/api/v1/changes_pb";
import { CommentKind, HelpState, InstructionState } from "../gen/reef/api/v1/comment_event_pb";
import {
  CommentSchema,
  CommentService,
  type Comment,
  type CreateCommentRequest,
  type UpdateInstructionRequest,
} from "../gen/reef/api/v1/comments_pb";
import {
  DiffService,
  GetDiffResponseSchema,
  type GetDiffRequest,
  type GetDiffResponse,
} from "../gen/reef/api/v1/diff_pb";
import {
  LandingOutcome,
  LandingService,
  type LandChangeRequest,
} from "../gen/reef/api/v1/landing_pb";
import {
  ChangeTransitionSchema,
  LifecycleState,
  type ChangeTransition,
} from "../gen/reef/api/v1/lifecycle_pb";
import {
  PolicyEvaluationSchema,
  PolicyService,
  type PolicyEvaluation,
  type StackMemberEvaluation,
} from "../gen/reef/api/v1/policy_pb";
import { ProvenanceService } from "../gen/reef/api/v1/provenance_pb";
import { RepositoryService } from "../gen/reef/api/v1/repositories_pb";
import { CHANGE, COMMIT, IdentityDaemon, REPO, TREE } from "./identity-daemon";

export { CHANGE, COMMIT, REPO, TREE };

/** A proposal entry of `REPO`, proposed by `claude@mac`. */
export function proposal(
  changeId: string,
  commitId: string,
  description: string,
  init: Exclude<MessageInitShape<typeof ProposalEntrySchema>, ProposalEntry> = {},
): ProposalEntry {
  return create(ProposalEntrySchema, {
    changeId,
    proposedBy: "claude@mac",
    lifecycle: {
      state: LifecycleState.PROPOSED,
      recorded: true,
      round: 1,
      commitId,
      destination: "main",
      last: { recordedAt: timestampFromMs(1_795_000_000_000) },
    },
    change: {
      changeId,
      commitId,
      treeId: TREE,
      description,
      lifecycle: { state: LifecycleState.PROPOSED, recorded: true },
    },
    ...init,
  });
}

type Method = "transitionChange" | "createComment" | "landChange" | "getDiff" | "listProposals";

export class ProposalDaemon extends IdentityDaemon {
  entries: ProposalEntry[] = [
    proposal(CHANGE, COMMIT, "Fix the lease renewal race\n\nRenew before the grace ends."),
  ];
  transitions: ChangeTransition[] = [];
  comments: Comment[] = [];
  /** The stack as the gate judges it, when it stops below the proposal. */
  members: StackMemberEvaluation[] = [];
  evaluation: PolicyEvaluation = create(PolicyEvaluationSchema, {});
  /** The symbol diff `GetDiff` answers with, and the hunks an expansion gets. */
  diff: GetDiffResponse = create(GetDiffResponseSchema, {});
  expand: (request: GetDiffRequest) => GetDiffResponse = () => create(GetDiffResponseSchema, {});
  readonly failures: Partial<Record<Method, Error>> = {};
  readonly declined: TransitionChangeRequest[] = [];
  readonly written: CreateCommentRequest[] = [];
  readonly moved: UpdateInstructionRequest[] = [];
  readonly landed: LandChangeRequest[] = [];
  readonly diffs: GetDiffRequest[] = [];

  override connection(): Connection {
    const transport = createRouterTransport((router) => {
      this.routes(router, { changes: false, policy: false });
      router.service(RepositoryService, {
        getRepository: () => ({ repository: { id: REPO, root: "/tmp/fixture" } }),
      });
      router.service(PolicyService, {
        evaluatePolicy: () => ({
          members: this.members,
          ...(this.members.length > 0
            ? {}
            : {
                evaluation: create(PolicyEvaluationSchema, {
                  decision: this.evaluation.decision,
                  rules: this.evaluation.rules,
                  verified: this.verified,
                }),
              }),
        }),
      });
      router.service(ProvenanceService, { listProvenance: () => ({ records: [] }) });
      router.service(ChangeService, {
        listProposals: (request) => {
          if (this.failures.listProposals) throw this.failures.listProposals;
          const wanted = request.changes;
          const states = request.states.length
            ? request.states
            : [LifecycleState.PROPOSED, LifecycleState.QUEUED];
          return {
            entries: this.entries.filter(
              (e) =>
                (wanted.length === 0 || wanted.some((w) => e.changeId.startsWith(w))) &&
                states.includes(e.lifecycle?.state ?? LifecycleState.UNSPECIFIED),
            ),
          };
        },
        listTransitions: () => ({ transitions: this.transitions }),
        transitionChange: (request) => {
          if (this.failures.transitionChange) throw this.failures.transitionChange;
          this.declined.push(request);
          const entry = this.entries.find((e) => e.change?.commitId === request.expectedCommitId);
          if (entry?.lifecycle) entry.lifecycle.state = request.to;
          if (entry?.change?.lifecycle) entry.change.lifecycle.state = request.to;
          const transition = create(ChangeTransitionSchema, {
            id: `trn_${String(this.declined.length).padStart(16, "0")}`,
            to: request.to,
            reason: request.reason,
            identity: "arsood",
            web: { session: "0123456789abcdef" },
            recordedAt: timestampNow(),
          });
          this.transitions = [transition, ...this.transitions];
          return { transition, transitions: [transition] };
        },
      });
      router.service(DiffService, {
        getDiff: (request) => {
          if (this.failures.getDiff) throw this.failures.getDiff;
          this.diffs.push(request);
          return request.symbols ? this.diff : this.expand(request);
        },
      });
      router.service(CommentService, {
        listComments: () => ({ comments: this.comments }),
        createComment: (request) => {
          if (this.failures.createComment) throw this.failures.createComment;
          this.written.push(request);
          // A reply to a help request answered meanwhile waits for its
          // Confirm, as the daemon writes it through the web.
          const late = this.comments.some(
            (c) => c.id === request.replyTo && c.help && c.help.state !== HelpState.OPEN,
          );
          const comment = create(CommentSchema, {
            id: `cmt_${String(this.written.length).padStart(16, "0")}`,
            changeId: CHANGE,
            kind: request.kind,
            text: request.text,
            replyTo: request.replyTo,
            author: { identity: "arsood", web: { session: "0123456789abcdef" } },
            createdAt: timestampNow(),
            ...(request.anchor ? { anchor: request.anchor } : {}),
            ...(request.kind === CommentKind.INSTRUCTION && (!request.replyTo || late)
              ? {
                  instruction: {
                    state: InstructionState.AWAITING_CONFIRMATION,
                    estimate: {
                      checksKnown: true,
                      checks: 2,
                      checkWallMs: 40_000n,
                      unknown: ["Model tokens and price are not visible to Reef."],
                    },
                  },
                }
              : {}),
          });
          this.comments = [...this.comments, comment];
          return { comment };
        },
        updateInstruction: (request) => {
          this.moved.push(request);
          const comment = this.comments.find((c) => c.id === request.id);
          if (comment?.instruction) comment.instruction.state = request.to;
          return { comment };
        },
      });
      router.service(LandingService, {
        landChange: (request) => {
          if (this.failures.landChange) throw this.failures.landChange;
          this.landed.push(request);
          return { landing: { id: "lnd_0000000000000001", outcome: LandingOutcome.QUEUED } };
        },
      });
    });
    return { transport, label: "test" };
  }
}
