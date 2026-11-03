// The generated methods, and the generator's rules (tests 11 and 12).

import { readFileSync } from "node:fs";
import { join, resolve } from "node:path";

import { create, createFileRegistry, type DescService } from "@bufbuild/protobuf";
import {
  FieldDescriptorProto_Label as Label,
  FieldDescriptorProto_Type as Type,
  FileDescriptorProtoSchema,
} from "@bufbuild/protobuf/wkt";
import { describe, expect, it } from "vitest";

// Plain JavaScript, typed by its JSDoc (tsc checks it with checkJs).
import * as gen from "../../codegen/protoc-gen-reef.mjs";
import * as apiV1 from "../../src/api/v1.js";
import { Client } from "../../src/client.js";
import { RPCS } from "../../src/gen/rpcs.js";
import { Stream } from "../../src/stream.js";

function isService(value: unknown): value is DescService {
  return (
    typeof value === "object" && value !== null && (value as { kind?: unknown }).kind === "service"
  );
}

const SDK = resolve(import.meta.dirname, "..", "..");
const plugin = gen as unknown as {
  RESERVED: Set<string>;
  pageItems: (file?: string) => Record<string, string | null>;
  methodsOf: (
    schema: unknown,
    table: Record<string, string | null>,
  ) => { name: string; items: string | null }[];
  camel: (name: string) => string;
};

describe("codegen", () => {
  const proto = Client.prototype as unknown as Record<string, unknown>;
  const rpcs = Object.values(RPCS);
  const methods = readFileSync(join(SDK, "src", "gen", "methods.ts"), "utf8");

  it("has a method for every RPC", () => {
    // Every method of every v1 service, counted from the generated descriptors.
    const declared = (Object.values(apiV1) as unknown[])
      .filter(isService)
      .flatMap((service) => service.methods.map((m) => m.name));
    expect(rpcs.map((r) => r.name).sort()).toEqual(declared.sort());
    // Every RPC declares its access; a generator that lost the option would make them all UNSPECIFIED.
    expect(rpcs.filter((r) => (r.access as string) === "UNSPECIFIED")).toEqual([]);
    expect(RPCS.CreateRemoteToken.access).toBe("ADMIN");
    expect(RPCS.GetStatus.access).toBe("READ");
    expect(RPCS.CreateWorkspace.access).toBe("WRITE");
    for (const rpc of rpcs) {
      const name = plugin.camel(rpc.name);
      expect(typeof proto[name], name).toBe("function");
      expect(typeof proto[`${name}Pages`], `${name}Pages`).toBe(
        rpc.paged ? "function" : "undefined",
      );
      expect(typeof proto[`${name}All`], `${name}All`).toBe(
        rpc.items === null ? "undefined" : "function",
      );
      expect(plugin.RESERVED.has(name)).toBe(false);
    }
    expect(
      rpcs
        .filter((r) => r.kind === "server_streaming")
        .map((r) => r.name)
        .sort(),
    ).toEqual(["Backup", "BuildStore", "SubscribeOperations", "SubscribePresence"]);
    expect(RPCS.QueryChanges.paged).toBe(true);
    expect(RPCS.QueryChanges.items).toBeNull();
    expect(RPCS.GetDiff.items).toBe("files");
  });

  it("returns a Stream from a streaming method without starting it", () => {
    const client = Object.create(Client.prototype) as Client;
    // A stream starts on its first iteration, so making one needs no connection; the closed
    // check is all that runs.
    Object.defineProperty(client, "streamCall", {
      value: (..._args: unknown[]) =>
        new Stream(
          () => Promise.resolve((async function* () {})()),
          (e) => Promise.resolve(e),
        ),
    });
    expect(client.subscribeOperations({})).toBeInstanceOf(Stream);
  });

  it("documents every method, and ADMIN ones say so", () => {
    for (const rpc of rpcs) {
      const name = plugin.camel(rpc.name);
      const at = methods.indexOf(`  ${name}(request?`);
      expect(at, name).toBeGreaterThan(0);
      const block = methods.slice(methods.lastIndexOf("/**", at), at);
      expect(block.replace(/[/*\s]/g, "").length, name).toBeGreaterThan(20);
      expect(block.includes("ADMIN: local socket only"), name).toBe(rpc.access === "ADMIN");
    }
  });

  it("asks page-items.json for a paged RPC with two lists", () => {
    const fileProto = (lists: number) =>
      create(FileDescriptorProtoSchema, {
        name: "reef/api/v1/things.proto",
        package: "reef.api.v1",
        syntax: "proto3",
        messageType: [
          {
            name: "ListThingsRequest",
            field: [
              {
                name: "page_token",
                number: 1,
                type: Type.STRING,
                label: Label.OPTIONAL,
                jsonName: "pageToken",
              },
            ],
          },
          {
            name: "ListThingsResponse",
            field: [
              {
                name: "next_page_token",
                number: 1,
                type: Type.STRING,
                label: Label.OPTIONAL,
                jsonName: "nextPageToken",
              },
              ...Array.from({ length: lists }, (_, n) => ({
                name: `things${n}`,
                number: 2 + n,
                type: Type.STRING,
                label: Label.REPEATED,
                jsonName: `things${n}`,
              })),
            ],
          },
        ],
        service: [
          {
            name: "ThingService",
            method: [
              {
                name: "ListThings",
                inputType: ".reef.api.v1.ListThingsRequest",
                outputType: ".reef.api.v1.ListThingsResponse",
              },
            ],
          },
        ],
      });
    const schemaOf = (lists: number) => {
      const file = createFileRegistry(fileProto(lists), () => undefined).getFile(
        "reef/api/v1/things.proto",
      );
      return { files: [file], allFiles: [file] };
    };
    expect(plugin.methodsOf(schemaOf(1), {})[0]?.items).toBe("things0");
    expect(() => plugin.methodsOf(schemaOf(2), {})).toThrow(/sdk\/fixtures\/page-items\.json/);
    expect(plugin.methodsOf(schemaOf(2), { ListThings: "things1" })[0]?.items).toBe("things1");
    expect(plugin.methodsOf(schemaOf(2), { ListThings: null })[0]?.items).toBeNull();
  });

  it("reads the shared paged-items table", () => {
    const table = JSON.parse(
      readFileSync(join(SDK, "..", "fixtures", "page-items.json"), "utf8"),
    ) as {
      items: Record<string, string | null>;
    };
    expect(plugin.pageItems()).toEqual(table.items);
  });
});

describe("floors match the generator", () => {
  const pkg = JSON.parse(readFileSync(join(SDK, "package.json"), "utf8")) as {
    dependencies: Record<string, string>;
    devDependencies: Record<string, string>;
    engines: { node: string };
  };
  const installed = (name: string) =>
    (
      JSON.parse(readFileSync(join(SDK, "node_modules", name, "package.json"), "utf8")) as {
        version: string;
      }
    ).version;

  it("protobuf-es's runtime is at least the generator's", () => {
    expect(pkg.dependencies["@bufbuild/protobuf"]).toBe(
      `^${pkg.devDependencies["@bufbuild/protoc-gen-es"]}`,
    );
    expect(pkg.devDependencies["@bufbuild/protoplugin"]).toBe(
      pkg.devDependencies["@bufbuild/protoc-gen-es"],
    );
  });

  it("the two Connect floors are equal and installed", () => {
    const floor = pkg.dependencies["@connectrpc/connect"];
    expect(pkg.dependencies["@connectrpc/connect-node"]).toBe(floor);
    expect(`^${installed("@connectrpc/connect")}`).toBe(floor);
    expect(`^${installed("@connectrpc/connect-node")}`).toBe(floor);
  });

  it("engines.node is CI's oldest Node", () => {
    const ci = readFileSync(join(SDK, "..", "..", ".github", "workflows", "ci.yml"), "utf8");
    const job = ci.slice(ci.indexOf("\n  typescript:\n"));
    const matrix = /node: \["([0-9.]+)"/.exec(job);
    expect(matrix?.[1]).toBeDefined();
    const oldest = (matrix?.[1] ?? "").split(".").slice(0, 2).join(".");
    expect(pkg.engines.node).toBe(`>=${oldest}`);
  });
});
