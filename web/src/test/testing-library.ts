// The Preact Testing Library, with React Testing Library's `act`, `unmount`
// and `cleanup`, which the tests were written against (#252).
import {
  act as preactAct,
  cleanup as preactCleanup,
  render as preactRender,
  type RenderResult,
} from "@testing-library/preact";
import { setImmediate } from "node:timers";
import { options, type ComponentChild } from "preact";

export * from "@testing-library/preact";

let renders = 0;
const diffed = options.diffed?.bind(options);
options.diffed = (vnode) => {
  renders += 1;
  diffed?.(vnode);
};

// From `node:timers`, which fake timers leave alone, as React's `act` does.
const nextTask = () =>
  new Promise<void>((resolve) => {
    setImmediate(resolve);
  });

async function renderUntilQuiet() {
  for (;;) {
    const before = renders;
    await preactAct(nextTask);
    if (renders === before) return;
  }
}

/**
 * React's `act`: renders and runs effects before returning, and, awaited,
 * goes on rendering what the work it started causes, a task at a time,
 * until a task passes with nothing to render. Preact's renders only once,
 * when the callback settles, so a reply that arrives after that, or a read
 * that an effect starts, is never rendered.
 */
export function act(callback: () => void | Promise<void>): PromiseLike<void> {
  const settled = preactAct(callback);
  return {
    then: (resolve, reject) => settled.then(renderUntilQuiet).then(resolve, reject),
  };
}

/** Unmounts inside `act`, so effect cleanups run before it returns. */
export function render(ui: ComponentChild): RenderResult {
  const result = preactRender(ui);
  return {
    ...result,
    unmount: () => {
      void preactAct(result.unmount);
    },
  };
}

export function cleanup() {
  void preactAct(preactCleanup);
}
