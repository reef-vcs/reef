// The selected workspace's `GetWorkspace`, and "Measure again" (R2.20,
// #272). The daemon keeps every active workspace's disk figure itself and
// `ListWorkspaces` carries it (`Workspace.disk`), so the Disk column walks
// nothing (#146). This reads only the selected workspace, for its snapshots
// and the other workspaces on its change, without a walk; when an operation
// touched it, again. "Measure again" asks the daemon to walk that one
// workspace now (`GetWorkspace` with its walk), which updates the kept
// figure; `onMeasured` then refreshes the list. Reads are paced like every
// other (live/paced.ts) and start from an animation frame, so a hidden tab
// reads nothing. `snapshot_first` is never sent: the page never makes the
// daemon write.
import type { Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import type { GetWorkspaceResponse } from "../gen/reef/api/v1/workspaces_pb";
import type { FrameScheduler } from "../live/frame";
import { Paced } from "../live/paced";

export interface DiskResult {
  /** The last good answer; with `disk_usage` after "Measure again". */
  response: GetWorkspaceResponse | undefined;
  /** When the last good answer came, in milliseconds since the epoch. */
  measuredAt: number | undefined;
  /** "Measure again" answered without a figure (the directory is gone). */
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
    private readonly onMeasured: () => void = () => undefined,
  ) {
    // Spaced runs start from a frame too: a hidden tab measures nothing.
    this.paced = new Paced(() => this.measureNext(), now, frame);
  }

  /** The active workspaces, in list order: only these are read. */
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

  /** What would be read next, or undefined: a "Measure again", else the
   * selection when it was never read or an operation touched it. */
  next(): string | undefined {
    const active = new Set(this.ids);
    for (const id of this.forced) if (active.has(id)) return id;
    const result = this.selected ? this.results.get(this.selected) : undefined;
    if (this.selected && active.has(this.selected) && (!result || result.dirty))
      return this.selected;
    return undefined;
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
    const walk = this.forced.delete(id);
    return this.measure(id, walk);
  }

  private async measure(id: string, walk: boolean) {
    const before = this.results.get(id);
    const mark = this.marks.get(id) ?? 0;
    // An operation that touched it while the walk ran may postdate what it saw.
    const touched = () => (this.marks.get(id) ?? 0) !== mark;
    try {
      const response = await this.connection.clients.workspaces.getWorkspace({
        repo: this.repo(),
        workspace: id,
        snapshotFirst: false,
        skipDiskUsage: !walk,
      });
      if (this.stopped) return;
      this.results.set(id, {
        response,
        measuredAt: walk ? this.now() : before?.measuredAt,
        notMeasurable: walk && !response.diskUsage,
        error: null,
        dirty: touched(),
      });
      // The walk updated the daemon's figure: the list shows it.
      if (walk) this.onMeasured();
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
