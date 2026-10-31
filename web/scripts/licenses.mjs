// Writes THIRD_PARTY_LICENSES.txt beside the built app (R2.18): every package
// in the production dependency tree, with its version, license and license
// text. Fails on a license that deny.toml's `allow` does not list, so the
// bundle is held to the same list as the Rust dependencies.
import { readFileSync, readdirSync, writeFileSync } from "node:fs";
import { join } from "node:path";

const web = join(import.meta.dirname, "..");
const out = join(web, "../crates/reef-web/assets/THIRD_PARTY_LICENSES.txt");

const deny = readFileSync(join(web, "../deny.toml"), "utf8");
const allowBlock = /^allow\s*=\s*\[([^\]]*)\]/m.exec(deny)?.[1] ?? "";
const allowed = new Set([...allowBlock.matchAll(/"([^"]+)"/g)].map((m) => m[1]));

// An SPDX expression is allowed when one side of every OR and both sides of
// every AND are. Package licenses are short, so this needs no full parser.
function permitted(expression) {
  const inner = expression.trim().replace(/^\((.*)\)$/, "$1");
  if (/ OR /.test(inner)) return inner.split(/ OR /).some(permitted);
  if (/ AND /.test(inner)) return inner.split(/ AND /).every(permitted);
  return allowed.has(inner.trim());
}

const lock = JSON.parse(readFileSync(join(web, "package-lock.json"), "utf8"));
const shipped = Object.entries(lock.packages)
  .filter(([path, entry]) => path.startsWith("node_modules/") && !entry.dev && !entry.optional)
  .map(([path]) => path)
  .sort();

const refused = [];
const sections = shipped.map((path) => {
  const dir = join(web, path);
  const manifest = JSON.parse(readFileSync(join(dir, "package.json"), "utf8"));
  const license = typeof manifest.license === "string" ? manifest.license : "UNKNOWN";
  if (!permitted(license)) refused.push(`${manifest.name}@${manifest.version}: ${license}`);
  const file = readdirSync(dir).find((name) => /^(licen[cs]e|copying)(\.|$)/i.test(name));
  const text = file ? readFileSync(join(dir, file), "utf8").trim() : `License: ${license}`;
  return `${manifest.name} ${manifest.version} (${license})\n\n${text}\n`;
});

if (refused.length > 0) {
  console.error(`licenses not in deny.toml's allow list:\n  ${refused.join("\n  ")}`);
  process.exit(1);
}

const rule = `\n${"-".repeat(72)}\n\n`;
writeFileSync(out, `The Reef web UI bundles these packages.\n${rule}${sections.join(rule)}`);
console.log(`wrote ${out} (${shipped.length} packages)`);
