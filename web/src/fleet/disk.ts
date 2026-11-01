// Disk usage on a budget (R2.20). Each figure is a `GetWorkspace` that walks
// the workspace's directory on the daemon, so the page measures one
// workspace at a time, paced like every other read (live/paced.ts): one open
// page costs the daemon at most a quarter of one core in walks, whatever the
// fleet's size.
//
// Order: what the user asked to measure again, then the selected workspace if
// it needs a figure, then workspaces never measured (in list order), then
// those an operation touched since their figure (oldest figure first).
// Nothing else re-measures a workspace: build output grows one without any
// event, which is what "Measure again" is for. A measurement starts from an
// animation frame, so a hidden tab measures nothing. `snapshot_first` is never
// sent: the page never makes the daemon write.
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import type { GetWorkspaceResponse } from "../gen/reef/api/v1/workspaces_pb";
import type { FrameScheduler } from "../live/frame";
import { Paced } from "../live/paced";

export interface DiskResult {
  /** The last good answer; it carries `disk_usage` unless not measurable. */
  response: GetWorkspaceResponse | undefined;
  /** When the last good answer came, in milliseconds since the epoch. */
  measuredAt: number | undefined;
  /** The daemon answered without a figure (its base is no longer cached). */
  notMeasurable: boolean;
  /** The last call failed; the last good answer stays. */
  error: ReefError | null;
  /** An operation touched it since its figure. */
  dirty: boolean;
}

export class DiskScheduler {
  readonly results = new Map<string, DiskResult>();
  /** Increases with every answer. */
  version = 0;
  private ids: string[] = [];
  private selected = "";
  /** "Measure again", in the order asked. */
  private readonly forced = new Set<string>();
  private framed = false;
  private stopped = false;
  /** Bumped by every `markDirty`, so a mark during a measurement survives its answer. */
  private readonly marks = new Map<string, number>();
  private readonly paced: Paced;

  constructor(
    private readonly connection: Connection,
    private readonly repo: () => string,
    private readonly frame: FrameScheduler,
    private readonly onChange: () => void,
    private readonly onSessionInvalid: () => void,
    private readonly now: () => number = Date.now,
  ) {
    // Spaced runs start from a frame too: a hidden tab measures nothing.
    this.paced = new Paced(() => this.measureNext(), now, frame);
  }

  /** The active workspaces, in list order: only these are measured. */
  setWorkspaces(ids: string[]) {
    this.ids = ids;
    this.ask();
  }

  select(id: string) {
    this.selected = id;
    this.ask();
  }

  /** An operation touched `id`. */
  markDirty(id: string) {
    this.marks.set(id, (this.marks.get(id) ?? 0) + 1);
    const result = this.results.get(id);
    if (result) result.dirty = true;
    this.ask();
  }

  /** The user asked: first, and not spaced. */
  measureAgain(id: string) {
    this.forced.add(id);
    this.frame(() => {
      // A run that started since may already have taken it.
      if (this.forced.has(id)) this.paced.requestNow();
    });
  }

  stop() {
    this.stopped = true;
    this.paced.stop();
  }

  /** What would be measured next, or undefined. */
  next(): string | undefined {
    const active = new Set(this.ids);
    for (const id of this.forced) if (active.has(id)) return id;
    const needs = (id: string) => {
      const result = this.results.get(id);
      return !result || result.dirty;
    };
    if (this.selected && active.has(this.selected) && needs(this.selected)) return this.selected;
    const never = this.ids.find((id) => !this.results.has(id));
    if (never !== undefined) return never;
    let oldest: string | undefined;
    let oldestAt = Number.POSITIVE_INFINITY;
    for (const id of this.ids) {
      const result = this.results.get(id);
      if (!result?.dirty) continue;
      const at = result.measuredAt ?? Number.NEGATIVE_INFINITY;
      if (at < oldestAt) {
        oldest = id;
        oldestAt = at;
      }
    }
    return oldest;
  }

  /** Asks for a measurement at the next frame, if one is due. */
  private ask() {
    if (this.framed || this.stopped) return;
    this.framed = true;
    this.frame(() => {
      this.framed = false;
      if (this.next() !== undefined) this.paced.request();
    });
  }

  private measureNext(): Promise<void> | undefined {
    if (this.stopped) return undefined;
    const id = this.next();
    if (id === undefined) return undefined;
    this.forced.delete(id);
    return this.measure(id);
  }

  private async measure(id: string) {
    const before = this.results.get(id);
    const mark = this.marks.get(id) ?? 0;
    // An operation that touched it while the walk ran may postdate what it saw.
    const touched = () => (this.marks.get(id) ?? 0) !== mark;
    try {
      const response = await this.connection.clients.workspaces.getWorkspace({
        repo: this.repo(),
        workspace: id,
        snapshotFirst: false,
        skipDiskUsage: false,
      });
      if (this.stopped) return;
      this.results.set(id, {
        response,
        measuredAt: this.now(),
        notMeasurable: !response.diskUsage,
        error: null,
        dirty: touched(),
      });
    } catch (thrown) {
      if (this.stopped) return;
      const error = reefError(thrown);
      if (isSessionInvalid(error)) {
        this.onSessionInvalid();
        return;
      }
      // Not retried until an operation touches it again or the user asks.
      this.results.set(id, {
        response: before?.response,
        measuredAt: before?.measuredAt,
        notMeasurable: before?.notMeasurable ?? false,
        error,
        dirty: touched(),
      });
    }
    this.version += 1;
    this.onChange();
    // The next one, from a frame, spaced after this run.
    this.ask();
  }
}
