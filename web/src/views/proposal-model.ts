// The data of the Proposals tab and the proposal page (R3.31).
//
// Both read live from the shared operation stream, as the workspaces page
// does: nothing is read until the stream is live, so an operation recorded
// after a read arrives on it and asks for another, and refreshes are paced
// (live/paced.ts). Any operation in the repository asks for one: a comment,
// a transition and a landing are operations, and so is the automatic
// service's queueing. Evidence is not: a check's result or a sign-off
// records no operation, and it changes the policy's verdict and the
// automatic status. So while something is in review, a read also follows
// the last one by `POLL_MS`, paced the same way. `ListProposals` has no
// `at_operation`, so neither page reads at an operation.
//
// The tab's list is held in place (the design's rule): rows keep their order,
// a proposal that left review keeps its row with its new state, and new
// proposals wait behind a count until the person shows them.
//
// The proposal page reads the proposal as a unit, in this order of the page:
// the entry (`ListProposals` for its change, any state), the changes below it
// (R3.11's `dependencies`, with their entries), the policy's verdict
// (`EvaluatePolicy`, over the stack when it has one), the evidence, the
// comments, the transitions and the declared provenance. A failed policy read
// is shown in its section and does not fail the page.
import { Code } from "@connectrpc/connect";
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { ChangeService, type ProposalEntry } from "../gen/reef/api/v1/changes_pb";
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
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import type { OperationBatch, OperationStore } from "../live/operations";
import { after, type Cancel } from "../live/backoff";
import { Paced } from "../live/paced";
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

/** Open proposals the tab lists at most; past it the tab says so. */
export const PROPOSALS_SHOWN = 200;

export const NOT_A_PROPOSAL = "NOT_A_PROPOSAL";

/** When an entry last moved: its latest transition, else never. */
export function updatedAt(entry: ProposalEntry): number {
  const at = entry.lifecycle?.last?.recordedAt;
  return at ? Number(at.seconds) * 1000 + Math.floor(at.nanos / 1e6) : 0;
}

function newestFirst(a: ProposalEntry, b: ProposalEntry): number {
  return updatedAt(b) - updatedAt(a) || a.changeId.localeCompare(b.changeId);
}

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

  constructor(
    protected readonly connection: Connection,
    private readonly operations: OperationStore,
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
    if (this.operations.live) this.requestFirst();
    return () => {
      this.stopped = true;
      unsubscribe();
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

  protected abstract read(repo: string): Promise<void>;

  /** Whether what was read is still in review, so evidence may arrive without an operation. */
  protected abstract inReview(): boolean;

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

  private refresh(): Promise<void> | undefined {
    if (this.stopped) return undefined;
    if (!this.operations.live) {
      this.waiting = true;
      return undefined;
    }
    return this.readRepo();
  }

  private async readRepo() {
    try {
      if (this.repoId === undefined) {
        const { repository } = await client(this.connection, RepositoryService).getRepository({
          repo: this.repo,
        });
        if (this.isStopped()) return;
        this.repoId = repository?.id || this.repo;
        this.onRepository(repository);
      }
      await this.read(this.repoId);
    } catch (thrown) {
      if (this.isStopped()) return;
      this.onError(this.failed(thrown));
    } finally {
      this.arm();
    }
  }

  protected abstract onRepository(repository: Repository | undefined): void;

  protected abstract onError(error: ReefError | null): void;
}

// ---- The Proposals tab ----

export interface ProposalsSnapshot {
  /** Rows in display order; null until the first read answers. */
  rows: ProposalEntry[] | null;
  /** Open proposals that arrived since, waiting behind the count. */
  fresh: number;
  /** Rows whose proposal left review since it was shown. */
  left: number;
  /** More open proposals than `PROPOSALS_SHOWN`. */
  more: boolean;
  repository: Repository | undefined;
  /** A failed refresh; the last good rows stay. */
  error: ReefError | null;
  version: number;
}

export class ProposalsModel extends RepoReader<ProposalsSnapshot> {
  /** Change ids in display order. */
  private order: string[] = [];
  private readonly entries = new Map<string, ProposalEntry>();
  /** Open ones not shown yet, by change id. */
  private readonly pending = new Map<string, ProposalEntry>();

  constructor(
    connection: Connection,
    operations: OperationStore,
    frame: FrameScheduler,
    onSessionInvalid: () => void,
    repo: string,
  ) {
    super(connection, operations, frame, onSessionInvalid, repo, {
      rows: null,
      fresh: 0,
      left: 0,
      more: false,
      repository: undefined,
      error: null,
      version: 0,
    });
  }

  /** "Show n new": new proposals go on top, and the rows that left review go. */
  showNew() {
    const fresh = [...this.pending.values()].sort(newestFirst);
    for (const entry of fresh) this.entries.set(entry.changeId, entry);
    this.pending.clear();
    this.order = [
      ...fresh.map((e) => e.changeId),
      ...this.order.filter((id) => this.isOpen(this.entries.get(id))),
    ];
    for (const id of [...this.entries.keys()]) {
      if (!this.order.includes(id)) this.entries.delete(id);
    }
    this.publish();
  }

  protected onRepository(repository: Repository | undefined) {
    this.update({ repository });
  }

  protected onError(error: ReefError | null) {
    if (error) this.update({ error });
  }

  protected inReview(): boolean {
    return (this.snapshot.rows ?? []).some((entry) => this.isOpen(entry));
  }

  protected async read(repo: string) {
    const changes = client(this.connection, ChangeService);
    const listed = await changes.listProposals({ repo, pageSize: PROPOSALS_SHOWN });
    if (this.isStopped()) return;
    const open = new Map(listed.entries.map((e) => [e.changeId, e]));
    // Rows that left review keep their place with their new state.
    const gone = this.order.filter((id) => !open.has(id));
    const closed =
      gone.length > 0
        ? (await changes.listProposals({ repo, changes: gone, states: ANY_STATE })).entries
        : [];
    if (this.isStopped()) return;
    for (const entry of closed) this.entries.set(entry.changeId, entry);
    const first = this.snapshot.rows === null;
    for (const [id, entry] of open) {
      if (this.entries.has(id) || first) this.entries.set(id, entry);
      else this.pending.set(id, entry);
    }
    for (const id of [...this.pending.keys()]) if (!open.has(id)) this.pending.delete(id);
    if (first) this.order = [...open.values()].sort(newestFirst).map((e) => e.changeId);
    this.snapshot = { ...this.snapshot, more: listed.nextPageToken !== "", error: null };
    this.publish();
  }

  private isOpen(entry: ProposalEntry | undefined): boolean {
    const state = entry?.lifecycle?.state;
    return state === LifecycleState.PROPOSED || state === LifecycleState.QUEUED;
  }

  private publish() {
    const rows = this.order.flatMap((id) => this.entries.get(id) ?? []);
    this.update({
      rows,
      fresh: this.pending.size,
      left: rows.filter((e) => !this.isOpen(e)).length,
    });
  }
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
  /** A failed refresh; the last good read stays. */
  error: ReefError | null;
  /** The change is not a recorded proposal, or the repository is unknown: the page is replaced. */
  fatal: ReefError | null;
  version: number;
}

export class ProposalModel extends RepoReader<ProposalSnapshot> {
  constructor(
    connection: Connection,
    operations: OperationStore,
    frame: FrameScheduler,
    onSessionInvalid: () => void,
    repo: string,
    private readonly change: string,
  ) {
    super(connection, operations, frame, onSessionInvalid, repo, {
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
    if (this.snapshot.read === null) this.update({ fatal: error });
    else this.update({ error });
  }

  protected inReview(): boolean {
    const state = this.snapshot.read?.entry.lifecycle?.state;
    return state === LifecycleState.PROPOSED || state === LifecycleState.QUEUED;
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

  protected async read(repo: string) {
    const changes = client(this.connection, ChangeService);
    const listed = await changes.listProposals({
      repo,
      changes: [this.change],
      states: ANY_STATE,
    });
    if (this.isStopped()) return;
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
        hint: "Open a proposal from the Proposals tab.",
        next: [],
        metadata: {},
      });
      return;
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
    if (this.isStopped()) return;
    const members = new Map(stack.entries.map((e) => [e.changeId, e]));
    this.update({
      read: {
        entry,
        stack: below.map((id) => ({ changeId: id, entry: members.get(id) })),
        evaluation: policyRead.evaluation,
        members: policyRead.members,
        policyError: policyRead.policyError,
        evidence: evidence.evidence,
        comments: comments.comments,
        moreComments: comments.more,
        transitions: transitions.transitions,
        provenance: provenance.records[0],
      },
      error: null,
    });
  }
}
