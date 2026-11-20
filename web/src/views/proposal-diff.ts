// The proposal view's symbol diff, as data (R3.31): `GetDiff`'s files
// ordered by how many definitions changed, each covered file's changes
// nested by `enclosing_symbol`, and what a file says when the index could
// not answer for it. The page renders this; nothing here reads the daemon.
import {
  SymbolChangeKind,
  SymbolCoverage,
  type FileDiff,
  type SymbolChange,
} from "../gen/reef/api/v1/diff_pb";
import { SymbolKind } from "../gen/reef/api/v1/symbols_pb";

/** A changed definition and the changed definitions inside it. */
export interface SymbolNode {
  change: SymbolChange;
  children: SymbolNode[];
}

/** One file of the diff, as the page shows it. */
export interface DiffFile {
  path: string;
  file: FileDiff;
  /** Definitions at the top level of the file, in the diff's order; empty when not covered. */
  symbols: SymbolNode[];
  /** How many definitions changed, nested ones included. */
  changed: number;
  /** Why the file shows by lines, or empty when its definitions are listed. */
  byLines: string;
}

export const KIND_WORDS: Record<SymbolChangeKind, string> = {
  [SymbolChangeKind.UNSPECIFIED]: "changed",
  [SymbolChangeKind.ADDED]: "added",
  [SymbolChangeKind.REMOVED]: "removed",
  [SymbolChangeKind.CHANGED]: "changed",
  [SymbolChangeKind.MOVED]: "moved",
};

const COVERAGE_WORDS: Partial<Record<SymbolCoverage, string>> = {
  [SymbolCoverage.UNSUPPORTED]: "no symbol index for this language",
  [SymbolCoverage.SKIPPED]: "skipped by the symbol index",
  [SymbolCoverage.NO_SCIP]: "no SCIP index uploaded for it",
  [SymbolCoverage.DIFF_TOO_LARGE]: "the diff is too large for symbols",
};

/** A definition's kind, in lower case ("function", "struct"). */
export function symbolKindWord(kind: SymbolKind): string {
  const name = (SymbolKind as Record<number, string | undefined>)[kind];
  return kind === SymbolKind.UNSPECIFIED || !name ? "definition" : name.toLowerCase();
}

/** Nests `changes` by `enclosing_symbol`; a change whose container did not change stays on top. */
export function nest(changes: SymbolChange[]): SymbolNode[] {
  const nodes = new Map<string, SymbolNode>();
  for (const change of changes) {
    if (change.symbol && !nodes.has(change.symbol)) {
      nodes.set(change.symbol, { change, children: [] });
    }
  }
  const top: SymbolNode[] = [];
  for (const change of changes) {
    const node = (change.symbol && nodes.get(change.symbol)) || { change, children: [] };
    if (node.change !== change) {
      // A second change with the same symbol (a removed and an added
      // definition of one name): its own row.
      top.push({ change, children: [] });
      continue;
    }
    const parent = change.enclosingSymbol ? nodes.get(change.enclosingSymbol) : undefined;
    if (parent && parent !== node) parent.children.push(node);
    else top.push(node);
  }
  return top;
}

function count(nodes: SymbolNode[]): number {
  return nodes.reduce((total, node) => total + 1 + count(node.children), 0);
}

/** One page of `files` as the page lists it: most changed definitions first, then by path. */
export function diffFiles(files: FileDiff[]): DiffFile[] {
  const listed = files.map((file): DiffFile => {
    const path = file.change?.path ?? "";
    const symbols = file.symbols;
    const covered = symbols?.coverage === SymbolCoverage.COVERED;
    const nested = covered ? nest(symbols.changes) : [];
    let byLines = "";
    if (file.binary) byLines = "binary";
    else if (file.tooLarge) byLines = "too large to diff";
    else if (!symbols) byLines = "no symbols asked for";
    else if (!covered) byLines = COVERAGE_WORDS[symbols.coverage] ?? "no symbols";
    else if (nested.length === 0) byLines = "no definition changed";
    return { path, file, symbols: nested, changed: count(nested), byLines };
  });
  return listed.sort((a, b) => b.changed - a.changed || a.path.localeCompare(b.path));
}

/** Lines in a definition, as "+12 −3". */
export function lineCounts(added: number, removed: number): string {
  return `+${String(added)} −${String(removed)}`;
}

/** What a definition expands by: its symbol, else its name. */
export function expandKey(change: SymbolChange): string {
  return change.symbol || change.name;
}
