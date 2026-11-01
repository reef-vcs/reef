// The workspaces page's data (R2.20): a repository's active workspaces with
// their changes, live from the shared operation stream, plus what the page
// keeps across refreshes (each workspace's cause, rows that ended, the
// agents' order) and the disk scheduler.
//
// Nothing is read while the stream is not live, so an operation recorded
// after a read arrives on the stream and asks for another. The first reads
// are `GetRepository` (the route may name a prefix or a path; the streams
// carry full ids) and `GetDaemonStatus` (the restart window). A refresh is
// `ListWorkspaces` and `ListChanges` over `working_copies()` in parallel,
// joined by change id; they cannot be pinned to one operation, so a change
// that moved between the two shows its id without a badge until the next
// operation, which the move is. Any operation in the repository asks for a
// refresh: snapshots, most operations, change a row's commit, last snapshot
// and file count. Refreshes are paced (live/paced.ts). Nothing is read on a
// timer.
//
// Rows never move: workspaces are listed newest first, so new ones arrive on
// top; one that ends keeps its row, in place, until the user hides it.
// Agents keep the order of their first appearance, by name on the first
// synced set and on top after that.
//
// At an operation (`at`), the page is frozen: the operation is looked up
// once, both reads are made at it, and neither stream is consulted.
import { Code, ConnectError } from "@connectrpc/connect";
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import type { Change } from "../gen/reef/api/v1/changes_pb";
import { OperationKind, type Operation } from "../gen/reef/api/v1/operations_pb";
import type { Repository } from "../gen/reef/api/v1/repositories_pb";
import {
  WorkspaceState,
  type GetWorkspaceResponse,
  type Workspace,
} from "../gen/reef/api/v1/workspaces_pb";
import { joinAgents } from "../fleet/agents";
import { DiskScheduler } from "../fleet/disk";
import { restartWindowMs } from "../fleet/lease";
import { WORKING_COPIES_REVSET } from "../graph/revset";
import { after, type Cancel } from "../live/backoff";
import { FrameNotifier, type FrameScheduler } from "../live/frame";
import type { OperationBatch, OperationStore } from "../live/operations";
import { Paced } from "../live/paced";
import type { PresenceStore } from "../live/presence";

export const REPOSITORY_NOT_FOUND = "REPOSITORY_NOT_FOUND";
/** Workspaces read at most; past it the page says so. */
export const WORKSPACES_SHOWN = 1000;

const LISTED = [WorkspaceState.ACTIVE, WorkspaceState.RELEASING];

/** Kinds that make a workspace's disk figure stale, with its `workspace_id`. */
const DIRTYING = new Set([
  OperationKind.SNAPSHOT,
  OperationKind.EDIT,
  OperationKind.MOVE_WORKSPACE,
]);

export interface EndedRow {
  /** Its final state, or the last listed one if the daemon no longer knows it. */
  workspace: Workspace;
  /** "released", "reclaimed (lease expired)" or "ended". */
  word: string;
  /** The `RELEASE_WORKSPACE` operation, if the stream carried one. */
  operation: Operation | undefined;
}

export interface WorkspaceRow {
  id: string;
  workspace: Workspace;
  /** Its change from the last `ListChanges`; undefined when the two reads straddled a move. */
  change: Change | undefined;
  /** Set once it is no longer listed. */
  ended: EndedRow | undefined;
  /** The latest streamed operation naming it. */
  cause: Operation | undefined;
  /** It arrived in a live refresh: shown with the one change transition. */
  arrived: boolean;
}

export interface FleetSnapshot {
  /** Null until the first refresh answers. */
  rows: WorkspaceRow[] | null;
  repository: Repository | undefined;
  /** At an operation: the operation, once looked up. */
  frozenAt: Operation | undefined;
  /** More active workspaces than `WORKSPACES_SHOWN`. */
  more: boolean;
  /** A failed refresh; the last good rows stay. */
  error: ReefError | null;
  /** The repository is not found: the page is replaced. */
  fatal: ReefError | null;
  /** When the last refresh ended, in milliseconds since the epoch. */
  lastRefreshAt: number | undefined;
  /** Within two heartbeat intervals of the daemon's start. */
  restarting: boolean;
  /** Agent names in display order, gone ones included until hidden. */
  agentOrder: string[];
  /** A frozen read's `GetWorkspace` for the selection. */
  frozenWorkspace: GetWorkspaceResponse | undefined;
  /** Bumped by every change, disk answers included. */
  version: number;
}

export interface FleetOptions {
  repo: string;
  /** An operation id or unique prefix: the page at that point, frozen. */
  at?: string | undefined;
}

export class FleetModel {
  readonly disk: DiskScheduler;
  private snapshot: FleetSnapshot;
  private readonly notifier: FrameNotifier;
  private readonly paced: Paced;
  /** As the route gave it. */
  private readonly repo: string;
  /** The full id, once `GetRepository` answered. */
  private repoId: string | undefined;
  private readonly at: string | undefined;
  private readonly causes = new Map<string, Operation>();
  private readonly ended = new Map<string, EndedRow>();
  /** Row ids in display order, ended ones included until hidden. */
  private order: string[] = [];
  private listed = new Map<string, Workspace>();
  private changes = new Map<string, Change>();
  private readonly arrived = new Set<string>();
  private started = false;
  private requested = false;
  private waiting = false;
  private stopped = false;
  private uptimeMs: number | undefined;
  /** When the uptime was read, by this tab's clock: only the time since is used. */
  private uptimeReadAt = 0;
  private selection = 0;
  private windowDecided = false;
  private closeWindow: Cancel | undefined;
  private resyncs = -1;
  private agentsSeeded = false;
  private readonly hiddenAgents = new Set<string>();

  constructor(
    private readonly connection: Connection,
    private readonly operations: OperationStore,
    private readonly presence: PresenceStore,
    frame: FrameScheduler,
    private readonly onSessionInvalid: () => void,
    options: FleetOptions,
    private readonly now: () => number = Date.now,
  ) {
    this.notifier = new FrameNotifier(frame);
    this.paced = new Paced(() => this.refresh(), now);
    this.repo = options.repo;
    this.at = options.at || undefined;
    this.disk = new DiskScheduler(
      connection,
      () => this.repoId ?? this.repo,
      frame,
      () => {
        this.update({ version: this.snapshot.version + 1 });
      },
      onSessionInvalid,
      now,
    );
    this.snapshot = {
      rows: null,
      repository: undefined,
      frozenAt: undefined,
      more: false,
      error: null,
      fatal: null,
      lastRefreshAt: undefined,
      restarting: false,
      agentOrder: [],
      frozenWorkspace: undefined,
      version: 0,
    };
  }

  subscribe = (listener: () => void): (() => void) => this.notifier.subscribe(listener);

  getSnapshot = (): FleetSnapshot => this.snapshot;

  get frozen(): boolean {
    return this.at !== undefined;
  }

  start(): () => void {
    if (this.frozen) {
      void this.readAt();
      return () => {
        this.stopped = true;
      };
    }
    const unsubscribeOperations = this.operations.subscribe((batch) => {
      this.onBatch(batch);
    });
    const unsubscribePresence = this.presence.subscribe(() => {
      this.onPresence();
    });
    if (this.operations.live) this.requestFirst();
    this.onPresence();
    return () => {
      this.stopped = true;
      unsubscribeOperations();
      unsubscribePresence();
      this.paced.stop();
      this.disk.stop();
      this.closeWindow?.();
    };
  }

  /** The user selected `id` (or nothing): measured first if it needs a figure. */
  select(id: string) {
    this.disk.select(id);
  }

  /** At an operation, the selection's `GetWorkspace` at it, without disk. */
  async selectAt(id: string) {
    // Answers to an earlier selection are dropped.
    const selection = ++this.selection;
    this.update({ frozenWorkspace: undefined });
    if (!this.frozen || !id) return;
    try {
      const response = await this.connection.clients.workspaces.getWorkspace({
        repo: this.repoId ?? this.repo,
        workspace: id,
        atOperation: this.snapshot.frozenAt?.id ?? this.at ?? "",
        skipDiskUsage: true,
      });
      if (!this.isStopped() && selection === this.selection) {
        this.update({ frozenWorkspace: response });
      }
    } catch (thrown) {
      if (selection === this.selection) this.fail(thrown);
    }
  }

  /** "Hide n ended": ended rows leave the table. */
  hideEnded() {
    this.order = this.order.filter((id) => !this.ended.has(id));
    for (const id of [...this.ended.keys()]) this.ended.delete(id);
    this.publishRows();
  }

  /** "Hide n gone": agents with no live entry leave the table. */
  hideGone(gone: string[]) {
    for (const name of gone) this.hiddenAgents.add(name);
    const hidden = new Set(gone);
    this.update({ agentOrder: this.snapshot.agentOrder.filter((n) => !hidden.has(n)) });
  }

  private requestFirst() {
    if (this.requested) return;
    this.requested = true;
    this.paced.request();
  }

  private onBatch(batch: OperationBatch) {
    let ask = batch.reset || this.waiting;
    for (const operation of batch.operations) {
      if (this.repoId !== undefined && operation.repoId !== this.repoId) continue;
      ask = true;
      const touched = new Set<string>();
      if (operation.workspaceId) {
        this.causes.set(operation.workspaceId, operation);
        if (DIRTYING.has(operation.kind)) touched.add(operation.workspaceId);
      }
      for (const effect of operation.effects?.workingCopies ?? []) {
        if (effect.workspaceId) touched.add(effect.workspaceId);
      }
      for (const id of touched) this.disk.markDirty(id);
    }
    if (batch.live && !this.requested) {
      this.requestFirst();
      return;
    }
    if (!ask || !this.requested) return;
    this.waiting = !batch.live;
    if (batch.live) this.paced.request();
  }

  private onPresence() {
    if (this.frozen || this.stopped) return;
    if (this.presence.resyncs !== this.resyncs) {
      // A new subscription: the daemon may have restarted, so the restart
      // window is read again (only once the repository is known).
      this.resyncs = this.presence.resyncs;
      if (this.started) void this.readStatus();
    }
    if (!this.presence.synced) return;
    const repoId = this.repoId;
    if (repoId === undefined) return;
    const names = [...joinAgents(this.presence.entries.values(), repoId).agents.keys()];
    const known = new Set(this.snapshot.agentOrder);
    const fresh = names.filter((n) => !known.has(n));
    for (const name of fresh) this.hiddenAgents.delete(name);
    if (!this.agentsSeeded) {
      this.agentsSeeded = true;
      this.update({ agentOrder: [...names].sort() });
    } else if (fresh.length > 0) {
      this.update({ agentOrder: [...fresh.sort(), ...this.snapshot.agentOrder] });
    }
  }

  private refresh(): Promise<void> | undefined {
    if (this.stopped) return undefined;
    if (!this.operations.live) {
      this.waiting = true;
      return undefined;
    }
    return this.readLive();
  }

  private async readLive() {
    try {
      if (!this.started) {
        const { repository } = await this.connection.clients.repositories.getRepository({
          repo: this.repo,
        });
        if (this.isStopped()) return;
        this.repoId = repository?.id || this.repo;
        this.started = true;
        this.update({ repository });
        this.onPresence();
        await this.readStatus();
        if (this.isStopped()) return;
      }
      const repo = this.repoId ?? this.repo;
      const [workspaces, changes] = await Promise.all([
        this.listWorkspaces(repo, ""),
        this.listChanges(repo, ""),
      ]);
      if (this.isStopped()) return;
      await this.apply(workspaces.workspaces, workspaces.more, changes, true);
    } catch (thrown) {
      this.fail(thrown);
    }
  }

  private async readStatus() {
    try {
      const status = await this.connection.clients.daemon.getDaemonStatus({ skipBackends: true });
      if (this.isStopped()) return;
      this.uptimeMs = Number(status.uptimeMs);
      this.uptimeReadAt = this.now();
      this.windowDecided = false;
      this.decideWindow();
    } catch (thrown) {
      this.fail(thrown);
    }
  }

  /** Opens the restart window once both the uptime and the leases are known. */
  private decideWindow() {
    if (this.windowDecided || this.uptimeMs === undefined || this.listed.size === 0) return;
    this.windowDecided = true;
    this.closeWindow?.();
    this.closeWindow = undefined;
    // The window may be decided long after the read (no workspace until then).
    const uptime = this.uptimeMs + Math.max(0, this.now() - this.uptimeReadAt);
    const left = restartWindowMs(
      uptime,
      [...this.listed.values()].flatMap((w) => (w.lease ? [w.lease] : [])),
    );
    if (left <= 0) {
      if (this.snapshot.restarting) this.update({ restarting: false });
      return;
    }
    this.update({ restarting: true });
    // The one re-render delay: it reads nothing.
    this.closeWindow = after(left, () => {
      this.closeWindow = undefined;
      this.update({ restarting: false });
    });
  }

  private async readAt() {
    try {
      const response = await this.connection.clients.operations.getOperation({
        repo: this.repo,
        operationId: this.at ?? "",
      });
      const operation = response.operation;
      if (this.isStopped()) return;
      if (!operation) throw new ConnectError("The daemon returned no operation.", Code.NotFound);
      this.repoId = operation.repoId || this.repo;
      this.update({ frozenAt: operation });
      const workspaces = await this.listWorkspaces(this.repoId, operation.id);
      if (this.isStopped()) return;
      const changes = await this.listChanges(this.repoId, workspaces.atOperation || operation.id);
      if (this.isStopped()) return;
      await this.apply(workspaces.workspaces, workspaces.more, changes, false);
    } catch (thrown) {
      this.fail(thrown);
    }
  }

  private async listWorkspaces(repo: string, atOperation: string) {
    const out: Workspace[] = [];
    let pageToken = "";
    let at = atOperation;
    do {
      const page = await this.connection.clients.workspaces.listWorkspaces({
        repo,
        states: LISTED,
        changedFileCounts: true,
        pageSize: WORKSPACES_SHOWN - out.length,
        pageToken,
        atOperation,
      });
      out.push(...page.workspaces);
      pageToken = page.nextPageToken;
      at = page.atOperation || at;
    } while (pageToken && out.length < WORKSPACES_SHOWN);
    return { workspaces: out, more: pageToken !== "", atOperation: at };
  }

  private async listChanges(repo: string, atOperation: string): Promise<Map<string, Change>> {
    const out = new Map<string, Change>();
    let pageToken = "";
    let at = atOperation;
    let pages = 0;
    do {
      const page = await this.connection.clients.changes.listChanges({
        repo,
        revset: WORKING_COPIES_REVSET,
        skipSnapshot: true,
        pageSize: WORKSPACES_SHOWN,
        pageToken,
        atOperation: at,
      });
      for (const entry of page.entries)
        if (entry.change) out.set(entry.change.changeId, entry.change);
      pageToken = page.nextPageToken;
      // Later pages are pinned to the first's operation.
      at = page.atOperation;
      pages += 1;
    } while (pageToken && pages < 10);
    return out;
  }

  private async apply(
    workspaces: Workspace[],
    more: boolean,
    changes: Map<string, Change>,
    live: boolean,
  ) {
    const first = this.snapshot.rows === null;
    const listed = new Map(workspaces.map((w) => [w.id, w] as const));
    // Rows that are no longer listed ended: read each once for its final state.
    const gone = live
      ? [...this.listed.keys()].filter((id) => !listed.has(id) && !this.ended.has(id))
      : [];
    const finals = await Promise.all(gone.map((id) => this.readFinal(id)));
    if (this.isStopped()) return;
    for (const [i, id] of gone.entries()) {
      const final = finals[i];
      const last = this.listed.get(id);
      if (final?.workspace?.state === WorkspaceState.ACTIVE) {
        // Still active (past the page's limit, or a straddle): keep it listed.
        listed.set(id, final.workspace);
        continue;
      }
      const workspace = final?.workspace ?? last;
      if (!workspace) continue;
      const cause = this.causes.get(id);
      this.ended.set(id, {
        workspace,
        word: endedWord(workspace),
        operation: cause?.kind === OperationKind.RELEASE_WORKSPACE ? cause : undefined,
      });
    }
    // New rows go on top, in the list's order (newest first).
    const known = new Set(this.order);
    const fresh = workspaces.map((w) => w.id).filter((id) => !known.has(id));
    if (!first && live) for (const id of fresh) this.arrived.add(id);
    this.order = [...fresh, ...this.order];
    this.listed = listed;
    this.changes = changes;
    // At an operation nothing is measured: disk is not replayed.
    if (live) this.disk.setWorkspaces(workspaces.map((w) => w.id));
    this.decideWindow();
    this.update({ more, error: null, lastRefreshAt: live ? this.now() : undefined });
    this.publishRows();
  }

  private async readFinal(id: string): Promise<GetWorkspaceResponse | undefined> {
    try {
      return await this.connection.clients.workspaces.getWorkspace({
        repo: this.repoId ?? this.repo,
        workspace: id,
        skipDiskUsage: true,
      });
    } catch (thrown) {
      const error = reefError(thrown);
      if (isSessionInvalid(error)) this.onSessionInvalid();
      return undefined;
    }
  }

  private publishRows() {
    const rows: WorkspaceRow[] = [];
    for (const id of this.order) {
      const ended = this.ended.get(id);
      const workspace = this.listed.get(id) ?? ended?.workspace;
      if (!workspace) continue;
      rows.push({
        id,
        workspace,
        change: ended ? undefined : this.changes.get(workspace.changeId),
        ended,
        cause: this.frozen ? undefined : this.causes.get(id),
        arrived: this.arrived.has(id),
      });
    }
    this.update({ rows, version: this.snapshot.version + 1 });
  }

  /** A call, so TypeScript does not narrow `stopped` across an await. */
  private isStopped(): boolean {
    return this.stopped;
  }

  private fail(thrown: unknown) {
    if (this.isStopped()) return;
    const error = reefError(thrown);
    if (isSessionInvalid(error)) {
      this.onSessionInvalid();
      return;
    }
    if (error.reason === REPOSITORY_NOT_FOUND && this.snapshot.rows === null) {
      this.update({ fatal: error });
      return;
    }
    // The last good rows stay; the error shows above them.
    this.update({ error });
  }

  private update(change: Partial<FleetSnapshot>) {
    this.snapshot = { ...this.snapshot, ...change };
    this.notifier.notify();
  }
}

function endedWord(workspace: Workspace): string {
  switch (workspace.state) {
    case WorkspaceState.RELEASED:
      return "released";
    case WorkspaceState.RECLAIMED:
      return "reclaimed (lease expired)";
    case WorkspaceState.RELEASING:
      return "releasing";
    default:
      return "ended";
  }
}
