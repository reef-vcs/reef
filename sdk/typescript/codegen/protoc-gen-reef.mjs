#!/usr/bin/env node
// @ts-check
// Writes the client's methods from the schema (R2.07, ADR 0054): a protoc plugin that buf runs
// beside protoc-gen-es (buf.gen.yaml). It writes, under src/gen/:
//
// - rpcs.ts: every RPC's path, descriptor, access, kind and paged items;
// - methods.ts: the abstract class Client extends, with one method per RPC in lower camel case,
//   `<method>Pages` for a paged RPC, and `<method>All` where its items are known (the response's
//   one repeated field, or sdk/fixtures/page-items.json's entry, which the Python generator reads
//   too);
// - api-v1.ts: the `reef-vcs/api/v1` entry, every v1 file's exports.
//
// Plain JavaScript with JSDoc types, so it runs on every Node in the matrix without a loader.

import { readFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { getOption } from "@bufbuild/protobuf";
import { createEcmaScriptPlugin, getComments, runNodeJs } from "@bufbuild/protoplugin";

/** @import { DescExtension, DescField, DescFile, DescMessage, DescMethod } from "@bufbuild/protobuf" */
/** @import { Schema } from "@bufbuild/protoplugin" */

const PACKAGE = "reef.api.v1";
const ACCESS_EXTENSION = "reef.api.v1.access";
const ACCESS_FIELD = 50701;
const ACCESS_NAMES = { 1: "READ", 2: "WRITE", 3: "ADMIN" };
const HERE = dirname(fileURLToPath(import.meta.url));
export const PAGE_ITEMS_FILE = join(HERE, "..", "..", "fixtures", "page-items.json");

/**
 * The client's hand-written members, which no generated method may shadow.
 */
export const RESERVED = new Set([
  "close",
  "withOptions",
  "workspace",
  "withWorkspace",
  "attach",
  "withAttached",
  "readFileToEnd",
  "daemonStatus",
  "requireRevision",
  "target",
  "agent",
  "remote",
  "connection",
  "unaryCall",
  "streamCall",
  "pagesCall",
  "itemsCall",
]);

export class GenerateError extends Error {}

/**
 * The paged items table.
 * @param {string} [file]
 * @returns {Record<string, string | null>}
 */
export function pageItems(file = PAGE_ITEMS_FILE) {
  const table = JSON.parse(readFileSync(file, "utf8"));
  return table.items;
}

/**
 * `CreateWorkspace` as `createWorkspace`.
 * @param {string} name
 */
export function camel(name) {
  return name.charAt(0).toLowerCase() + name.slice(1);
}

/**
 * @typedef {object} Method
 * @property {string} service The service's name, `WorkspaceService`.
 * @property {string} name The RPC's name, `CreateWorkspace`.
 * @property {string} local The method's name on the client, `createWorkspace`.
 * @property {string} path `/reef.api.v1.WorkspaceService/CreateWorkspace`.
 * @property {string} file The import path of the generated file, `./reef/api/v1/workspaces_pb.js`.
 * @property {string} serviceLocal The generated service constant, `WorkspaceService`.
 * @property {string} methodLocal The key under the service's `method`, `createWorkspace`.
 * @property {string} input `CreateWorkspaceRequest`.
 * @property {string} inputFile
 * @property {string} output `CreateWorkspaceResponse`.
 * @property {string} outputFile
 * @property {"READ" | "WRITE" | "ADMIN" | "UNSPECIFIED"} access
 * @property {"unary" | "server_streaming"} kind
 * @property {boolean} paged
 * @property {string | null} items The response field `All` yields, in its TypeScript name.
 * @property {string} comment The RPC's leading comment.
 */

/**
 * The generated file a descriptor's file maps to, as an import path from src/gen/.
 * @param {DescFile} file
 */
function importPath(file) {
  return `./${file.name}_pb.js`;
}

/**
 * @param {Schema<object>} schema
 * @returns {DescExtension | undefined}
 */
function accessExtension(schema) {
  for (const file of schema.allFiles) {
    for (const ext of file.extensions) {
      if (ext.typeName === ACCESS_EXTENSION) {
        return ext;
      }
    }
  }
  return undefined;
}

/**
 * @param {DescMethod} method
 * @param {DescExtension | undefined} ext
 * @returns {Method["access"]}
 */
function accessOf(method, ext) {
  const value =
    ext === undefined
      ? unknownAccess(method)
      : /** @type {number} */ (getOption(method, /** @type {any} */ (ext)));
  return /** @type {Record<number, Method["access"]>} */ (ACCESS_NAMES)[value] ?? "UNSPECIFIED";
}

/**
 * `(reef.api.v1.access)` read from the options' unknown field 50701, as clientgen.py reads it,
 * for a schema whose options.proto the plugin was not given.
 * @param {DescMethod} method
 * @returns {number}
 */
function unknownAccess(method) {
  const unknown = /** @type {{ no: number; wireType: number; data: Uint8Array }[] | undefined} */ (
    /** @type {any} */ (method.proto.options)?.$unknown
  );
  const field = unknown?.find((u) => u.no === ACCESS_FIELD && u.wireType === 0);
  if (field === undefined) {
    return 0;
  }
  let value = 0;
  for (let i = 0, shift = 0; i < field.data.length; i++, shift += 7) {
    const byte = /** @type {number} */ (field.data[i]);
    value |= (byte & 0x7f) << shift;
    if ((byte & 0x80) === 0) {
      break;
    }
  }
  return value;
}

/**
 * The repeated fields of a message, maps aside.
 * @param {DescMessage} message
 * @returns {DescField[]}
 */
function lists(message) {
  return message.fields.filter((f) => f.fieldKind === "list");
}

/**
 * Every RPC of the v1 files, in file and declaration order.
 * @param {Schema<object>} schema
 * @param {Record<string, string | null>} table
 * @returns {Method[]}
 */
export function methodsOf(schema, table) {
  const ext = accessExtension(schema);
  /** @type {Method[]} */
  const methods = [];
  for (const file of schema.files) {
    if (file.proto.package !== PACKAGE) {
      continue;
    }
    for (const service of file.services) {
      for (const rpc of service.methods) {
        if (rpc.methodKind !== "unary" && rpc.methodKind !== "server_streaming") {
          throw new GenerateError(
            `${rpc.name} is ${rpc.methodKind}; the client has no form for it`,
          );
        }
        /** @type {Method} */
        const method = {
          service: service.name,
          name: rpc.name,
          local: camel(rpc.name),
          path: `/${service.typeName}/${rpc.name}`,
          file: importPath(file),
          serviceLocal: service.name,
          methodLocal: rpc.localName,
          input: rpc.input.name,
          inputFile: importPath(rpc.input.file),
          output: rpc.output.name,
          outputFile: importPath(rpc.output.file),
          access: accessOf(rpc, ext),
          kind: rpc.methodKind,
          paged: false,
          items: null,
          comment: getComments(rpc).leading ?? "",
        };
        pages(method, rpc, table);
        const names = [method.local];
        if (method.paged) {
          names.push(`${method.local}Pages`);
        }
        if (method.items !== null) {
          names.push(`${method.local}All`);
        }
        for (const name of names) {
          if (RESERVED.has(name)) {
            throw new GenerateError(
              `${rpc.name} would be the client's ${name}, which is hand-written; rename the RPC or the member`,
            );
          }
        }
        methods.push(method);
      }
    }
  }
  return methods;
}

/**
 * Settles whether an RPC is paged, and what its `All` yields.
 * @param {Method} method
 * @param {DescMethod} rpc
 * @param {Record<string, string | null>} table
 */
function pages(method, rpc, table) {
  const token = rpc.input.fields.some((f) => f.name === "page_token");
  const next = rpc.output.fields.some((f) => f.name === "next_page_token");
  if (!token || !next) {
    return;
  }
  if (method.kind === "server_streaming") {
    throw new GenerateError(`${rpc.name} is paged and streams`);
  }
  method.paged = true;
  const repeated = lists(rpc.output);
  /** @type {string | null} */
  let items;
  if (Object.hasOwn(table, rpc.name)) {
    items = table[rpc.name] ?? null;
  } else if (repeated.length === 1) {
    items = /** @type {DescField} */ (repeated[0]).name;
  } else {
    throw new GenerateError(
      `${rpc.name} is paged and its response has ${repeated.length} repeated fields; name its items in sdk/fixtures/page-items.json`,
    );
  }
  if (items === null) {
    return;
  }
  const chosen = repeated.find((f) => f.name === items);
  if (chosen === undefined) {
    throw new GenerateError(
      `sdk/fixtures/page-items.json names ${rpc.name}.${items}, which is not a repeated field`,
    );
  }
  method.items = chosen.localName;
}

/**
 * A JSDoc block's body lines, safe inside `/** ... *\/`.
 * @param {string} text
 * @param {string} indent
 */
function jsdoc(text, indent) {
  const lines = text
    .replace(/\*\//g, "*\\/")
    .replace(/\s+$/, "")
    .split("\n")
    .map((line) => (line.trim() === "" ? `${indent} *` : `${indent} * ${line.replace(/^ /, "")}`));
  return `${indent}/**\n${lines.join("\n")}\n${indent} */\n`;
}

/**
 * @param {Method} method
 * @param {string} [extra]
 */
function doc(method, extra = "") {
  const parts = [];
  parts.push(method.comment.trim() || `\`${PACKAGE}.${method.service}.${method.name}\`.`);
  const notes = [`\`${PACKAGE}.${method.service}.${method.name}\`, access ${method.access}.`];
  if (method.access === "ADMIN") {
    notes.push("ADMIN: local socket only; a remote daemon refuses it.");
  }
  if (extra) {
    notes.push(extra);
  }
  parts.push(notes.join(" "));
  parts.push(
    `The request is a \`${method.input}\`, given as an object of its fields or as a message.`,
  );
  return parts.join("\n\n");
}

const HEADER = "// Generated from the schema by codegen/protoc-gen-reef.mjs; do not edit.\n\n";

/**
 * Type imports for every message a list of methods names, grouped by file.
 * @param {Method[]} methods
 * @param {(m: Method) => [string, string][]} pick
 */
function imports(methods, pick) {
  /** @type {Map<string, Set<string>>} */
  const byFile = new Map();
  for (const m of methods) {
    for (const [file, name] of pick(m)) {
      let names = byFile.get(file);
      if (names === undefined) {
        names = new Set();
        byFile.set(file, names);
      }
      names.add(name);
    }
  }
  return [...byFile.entries()]
    .sort(([a], [b]) => a.localeCompare(b))
    .map(([file, names]) => ({ file, names: [...names].sort() }));
}

/**
 * src/gen/rpcs.ts.
 * @param {Method[]} methods
 */
export function rpcsModule(methods) {
  let out = HEADER;
  out += 'import type { Rpc } from "../rpc.js";\n';
  for (const { file, names } of imports(methods, (m) => [[m.file, m.serviceLocal]])) {
    out += `import { ${names.join(", ")} } from "${file}";\n`;
  }
  out += "\n/** Every RPC of `reef.api.v1`: its path, descriptor, access level and paging. */\n";
  out += "export const RPCS = {\n";
  for (const m of methods) {
    out += `  ${m.name}: {\n`;
    out += `    name: "${m.name}",\n`;
    out += `    path: "${m.path}",\n`;
    out += `    method: ${m.serviceLocal}.method.${m.methodLocal},\n`;
    out += `    access: "${m.access}",\n`;
    out += `    kind: "${m.kind}",\n`;
    out += `    paged: ${m.paged},\n`;
    out += `    items: ${m.items === null ? "null" : `"${m.items}"`},\n`;
    out += "  },\n";
  }
  out += "} as const satisfies Record<string, Rpc>;\n";
  return out;
}

/**
 * src/gen/methods.ts.
 * @param {Method[]} methods
 */
export function methodsModule(methods) {
  let out = HEADER;
  out +=
    'import type { DescMessage, MessageInitShape, MessageShape } from "@bufbuild/protobuf";\n\n';
  out += 'import type { CallOptions } from "../options.js";\n';
  out += 'import type { Rpc } from "../rpc.js";\n';
  out += 'import type { Stream } from "../stream.js";\n';
  out += 'import { RPCS } from "./rpcs.js";\n';
  for (const { file, names } of imports(methods, (m) => [
    [m.inputFile, `${m.input}Schema`],
    [m.outputFile, m.output],
  ])) {
    out += `import type { ${names.join(", ")} } from "${file}";\n`;
  }
  out += "\n";
  out += "/** One method per RPC of `reef.api.v1`, which {@link Client} implements. */\n";
  out += "export abstract class Methods {\n";
  out += "  /** @internal */\n";
  out +=
    "  protected abstract unaryCall<I extends DescMessage, O extends DescMessage>(rpc: Rpc<I, O>, request: MessageInitShape<I> | undefined, options: CallOptions | undefined): Promise<MessageShape<O>>;\n";
  out += "  /** @internal */\n";
  out +=
    "  protected abstract streamCall<I extends DescMessage, O extends DescMessage>(rpc: Rpc<I, O>, request: MessageInitShape<I> | undefined, options: CallOptions | undefined): Stream<MessageShape<O>>;\n";
  out += "  /** @internal */\n";
  out +=
    "  protected abstract pagesCall<I extends DescMessage, O extends DescMessage>(rpc: Rpc<I, O>, request: MessageInitShape<I> | undefined, options: CallOptions | undefined): AsyncIterable<MessageShape<O>>;\n";
  out += "  /** @internal */\n";
  out +=
    "  protected abstract itemsCall<I extends DescMessage, O extends DescMessage, T>(rpc: Rpc<I, O>, request: MessageInitShape<I> | undefined, options: CallOptions | undefined, field: string): AsyncIterable<T>;\n";
  for (const m of methods) {
    const req = `request?: MessageInitShape<typeof ${m.input}Schema>`;
    const opts = "options?: CallOptions";
    const rpc = `RPCS.${m.name}`;
    out += "\n";
    if (m.kind === "server_streaming") {
      out += jsdoc(
        doc(
          m,
          "A server stream: iterate it with `for await`; leaving `await using`, `cancel()` or the signal ends it.",
        ),
        "  ",
      );
      out += `  ${m.local}(${req}, ${opts}): Stream<${m.output}> {\n`;
      out += `    return this.streamCall(${rpc}, request, options);\n  }\n`;
      continue;
    }
    out += jsdoc(doc(m), "  ");
    out += `  ${m.local}(${req}, ${opts}): Promise<${m.output}> {\n`;
    out += `    return this.unaryCall(${rpc}, request, options);\n  }\n`;
    if (!m.paged) {
      continue;
    }
    out += "\n";
    out += jsdoc(
      doc(
        m,
        "Every page, lazily: the same request with each `nextPageToken`, until one comes back empty. `timeoutMs` applies to each page.",
      ),
      "  ",
    );
    out += `  ${m.local}Pages(${req}, ${opts}): AsyncIterable<${m.output}> {\n`;
    out += `    return this.pagesCall(${rpc}, request, options);\n  }\n`;
    if (m.items === null) {
      continue;
    }
    out += "\n";
    out += jsdoc(doc(m, `Every page's \`${m.items}\`, item by item.`), "  ");
    out += `  ${m.local}All(${req}, ${opts}): AsyncIterable<${m.output}["${m.items}"][number]> {\n`;
    out += `    return this.itemsCall(${rpc}, request, options, "${m.items}");\n  }\n`;
  }
  out += "}\n";
  return out;
}

/**
 * src/gen/api-v1.ts: the `reef-vcs/api/v1` entry.
 * @param {Schema<object>} schema
 */
export function apiModule(schema) {
  const files = schema.files
    .filter((f) => f.proto.package === PACKAGE)
    .map((f) => importPath(f))
    .sort();
  let out = HEADER;
  out += "// Every message, enum and service of reef.api.v1, and protobuf-es's helpers for them.\n";
  for (const file of files) {
    out += `export * from "${file}";\n`;
  }
  out +=
    'export {\n  create,\n  fromBinary,\n  fromJson,\n  isMessage,\n  toBinary,\n  toJson,\n} from "@bufbuild/protobuf";\n';
  out += 'export type { MessageInitShape, MessageShape } from "@bufbuild/protobuf";\n';
  out +=
    'export {\n  durationFromMs,\n  durationMs,\n  timestampDate,\n  timestampFromDate,\n  timestampFromMs,\n  timestampMs,\n} from "@bufbuild/protobuf/wkt";\n';
  out += 'export type { Duration, Timestamp } from "@bufbuild/protobuf/wkt";\n';
  return out;
}

export const plugin = createEcmaScriptPlugin({
  name: "protoc-gen-reef",
  version: "v1",
  generateTs(schema) {
    const methods = methodsOf(schema, pageItems());
    schema.generateFile("rpcs.ts").print(rpcsModule(methods));
    schema.generateFile("methods.ts").print(methodsModule(methods));
    schema.generateFile("api-v1.ts").print(apiModule(schema));
  },
});

if (process.argv[1] === fileURLToPath(import.meta.url)) {
  runNodeJs(plugin);
}
