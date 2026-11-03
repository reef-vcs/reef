#!/usr/bin/env node
// @ts-check
// Packs reef-vcs at the workspace version (R2.07): copies package.json, README.md, LICENSE and
// dist/ into pack/, sets the version there from Cargo.toml (the committed package.json keeps a
// placeholder, so a release bumps nothing), checks the build carries the same version, and runs
// `npm pack` into dist-pack/.

import { execFileSync } from "node:child_process";
import { cpSync, existsSync, mkdirSync, readFileSync, rmSync, writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";

import { workspaceVersion } from "../codegen/generate.mjs";

const SDK = dirname(dirname(fileURLToPath(import.meta.url)));
const PACK = join(SDK, "pack");
const OUT = join(SDK, "dist-pack");

const version = workspaceVersion();
const built = join(SDK, "dist", "gen", "build.js");
if (!existsSync(built)) {
  throw new Error("dist/ is missing: run `npm run build` first");
}
const match = readFileSync(built, "utf8").match(/export const VERSION = "([^"]+)";/);
if (match?.[1] !== version) {
  throw new Error(`dist/ was built at ${match?.[1] ?? "no version"}, not ${version}: rebuild`);
}

rmSync(PACK, { recursive: true, force: true });
rmSync(OUT, { recursive: true, force: true });
mkdirSync(PACK);
mkdirSync(OUT);
for (const file of ["README.md", "LICENSE"]) {
  cpSync(join(SDK, file), join(PACK, file));
}
cpSync(join(SDK, "dist"), join(PACK, "dist"), { recursive: true });
const manifest = JSON.parse(readFileSync(join(SDK, "package.json"), "utf8"));
manifest.version = version;
delete manifest.scripts;
delete manifest.devDependencies;
writeFileSync(join(PACK, "package.json"), JSON.stringify(manifest, null, 2) + "\n");
execFileSync("npm", ["pack", "--pack-destination", OUT], { cwd: PACK, stdio: "inherit" });
