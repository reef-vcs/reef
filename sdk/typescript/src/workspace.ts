// A workspace held for a block of code, as `reef ws run` holds one for a command (R2.07).
//
// `client.workspace(...)` creates a workspace, and `client.attach(id)` takes over an existing one.
// While it is held, the lease is renewed on the daemon's cadence and one ACTIVITY presence entry
// is reported (R2.11); at the end the entry is cleared, the heartbeats stop, and the workspace is
// released with a final snapshot (unless kept), so every edit lands in its change.

import type { MessageInitShape } from "@bufbuild/protobuf";
import type { Duration } from "@bufbuild/protobuf/wkt";

import type { Client } from "./client.js";
import { ConflictError, isReefError } from "./errors.js";
import type { GetDiffRequestSchema, GetDiffResponse } from "./gen/reef/api/v1/diff_pb.js";
import type {
  EditFilesRequestSchema,
  EditFilesResponse,
  ReadFileRequestSchema,
  ReadFileResponse,
} from "./gen/reef/api/v1/files_pb.js";
import { PresenceKind } from "./gen/reef/api/v1/presence_pb.js";
import type { DeclareProvenanceResponse } from "./gen/reef/api/v1/provenance_pb.js";
import type { GetStatusRequestSchema, GetStatusResponse } from "./gen/reef/api/v1/status_pb.js";
import {
  type Backend,
  LeaseState,
  type MoveWorkspaceRequestSchema,
  type MoveWorkspaceResponse,
  type ReleaseWorkspaceResponse,
  type SnapshotWorkspaceResponse,
  type Workspace,
  WorkspaceState,
} from "./gen/reef/api/v1/workspaces_pb.js";
import { defaultOwner, presenceAgent, processOwner } from "./identity.js";
import { HEARTBEAT_DEADLINE, LeaseCore, leaseGone } from "./lease-core.js";
import { REFRESH } from "./presence-core.js";
import { Reporter } from "./presence.js";
import {
  createPlan,
  declaration,
  PROVENANCE_DISABLED,
  PROVENANCE_REVISION,
  type Provenance,
} from "./provenance.js";
import { debug, sleep, warn } from "./warn.js";

/** The longest activity text a command gives, as `ws run` cuts its purpose. */
export const PURPOSE_MAX_CHARS = 200;

/** A new workspace for a block. */
export interface WorkspaceOptions {
  /** The repository's id or path. */
  repo: string;
  /** The change to put the workspace on; not with `newOn`. */
  change?: string | undefined;
  /** A revision to put a new change on; not with `change`. */
  newOn?: string | undefined;
  purpose?: string | undefined;
  /** The workspace's owner; `<user>@<host>:<pid>` by default, as `ws run` owns its workspace. */
  owner?: string | undefined;
  backend?: Backend | undefined;
  leaseLength?: Duration | undefined;
  /** The presence entry's text; else `purpose`, else `running <argv>`. */
  activity?: string | undefined;
  /** Leave the workspace active at the end instead of releasing it. */
  keep?: boolean | undefined;
  /** Report one ACTIVITY presence entry while it is held; true by default. */
  reportPresence?: boolean | undefined;
  /**
   * What its work is for (R3.44): model, harness, task, instruction (hashed here, never sent),
   * client session, identity, actor. `REEF_MODEL` and `REEF_TASK` fill a model and task it does
   * not set, as `reef ws create`'s do.
   */
  provenance?: Provenance | undefined;
}

/** An existing workspace for a block. */
export interface AttachOptions {
  /** The holder it must have; else this user on this machine. */
  owner?: string | undefined;
  /** Release it at the end; false by default. */
  release?: boolean | undefined;
  /** Report one ACTIVITY presence entry while it is held; true by default. */
  reportPresence?: boolean | undefined;
  /** The presence entry's text; else its purpose, else `running <argv>`. */
  activity?: string | undefined;
}

/** `running <argv>`, cut as `ws run` cuts its purpose. */
export function commandActivity(): string {
  return "running " + process.argv.slice(1).join(" ").slice(0, PURPOSE_MAX_CHARS);
}

/** Whether a workspace held by `holder` may be attached for `owner`, or for `me`. */
export function mayAttach(holder: string, owner: string | undefined, me: string): boolean {
  if (owner !== undefined) {
    return holder === owner;
  }
  if (!holder.startsWith(me)) {
    return false;
  }
  const rest = holder.slice(me.length);
  return rest === "" || rest.startsWith(":");
}

/**
 * `ws run --workspace`'s rule: the workspace must be active with a live lease, and held by
 * `owner` or, without one, by this user on this machine. Throws ConflictError.
 */
export function checkAttachable(workspace: Workspace, owner: string | undefined): void {
  const lease = workspace.lease;
  const live =
    workspace.state === WorkspaceState.ACTIVE &&
    lease !== undefined &&
    (lease.state === LeaseState.ACTIVE || lease.state === LeaseState.IN_GRACE);
  if (!live) {
    throw new ConflictError(
      `workspace ${workspace.id} is WORKSPACE_STATE_${WorkspaceState[workspace.state]} and its ` +
        "lease has ended; only an active workspace can be attached",
      { hint: "Create a new workspace instead." },
    );
  }
  if (!mayAttach(workspace.owner, owner, defaultOwner())) {
    throw new ConflictError(
      `workspace ${workspace.id} is held by ${workspace.owner}; pass ` +
        `owner: ${JSON.stringify(workspace.owner)} to attach to it`,
    );
  }
}

/** Renews one lease from timers until stopped or the lease ends. */
class Heartbeater {
  readonly #client: Client;
  readonly #core: LeaseCore;
  readonly #stop = new AbortController();
  readonly #task: Promise<void>;
  lost = false;

  constructor(client: Client, workspace: Workspace) {
    if (workspace.lease === undefined) {
      throw new Error(`workspace ${workspace.id} has no lease`);
    }
    this.#client = client.withOptions({ timeoutMs: HEARTBEAT_DEADLINE * 1000 });
    this.#core = new LeaseCore(workspace.id, workspace.lease);
    this.#task = this.#run().catch((error: unknown) => {
      // Heartbeats never throw into the block: a client closed under them stops them.
      debug("heartbeats stopped: %s", error);
    });
  }

  async stop(): Promise<void> {
    this.#stop.abort();
    await this.#task;
  }

  async #run(): Promise<void> {
    const signal = this.#stop.signal;
    for (;;) {
      await sleep(this.#core.nextDelay() * 1000, signal);
      if (signal.aborted || this.#client.connection.closed) {
        return;
      }
      try {
        const response = await this.#client.renewLease(
          { workspace: this.#core.workspace, leaseId: this.#core.leaseId },
          { signal },
        );
        for (const note of this.#core.renewed(response.lease)) {
          warn(note.message, note.code);
        }
      } catch (error) {
        if (this.#stop.signal.aborted) {
          return;
        }
        const kind = isReefError(error) ? error.kind : "internal";
        const message = error instanceof Error ? error.message : String(error);
        if (leaseGone(kind)) {
          this.lost = true;
          const note = this.#core.gone(message);
          warn(note.message, note.code);
          return;
        }
        for (const note of this.#core.unreachable(message, Date.now())) {
          warn(note.message, note.code);
        }
      }
    }
  }
}

/**
 * The workspace a block holds, with the calls an agent makes on it most. Dispose it (`await
 * using`) to end the block: the presence entry is cleared, the heartbeats stop, and the workspace
 * is released with a final snapshot unless kept.
 */
export class WorkspaceHandle implements AsyncDisposable {
  readonly #client: Client;
  readonly #release: boolean;
  #heartbeater: Heartbeater | undefined;
  #reporter: Reporter | undefined;
  #released = false;
  #finished = false;
  /** The last Workspace the daemon returned for it. */
  workspace: Workspace;
  /** The workspace's id. */
  readonly id: string;
  /** Its repository. */
  readonly repoId: string;

  /** @internal */
  constructor(client: Client, workspace: Workspace, release: boolean) {
    this.#client = client;
    this.#release = release;
    this.workspace = workspace;
    this.id = workspace.id;
    this.repoId = workspace.repoId;
  }

  /** The workspace's directory, on the daemon's machine. */
  get path(): string {
    return this.workspace.path;
  }

  /** Its change, as of the last call through this handle. */
  get changeId(): string {
    return this.workspace.changeId;
  }

  /** Whether the daemon said the lease ended while the block ran. */
  get leaseLost(): boolean {
    return this.#heartbeater?.lost ?? false;
  }

  /** @internal Starts the heartbeats and presence. */
  start(reportPresence: boolean, activity: string | undefined): void {
    this.#heartbeater = new Heartbeater(this.#client, this.workspace);
    if (!reportPresence) {
      return;
    }
    const subject = {
      agent: presenceAgent(this.#client.agent, this.workspace.owner, defaultOwner()),
      kind: PresenceKind.ACTIVITY,
      workspace: this.workspace.id,
    };
    this.#reporter = new Reporter(this.#client, subject, REFRESH);
    this.#reporter.set({ activity: activity || this.workspace.purpose || commandActivity() });
  }

  #update(workspace: Workspace | undefined): void {
    if (workspace?.id) {
      this.workspace = workspace;
    }
  }

  /** Snapshots the workspace now. */
  async snapshot(): Promise<SnapshotWorkspaceResponse> {
    return this.#client.snapshotWorkspace({ workspace: this.id });
  }

  /** `getStatus` for the workspace. */
  status(fields: MessageInitShape<typeof GetStatusRequestSchema> = {}): Promise<GetStatusResponse> {
    return this.#client.getStatus({ ...fields, workspace: this.id });
  }

  /** `getDiff` for the workspace's change. */
  diff(fields: MessageInitShape<typeof GetDiffRequestSchema> = {}): Promise<GetDiffResponse> {
    return this.#client.getDiff({ ...fields, workspace: this.id });
  }

  /**
   * A whole file in the workspace, through `readFileToEnd`: at its change (`@`) unless `revision`
   * names another.
   */
  readFile(
    path: string,
    fields: MessageInitShape<typeof ReadFileRequestSchema> = {},
  ): Promise<ReadFileResponse> {
    return this.#client.readFileToEnd({
      ...fields,
      workspace: this.id,
      path,
      revision: fields.revision || "@",
    });
  }

  /**
   * `editFiles` in the workspace: its change (`@`) unless `revision` or `newOn` names another
   * target.
   */
  editFiles(fields: MessageInitShape<typeof EditFilesRequestSchema>): Promise<EditFilesResponse> {
    const target =
      fields.target?.case === undefined ? { case: "revision" as const, value: "@" } : fields.target;
    return this.#client.editFiles({ ...fields, workspace: this.id, target });
  }

  /** `moveWorkspace`: puts the workspace on another change. */
  async move(
    fields: MessageInitShape<typeof MoveWorkspaceRequestSchema>,
  ): Promise<MoveWorkspaceResponse> {
    const response = await this.#client.moveWorkspace({ ...fields, workspace: this.id });
    this.#update(response.workspace);
    return response;
  }

  /**
   * Replaces fields of the workspace's provenance context (R3.44): `undefined` leaves a field,
   * `""` clears it. `instruction` is hashed here, and only its SHA-256 is sent. Snapshots first,
   * so edits already made are recorded under the context they were made under, unless
   * `snapshot` is false. Throws IncompatibleError for a daemon older than API revision 18.
   */
  async declare(
    fields: Omit<Provenance, "identity" | "actor">,
    options: { snapshot?: boolean } = {},
  ): Promise<DeclareProvenanceResponse> {
    const message = declaration(fields, false);
    await this.#client.requireRevision(PROVENANCE_REVISION);
    return this.#client.declareProvenance({
      workspace: this.id,
      declaration: message,
      noSnapshot: options.snapshot === false,
    });
  }

  /** Replaces the presence entry's text, at most once a second. */
  activity(text: string): void {
    this.#reporter?.set({ activity: text });
  }

  async #stopBackground(): Promise<void> {
    const reporter = this.#reporter;
    this.#reporter = undefined;
    await reporter?.clear();
    const heartbeater = this.#heartbeater;
    await heartbeater?.stop();
  }

  /** Releases the workspace now, with a final snapshot; the block's end does nothing more. */
  async release(): Promise<ReleaseWorkspaceResponse> {
    await this.#stopBackground();
    const response = await this.#client.releaseWorkspace({ workspace: this.id });
    this.#released = true;
    return response;
  }

  /**
   * @internal Ends the block. A release failure is thrown when the block succeeded (`error`
   * undefined); otherwise it is a warning, so the block's error propagates.
   */
  async finish(error: unknown): Promise<void> {
    if (this.#finished) {
      return;
    }
    this.#finished = true;
    await this.#stopBackground();
    if (!this.#release || this.#released) {
      return;
    }
    try {
      await this.#client.releaseWorkspace({ workspace: this.id });
      this.#released = true;
    } catch (failure) {
      if (error === undefined) {
        throw failure;
      }
      warn(
        `reef: releasing workspace ${this.id} failed: ${failure instanceof Error ? failure.message : String(failure)}`,
        "REEF_RELEASE_FAILED",
      );
    }
  }

  /**
   * Ends the block: `await using`'s exit. A release failure is thrown; after a block that threw,
   * the language reports both as a SuppressedError.
   */
  async [Symbol.asyncDispose](): Promise<void> {
    await this.finish(undefined);
  }
}

/** @internal `client.workspace()`. */
export async function openWorkspace(
  client: Client,
  options: WorkspaceOptions,
): Promise<WorkspaceHandle> {
  const request = {
    repo: options.repo,
    start:
      options.change !== undefined
        ? { case: "change" as const, value: options.change }
        : options.newOn !== undefined
          ? { case: "newOn" as const, value: options.newOn }
          : { case: undefined },
    purpose: options.purpose ?? "",
    owner: options.owner || processOwner(),
    ...(options.backend === undefined ? {} : { backend: options.backend }),
    ...(options.leaseLength === undefined ? {} : { leaseLength: options.leaseLength }),
  };
  // What its work is for; a declaration made only of the environment's defaults is dropped
  // where the daemon cannot take it, as `reef ws create` drops it.
  const plan = createPlan(options.provenance);
  let provenance = plan?.declaration;
  if (plan?.explicit === true) {
    await client.requireRevision(PROVENANCE_REVISION);
  } else if (
    plan !== undefined &&
    (await client.daemonStatus()).apiRevision < PROVENANCE_REVISION
  ) {
    provenance = undefined;
  }
  let response;
  try {
    response = await client.createWorkspace({
      ...request,
      ...(provenance === undefined ? {} : { provenance }),
    });
  } catch (error) {
    if (
      provenance === undefined ||
      plan?.explicit === true ||
      !isReefError(error) ||
      error.reason !== PROVENANCE_DISABLED
    ) {
      throw error;
    }
    response = await client.createWorkspace(request);
  }
  const workspace = response.workspace;
  if (workspace === undefined) {
    throw new Error("CreateWorkspace answered without a workspace");
  }
  const handle = new WorkspaceHandle(client, workspace, options.keep !== true);
  return started(handle, options.reportPresence !== false, options.activity);
}

/** @internal `client.attach()`. */
export async function attachWorkspace(
  client: Client,
  id: string,
  options: AttachOptions,
): Promise<WorkspaceHandle> {
  const workspace = (await client.getWorkspace({ workspace: id })).workspace;
  if (workspace === undefined) {
    throw new Error("GetWorkspace answered without a workspace");
  }
  checkAttachable(workspace, options.owner);
  const lease = workspace.lease;
  if (lease !== undefined) {
    const renewed = await client.renewLease({ workspace: workspace.id, leaseId: lease.id });
    if (renewed.lease !== undefined) {
      workspace.lease = renewed.lease;
    }
  }
  const handle = new WorkspaceHandle(client, workspace, options.release === true);
  return started(handle, options.reportPresence !== false, options.activity);
}

async function started(
  handle: WorkspaceHandle,
  reportPresence: boolean,
  activity: string | undefined,
): Promise<WorkspaceHandle> {
  try {
    handle.start(reportPresence, activity);
  } catch (error) {
    await handle.finish(error).catch(() => undefined);
    throw error;
  }
  return handle;
}
