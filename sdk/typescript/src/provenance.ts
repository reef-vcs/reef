// What a workspace's work is declared to be for: its provenance context (R3.04, R3.44).
//
// Every commit the daemon records for a workspace gets a signed provenance record naming the
// context declared for it: the model, the harness, the task, the instruction and the harness's own
// session. `client.workspace({ provenance })` declares it at creation, and `handle.declare(...)`
// replaces fields later. An instruction is given as its text and hashed here with SHA-256 over its
// exact UTF-8 bytes: only the digest is kept and sent, never the text. Everything declared is a
// claim the daemon records, not something it verifies.

import { createHash } from "node:crypto";

import type { MessageInitShape } from "@bufbuild/protobuf";

import { SessionActor } from "./gen/reef/api/v1/identity_pb.js";
import type { ProvenanceDeclarationSchema } from "./gen/reef/api/v1/provenance_pb.js";

/** The API revision that added provenance (R3.04). */
export const PROVENANCE_REVISION = 18;

/** The reason a daemon that records no provenance refuses a declaration with. */
export const PROVENANCE_DISABLED = "PROVENANCE_DISABLED";

/** The environment variables a new workspace's model and task default to, as `reef ws create`. */
export const MODEL_ENV = "REEF_MODEL";
export const TASK_ENV = "REEF_TASK";

/**
 * A declaration: each field `undefined` to leave it, `""` to clear it (on `declare`).
 * `instruction` is the instruction's text, hashed here and never sent; `identity` and `actor`
 * bind the workspace's provenance session and are taken only at creation.
 */
export interface Provenance {
  model?: string | undefined;
  harness?: string | undefined;
  task?: string | undefined;
  /** The instruction's text (or bytes): only its SHA-256 is sent. */
  instruction?: string | Uint8Array | undefined;
  /** The instruction's SHA-256, if already computed; not with `instruction`. */
  instructionSha256?: string | undefined;
  /** The harness's own session id. */
  clientSession?: string | undefined;
  /** The person identity the workspace's provenance is bound to; creation only. */
  identity?: string | undefined;
  /** Who works in it; creation only. */
  actor?: "agent" | "executor" | "human" | undefined;
}

/** The daemon's limits on a declared label (model, harness, client session) and a task. */
export const MAX_LABEL_BYTES = 128;
export const MAX_TASK_BYTES = 512;

/** Whether a record can carry `value` as a label: 1 to 128 bytes of visible ASCII. */
export function isLabel(value: string): boolean {
  return /^[\x21-\x7e]{1,128}$/.test(value);
}

/** Whether a record can carry `value` as a task: 1 to 512 bytes of UTF-8, no control characters. */
export function isTask(value: string): boolean {
  const bytes = Buffer.byteLength(value, "utf8");
  return bytes > 0 && bytes <= MAX_TASK_BYTES && !/\p{Cc}/u.test(value);
}

/** The SHA-256 of an instruction's exact bytes (UTF-8 for text), as lowercase hex. */
export function instructionSha256(instruction: string | Uint8Array): string {
  return createHash("sha256").update(instruction).digest("hex");
}

const ACTORS = {
  agent: SessionActor.AGENT,
  executor: SessionActor.EXECUTOR,
  human: SessionActor.HUMAN,
} as const;

/**
 * The `ProvenanceDeclaration` message for `provenance`, the instruction hashed. A new workspace
 * (`creating`) has nothing to clear, so an empty field there declares nothing.
 */
export function declaration(
  provenance: Provenance,
  creating: boolean,
): MessageInitShape<typeof ProvenanceDeclarationSchema> {
  if (provenance.instruction !== undefined && provenance.instructionSha256 !== undefined) {
    throw new TypeError("give instruction or instructionSha256, not both");
  }
  if (!creating && (provenance.identity !== undefined || provenance.actor !== undefined)) {
    throw new TypeError(
      "identity and actor are fixed when the workspace is created; declare() takes the context fields only",
    );
  }
  // An empty instruction clears it, as an empty field does.
  const sha =
    provenance.instruction !== undefined
      ? provenance.instruction.length === 0
        ? ""
        : instructionSha256(provenance.instruction)
      : provenance.instructionSha256?.toLowerCase();
  const out: MessageInitShape<typeof ProvenanceDeclarationSchema> = {};
  const set = (value: string | undefined): string | undefined =>
    value === undefined || (creating && value.trim() === "") ? undefined : value.trim();
  const model = set(provenance.model);
  const harness = set(provenance.harness);
  const task = set(provenance.task);
  const instruction = set(sha);
  const clientSession = set(provenance.clientSession);
  if (model !== undefined) out.model = model;
  if (harness !== undefined) out.harness = harness;
  if (task !== undefined) out.task = task;
  if (instruction !== undefined) out.instructionSha256 = instruction;
  if (clientSession !== undefined) out.clientSession = clientSession;
  if (provenance.identity) out.identity = provenance.identity;
  if (provenance.actor !== undefined) {
    const name: string = provenance.actor;
    if (!Object.hasOwn(ACTORS, name)) {
      throw new TypeError(`actor is agent, executor or human, not ${name}`);
    }
    out.actor = ACTORS[name as keyof typeof ACTORS];
  }
  return out;
}

/**
 * What a new workspace declares: `provenance` with `REEF_MODEL` and `REEF_TASK` where it sets no
 * model or task, and whether the caller gave any of it (else a daemon that cannot take it is not
 * an error). `undefined` when nothing is declared.
 */
export function createPlan(
  provenance: Provenance | undefined,
  env: NodeJS.ProcessEnv = process.env,
):
  | { declaration: MessageInitShape<typeof ProvenanceDeclarationSchema>; explicit: boolean }
  | undefined {
  // A value the daemon would refuse is ignored: the environment never fails a creation.
  const fromEnv = (name: string, ok: (value: string) => boolean): string | undefined => {
    const value = env[name]?.trim();
    return value && ok(value) ? value : undefined;
  };
  const given = provenance ?? {};
  const merged: Provenance = {
    ...given,
    model: given.model ?? fromEnv(MODEL_ENV, isLabel),
    task: given.task ?? fromEnv(TASK_ENV, isTask),
  };
  const message = declaration(merged, true);
  if (Object.keys(message).length === 0) {
    return undefined;
  }
  const explicit = Object.keys(declaration(given, true)).length > 0;
  return { declaration: message, explicit };
}
