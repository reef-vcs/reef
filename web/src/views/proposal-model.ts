// The data of the proposal page (R3.31).
//
// It reads live from the shared operation stream, as the workspaces page
// does: nothing is read until the stream is live, so an operation recorded
// after a read arrives on it and asks for another, and refreshes are paced
// (live/paced.ts). Any operation in the repository asks for one: a comment,
// a transition and a landing are operations, and so is the automatic
// service's queueing. Evidence is not: a check's result or a sign-off
// records no operation. Where the destination's policy judges the proposal,
// its AUTOMATIC presence entry is replaced when new evidence changes its
// status (R3.33, #362), and that asks for a read; where nothing judges it
// (status `OFF`), a read also follows the last one by `POLL_MS` while it is
// in review, paced the same way. `ListProposals` has no `at_operation`, so
// the page never reads at an operation.
//
// The proposal page reads the proposal as a unit, in this order of the page:
// the entry (`ListProposals` for its change, any state), the changes below it
// (R3.11's `dependencies`, with their entries), the policy's verdict
// (`EvaluatePolicy`, over the stack when it has one), the evidence, the
// comments, the transitions and the declared provenance. A failed policy read
// is shown in its section and does not fail the page.
import { Code } from "@connectrpc/connect";
import { client, type Connection } from "../api/connection";
import { isPassing, isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { streamsOf } from "../api/streams";
import { AutomaticState, ChangeService, type ProposalEntry } from "../gen/reef/api/v1/changes_pb";
import { CommentService, type Comment } from "../gen/reef/api/v1/comments_pb";
import { EvidenceService, type Evidence } from "../gen/reef/api/v1/evidence_pb";
import { LifecycleState, type ChangeTransition } from "../gen/reef/api/v1/lifecycle_pb";
import {
  PolicyService,
  type PolicyEvaluation,
  type StackMemberEvaluation,
} from "../gen/reef/api/v1/policy_pb";
import { ProvenanceService, type ProvenanceRecord } from "../gen/reef/api/v1/provenance_pb";
import { RepositoryService, type Repository } from "../gen/reef/api/v1/repositories_pb";
import { PresenceKind, type Presence } from "../gen/reef/api/v1/presence_pb";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import { stateSignature } from "../live/kinds";
import type { OperationBatch, OperationStore } from "../live/operations";
import { after, type Cancel } from "../live/backoff";
import { Paced } from "../live/paced";
import type { PresenceStore } from "../live/presence";
import { hexFromEither } from "../ui/ids";

/** Every lifecycle state a proposal page may find its change in. */
export const ANY_STATE = [
  LifecycleState.DRAFT,
  LifecycleState.PROPOSED,
  LifecycleState.QUEUED,
  LifecycleState.LANDED,
  LifecycleState.DECLINED,
  LifecycleState.WITHDRAWN,
  LifecycleState.SUPERSEDED,
];

/** How long after a read the next one comes while something is in review: evidence records no operation. */
export const POLL_MS = 10_000;

/** Comments the page reads at most, in pages; past it the page says so. */
export const COMMENTS_SHOWN = 1_000;

export const NOT_A_PROPOSAL = "NOT_A_PROPOSAL";

/** A read of one repository, refreshed on its operations, one in flight. */
abstract class RepoReader<S extends { version: number }> {
  protected snapshot: S;
  protected repoId: string | undefined;
  protected stopped = false;
  private readonly notifier: FrameNotifier;
  private readonly paced: Paced;
  private requested = false;
  private waiting = false;
  private poll: Cancel | undefined;

  private signature = "";

  constructor(
    protected readonly connection: Connection,
    private readonly operations: OperationStore,
    private readonly presence: PresenceStore,
    frame: FrameScheduler,
    protected readonly onSessionInvalid: () => void,
    protected readonly repo: string,
    initial: S,
  ) {
    this.notifier = new FrameNotifier(frame);
    this.paced = new Paced(() => this.refresh());
    this.snapshot = initial;
  }

  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  getSnapshot = (): S => this.snapshot;

  start(): () => void {
    const unsubscribe = this.operations.subscribe((batch) => {
      this.onBatch(batch);
    });
    const unsubscribePresence = this.presence.subscribe(() => {
      this.onPresence();
    });
    const unsubscribeResume = streamsOf(this.connection).onResume(() => {
      if (this.requested) this.paced.requestNow();
    });
    if (this.operations.live) this.requestFirst();
    return () => {
      this.stopped = true;
      unsubscribe();
      unsubscribeResume();
      unsubscribePresence();
      this.paced.stop();
      this.poll?.();
    };
  }

  /** A method, so a check after an await is not narrowed away. */
  protected isStopped(): boolean {
    return this.stopped;
  }

  /** Reads again now: after the page's own write, which the stream also reports. */
  refreshNow() {
    this.paced.requestNow();
  }

  /** Whether all of it was answered. */
  protected abstract read(repo: string): Promise<boolean>;

  /** Whether evidence may arrive with no event: in review, and judged by nothing. */
  protected abstract inReview(): boolean;

  /** The live presence entries whose change asks for a read, as one string. */
  protected abstract presenceSignature(entries: Iterable<Presence>): string;

  protected update(patch: Partial<S>) {
    this.snapshot = { ...this.snapshot, ...patch, version: this.snapshot.version + 1 };
    this.notifier.notify();
  }

  /** A failed read: an invalid session ends the page; any other error is the model's. */
  protected failed(thrown: unknown): ReefError | null {
    const error = reefError(thrown);
    if (isSessionInvalid(error)) {
      this.onSessionInvalid();
      return null;
    }
    return error;
  }

  /** The next read `POLL_MS` from now while in review; a hidden tab only waits again. */
  private arm() {
    this.poll?.();
    this.poll = undefined;
    if (this.isStopped() || !this.inReview()) return;
    this.poll = after(POLL_MS, () => {
      this.poll = undefined;
      if (typeof document !== "undefined" && document.visibilityState === "hidden") this.arm();
      else this.paced.request();
    });
  }

  private onPresence() {
    const signature = this.presenceSignature(this.presence.entries.values());
    if (signature === this.signature) return;
    this.signature = signature;
    if (this.requested && this.operations.live) this.paced.request();
  }

  private requestFirst() {
    if (this.requested) return;
    this.requested = true;
    this.paced.request();
  }

  private onBatch(batch: OperationBatch) {
    let ask = batch.reset || this.waiting;
    for (const operation of batch.operations) {
      if (this.repoId === undefined || operation.repoId === this.repoId) ask = true;
    }
    if (batch.live && !this.requested) {
      this.requestFirst();
      return;
    }
    if (!ask || !this.requested) return;
    this.waiting = !batch.live;
    if (batch.live) this.paced.request();
  }

  private refresh(): Promise<boolean> | undefined {
    if (this.stopped) return undefined;
    if (!this.operations.live) {
      this.waiting = true;
      return undefined;
    }
    return this.readRepo();
  }

  private async readRepo(): Promise<boolean> {
    try {
      if (this.repoId === undefined) {
        const { repository } = await client(this.connection, RepositoryService).getRepository({
          repo: this.repo,
        });
        if (this.isStopped()) return true;
        this.repoId = repository?.id || this.repo;
        this.onRepository(repository);
      }
      return await this.read(this.repoId);
    } catch (thrown) {
      if (this.isStopped()) return true;
      const error = this.failed(thrown);
      this.onError(error);
      return error === null || !isPassing(error);
    } finally {
      this.arm();
    }
  }

  protected abstract onRepository(repository: Repository | undefined): void;

  protected abstract onError(error: ReefError | null): void;
}

// ---- The proposal page ----

/** A change below the proposal (R3.11), with its entry when it has one. */
export interface StackMember {
  changeId: string;
  entry: ProposalEntry | undefined;
}

export interface ProposalRead {
  entry: ProposalEntry;
  /** The changes below it, bottom first. */
  stack: StackMember[];
  /** Its own evaluation; for a stack, as judged in it, and unset when the stack stops below it. */
  evaluation: PolicyEvaluation | undefined;
  /** For a stack, every change of it as the gate would judge it, bottom first. */
  members: StackMemberEvaluation[];
  /** The policy could not be read; the rest of the page stands. */
  policyError: ReefError | null;
  evidence: Evidence[];
  /** Oldest first. */
  comments: Comment[];
  /** More than `COMMENTS_SHOWN`. */
  moreComments: boolean;
  /** Newest first. */
  transitions: ChangeTransition[];
  /** The newest provenance record of the change, if any (R3.04). */
  provenance: ProvenanceRecord | undefined;
}

export interface ProposalSnapshot {
  /** Null until the first read answers. */
  read: ProposalRead | null;
  repository: Repository | undefined;
  /** A failed refresh, or a first read the daemon has not answered; the last good read stays. */
  error: ReefError | null;
  /** The change is not a recorded proposal, or the repository is unknown: the page is replaced. */
  fatal: ReefError | null;
  version: number;
}

export class ProposalModel extends RepoReader<ProposalSnapshot> {
  constructor(
    connection: Connection,
    operations: OperationStore,
    presence: PresenceStore,
    frame: FrameScheduler,
    onSessionInvalid: () => void,
    repo: string,
    private readonly change: string,
  ) {
    super(connection, operations, presence, frame, onSessionInvalid, repo, {
      read: null,
      repository: undefined,
      error: null,
      fatal: null,
      version: 0,
    });
  }

  protected onRepository(repository: Repository | undefined) {
    this.update({ repository });
  }

  protected onError(error: ReefError | null) {
    if (!error) return;
    if (this.snapshot.read === null && !isPassing(error)) this.update({ fatal: error });
    else this.update({ error });
  }

  protected inReview(): boolean {
    const entry = this.snapshot.read?.entry;
    const state = entry?.lifecycle?.state;
    const open = state === LifecycleState.PROPOSED || state === LifecycleState.QUEUED;
    const judged = entry?.automatic !== undefined && entry.automatic.state !== AutomaticState.OFF;
    return open && !judged;
  }

  protected presenceSignature(entries: Iterable<Presence>): string {
    const changeId = this.snapshot.read?.entry.changeId;
    if (!changeId) return "";
    const mine = [...entries].filter((p) => p.changeId === changeId);
    return stateSignature(mine, PresenceKind.AUTOMATIC, "");
  }

  /** The change's comments, oldest first, page by page up to `COMMENTS_SHOWN`. */
  private async comments(repo: string, changeId: string) {
    const comments: Comment[] = [];
    let token = "";
    do {
      const page = await client(this.connection, CommentService).listComments({
        repo,
        changes: [changeId],
        pageSize: 200,
        pageToken: token,
      });
      comments.push(...page.comments);
      token = page.nextPageToken;
    } while (token && comments.length < COMMENTS_SHOWN);
    return { comments, more: token !== "" };
  }

  protected async read(repo: string): Promise<boolean> {
    const changes = client(this.connection, ChangeService);
    const listed = await changes.listProposals({
      repo,
      changes: [this.change],
      states: ANY_STATE,
    });
    if (this.isStopped()) return true;
    const hex = hexFromEither(this.change) ?? this.change;
    const entry =
      listed.entries.find((e) => e.changeId === hex) ??
      (listed.entries.length === 1 ? listed.entries[0] : undefined);
    if (!entry) {
      this.onError({
        code: Code.NotFound,
        kind: "not_found",
        reason: NOT_A_PROPOSAL,
        message:
          listed.entries.length > 1
            ? `${this.change} names more than one change in review.`
            : `${this.change} was never proposed in this repository.`,
        hint: "Open a proposal from the inbox.",
        next: [],
        metadata: {},
      });
      return true;
    }
    const changeId = entry.changeId;
    const below = entry.dependencies.map((d) => d.changeId);
    const commit = entry.change?.commitId ?? entry.lifecycle?.commitId ?? "";
    const policy = commit
      ? client(this.connection, PolicyService)
          .evaluatePolicy({ repo, revision: commit, stack: below.length > 0, skipSnapshot: true })
          .then(
            ({ evaluation, members }) => ({ evaluation, members, policyError: null }),
            (thrown: unknown) => ({
              evaluation: undefined,
              members: [],
              policyError: this.failed(thrown),
            }),
          )
      : Promise.resolve({ evaluation: undefined, members: [], policyError: null });
    const [stack, policyRead, evidence, comments, transitions, provenance] = await Promise.all([
      below.length > 0
        ? changes.listProposals({ repo, changes: below, states: ANY_STATE })
        : Promise.resolve({ entries: [] as ProposalEntry[] }),
      policy,
      client(this.connection, EvidenceService).listEvidence({ repo, changeId, pageSize: 200 }),
      this.comments(repo, changeId),
      changes.listTransitions({ repo, changes: [changeId], pageSize: 100 }),
      client(this.connection, ProvenanceService).listProvenance({
        repo,
        changeIds: [changeId],
        pageSize: 1,
      }),
    ]);
    if (this.isStopped()) return true;
    // A verdict no answer came for this time: the last one of this commit stands.
    const { policyError } = policyRead;
    const last = this.snapshot.read;
    const held =
      policyError && isPassing(policyError) && last?.entry.change?.commitId === commit
        ? last
        : null;
    const members = new Map(stack.entries.map((e) => [e.changeId, e]));
    this.update({
      read: {
        entry,
        stack: below.map((id) => ({ changeId: id, entry: members.get(id) })),
        evaluation: (held ?? policyRead).evaluation,
        members: (held ?? policyRead).members,
        policyError,
        evidence: evidence.evidence,
        comments: comments.comments,
        moreComments: comments.more,
        transitions: transitions.transitions,
        provenance: provenance.records[0],
      },
      error: null,
      fatal: null,
    });
    return !(policyError && isPassing(policyError));
  }
}
