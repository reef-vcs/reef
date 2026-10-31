// Rendering waits for an animation frame; reading the streams never does.

/** Runs `run` before the next paint. Browsers pause this in hidden tabs. */
export type FrameScheduler = (run: () => void) => void;

export const animationFrame: FrameScheduler = (run) => {
  requestAnimationFrame(() => {
    run();
  });
};

/**
 * Calls the listeners once per frame however many times `notify` ran since
 * the last one, so a burst of a thousand stream frames is one React commit.
 */
export class FrameNotifier {
  private scheduled = false;
  private readonly listeners = new Set<() => void>();

  constructor(private readonly frame: FrameScheduler) {}

  subscribe(listener: () => void): () => void {
    this.listeners.add(listener);
    return () => {
      this.listeners.delete(listener);
    };
  }

  notify() {
    if (this.scheduled) return;
    this.scheduled = true;
    this.frame(() => {
      this.scheduled = false;
      for (const listener of [...this.listeners]) listener();
    });
  }
}
