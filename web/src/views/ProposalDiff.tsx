// The proposal page's diff (R3.31): the changed definitions of each file,
// by `GetDiff { symbols, max_call_sites: 0 }`, and lines on demand. A
// definition expands to its own lines and call sites; a file the index
// cannot answer for expands to its hunks. Expansions are read one at a
// time and kept for the commit, whose diff never changes. Nothing is cut
// by the page (ADR 0043): a cut the daemon made says so.
import { useEffect, useState } from "react";
import { client, type Connection } from "../api/connection";
import { isSessionInvalid, reefError, type ReefError } from "../api/errors";
import { useRetries } from "../api/streams";
import { LineSide } from "../gen/reef/api/v1/comments_pb";
import {
  DiffFormat,
  DiffService,
  type FileDiff,
  type GetDiffResponse,
  type Hunk,
} from "../gen/reef/api/v1/diff_pb";
import { ErrorView } from "../ui/ErrorView";
import { plural } from "../ui/words";
import {
  diffFiles,
  expandKey,
  KIND_WORDS,
  lineCounts,
  symbolKindWord,
  type DiffFile,
  type SymbolNode,
} from "./proposal-diff";

/** Files per page of the symbol list. */
const FILES_PER_PAGE = 100;

/** Where a line comment goes: a path, a side and a line. */
export interface LineAnchor {
  path: string;
  side: LineSide;
  line: number;
}

interface Expanded {
  files: FileDiff[];
}

type Expansion = { kind: "loading" } | { kind: "failed"; error: ReefError } | Expanded;

function useSymbolDiff(
  connection: Connection,
  repo: string,
  commit: string,
  onSessionInvalid: () => void,
) {
  const [pages, setPages] = useState<{ commit: string; responses: GetDiffResponse[] } | null>(null);
  // Answers, errors and the page asked for are kept with their commit.
  const [error, setError] = useState<{ commit: string; error: ReefError } | null>(null);
  const [page, setPage] = useState({ commit, token: "" });
  const token = page.commit === commit ? page.token : "";
  const retries = useRetries(connection, error !== null);
  useEffect(() => {
    let current = true;
    client(connection, DiffService)
      .getDiff({
        repo,
        toRevision: commit,
        format: DiffFormat.STAT,
        symbols: true,
        maxCallSites: 0,
        pageSize: FILES_PER_PAGE,
        pageToken: token,
        skipSnapshot: true,
      })
      .then(
        (response) => {
          if (!current) return;
          setError(null);
          setPages((before) => ({
            commit,
            responses:
              before?.commit === commit && token ? [...before.responses, response] : [response],
          }));
        },
        (thrown: unknown) => {
          if (!current) return;
          const failed = reefError(thrown);
          if (isSessionInvalid(failed)) onSessionInvalid();
          else setError({ commit, error: failed });
        },
      );
    return () => {
      current = false;
    };
  }, [connection, retries, repo, commit, token, onSessionInvalid]);
  const responses = pages?.commit === commit ? pages.responses : null;
  const next = responses?.at(-1)?.nextPageToken ?? "";
  return {
    responses,
    error: error?.commit === commit ? error.error : null,
    more: next
      ? () => {
          setPage({ commit, token: next });
        }
      : undefined,
  };
}

/** One line of a hunk, numbered on each side it is on. */
interface HunkLine {
  text: string;
  sign: " " | "+" | "-";
  old: number | undefined;
  new: number | undefined;
}

export function hunkLines(hunk: Hunk): HunkLine[] {
  let old = hunk.oldStart;
  let next = hunk.newStart;
  const lines = hunk.lines.endsWith("\n") ? hunk.lines.slice(0, -1) : hunk.lines;
  return lines.split("\n").map((line) => {
    const sign = line[0] === "+" || line[0] === "-" ? line[0] : " ";
    const text = line.slice(1);
    if (sign === "+") return { text, sign, old: undefined, new: next++ };
    if (sign === "-") return { text, sign, old: old++, new: undefined };
    return { text, sign, old: old++, new: next++ };
  });
}

function Hunks({
  file,
  onAnchor,
}: {
  file: FileDiff;
  onAnchor: ((anchor: LineAnchor) => void) | undefined;
}) {
  const path = file.change?.path ?? "";
  if (file.binary) return <p className="secondary">A binary file: no lines to show.</p>;
  if (file.tooLarge) return <p className="secondary">Too large to diff.</p>;
  if (file.hunks.length === 0) return <p className="secondary">No lines changed.</p>;
  return (
    <div className="hunks">
      {file.hunks.map((hunk, h) => (
        <pre key={h} className="hunk">
          <span className="hunk-head secondary">
            @@ −{hunk.oldStart},{hunk.oldLines} +{hunk.newStart},{hunk.newLines} @@
          </span>
          {hunkLines(hunk).map((line, i) => {
            const side = line.sign === "-" ? LineSide.OLD : LineSide.NEW;
            const number = line.sign === "-" ? line.old : line.new;
            const className =
              line.sign === "+" ? "line added" : line.sign === "-" ? "line removed" : "line";
            return (
              <span key={i} className={className}>
                {onAnchor && number !== undefined ? (
                  <button
                    type="button"
                    className="line-number"
                    title={`Comment on line ${String(number)}`}
                    aria-label={`Comment on ${path} line ${String(number)}`}
                    onClick={() => {
                      onAnchor({ path, side, line: number });
                    }}
                  >
                    {number}
                  </button>
                ) : (
                  <span className="line-number">{number ?? ""}</span>
                )}
                <span className="line-sign">{line.sign}</span>
                {line.text}
                {"\n"}
              </span>
            );
          })}
        </pre>
      ))}
      {file.truncated && (
        <p className="secondary">
          The daemon cut these hunks at its line limit; <code>reef diff</code> shows them all.
        </p>
      )}
    </div>
  );
}

function CallSites({ file, symbol }: { file: FileDiff; symbol: string }) {
  const change = file.symbols?.changes.find((c) => expandKey(c) === symbol);
  if (!change || change.callSiteCount === 0) return null;
  return (
    <div className="call-sites">
      <p className="secondary">
        {plural(change.callSiteCount, "call site")}
        {change.callSites.length < change.callSiteCount &&
          `, the first ${String(change.callSites.length)}`}
        :
      </p>
      <ul>
        {change.callSites.map((site, i) => (
          <li key={i} className="mono">
            {site.path}:{site.range?.startLine ?? "?"}
            {site.enclosingSymbol && (
              <span className="secondary">
                {" "}
                in {site.enclosingSymbol.split(/[#./]/).filter(Boolean).at(-1)}
              </span>
            )}
          </li>
        ))}
      </ul>
    </div>
  );
}

function ExpandedView({
  expansion,
  symbol,
  onAnchor,
}: {
  expansion: Expansion;
  symbol: string | undefined;
  onAnchor: ((anchor: LineAnchor) => void) | undefined;
}) {
  if ("kind" in expansion) {
    return expansion.kind === "loading" ? (
      <p className="secondary">Reading the lines.</p>
    ) : (
      <ErrorView error={expansion.error} />
    );
  }
  if (expansion.files.length === 0) {
    return <p className="secondary">The daemon found no lines for it.</p>;
  }
  return (
    <>
      {expansion.files.map((file, i) => (
        <div key={i}>
          <Hunks file={file} onAnchor={onAnchor} />
          {symbol && <CallSites file={file} symbol={symbol} />}
        </div>
      ))}
    </>
  );
}

interface Expander {
  get: (path: string, symbol: string | undefined) => Expansion | undefined;
  isOpen: (path: string, symbol: string | undefined) => boolean;
  busy: boolean;
  toggle: (path: string, symbol: string | undefined) => void;
}

function useExpander(
  connection: Connection,
  repo: string,
  commit: string,
  onSessionInvalid: () => void,
): Expander {
  // Keyed by commit too: a commit's diff never changes, and the page moves
  // to another commit only when the person asks for the new version.
  const [expansions, setExpansions] = useState<ReadonlyMap<string, Expansion>>(new Map());
  const [open, setOpen] = useState<ReadonlySet<string>>(new Set());
  const [busy, setBusy] = useState(false);
  const id = (path: string, symbol: string | undefined) => `${commit}\n${path}\n${symbol ?? ""}`;
  function toggle(path: string, symbol: string | undefined) {
    const which = id(path, symbol);
    setOpen((before) => {
      const next = new Set(before);
      if (!next.delete(which)) next.add(which);
      return next;
    });
    const known = expansions.get(which);
    if ((known && !("kind" in known && known.kind === "failed")) || busy) return;
    const set = (expansion: Expansion) => {
      setExpansions((before) => new Map(before).set(which, expansion));
    };
    set({ kind: "loading" });
    setBusy(true);
    client(connection, DiffService)
      .getDiff({
        repo,
        toRevision: commit,
        format: DiffFormat.HUNKS,
        paths: [path],
        expandSymbols: symbol ? [symbol] : [],
        skipSnapshot: true,
      })
      .then(
        (response) => {
          set({ files: response.files.filter((file) => (file.change?.path ?? "") === path) });
        },
        (thrown: unknown) => {
          const error = reefError(thrown);
          if (isSessionInvalid(error)) onSessionInvalid();
          set({ kind: "failed", error });
        },
      )
      .finally(() => {
        setBusy(false);
      });
  }
  return {
    get: (path, symbol) => expansions.get(id(path, symbol)),
    isOpen: (path, symbol) => open.has(id(path, symbol)),
    busy,
    toggle,
  };
}

function SymbolRows({
  file,
  nodes,
  expander,
  onAnchor,
}: {
  file: DiffFile;
  nodes: SymbolNode[];
  expander: Expander;
  onAnchor: ((anchor: LineAnchor) => void) | undefined;
}) {
  return (
    <ul className="symbols">
      {nodes.map((node, i) => {
        const change = node.change;
        const symbol = expandKey(change);
        const isOpen = expander.isOpen(file.path, symbol);
        const expansion = expander.get(file.path, symbol);
        return (
          <li key={`${symbol}\n${String(i)}`}>
            <div className="symbol-row">
              <span className={`symbol-kind ${KIND_WORDS[change.kind]}`}>
                {KIND_WORDS[change.kind]}
              </span>{" "}
              <span className="secondary">{symbolKindWord(change.symbolKind)}</span>{" "}
              <span className="mono">{change.name || symbol}</span>{" "}
              <span className="secondary">
                {lineCounts(change.linesAdded, change.linesRemoved)}
              </span>
              {change.oldPath && <span className="secondary"> · from {change.oldPath}</span>}
              {!change.nestedOnly && (
                <>
                  {" "}
                  <button
                    type="button"
                    className="link-button"
                    aria-expanded={isOpen}
                    disabled={expander.busy && !expansion}
                    onClick={() => {
                      expander.toggle(file.path, symbol);
                    }}
                  >
                    {isOpen ? "Hide lines" : "Show lines"}
                  </button>
                </>
              )}
            </div>
            {isOpen && expansion && (
              <ExpandedView expansion={expansion} symbol={symbol} onAnchor={onAnchor} />
            )}
            {node.children.length > 0 && (
              <SymbolRows
                file={file}
                nodes={node.children}
                expander={expander}
                onAnchor={onAnchor}
              />
            )}
          </li>
        );
      })}
    </ul>
  );
}

function FileSection({
  file,
  expander,
  onAnchor,
}: {
  file: DiffFile;
  expander: Expander;
  onAnchor: ((anchor: LineAnchor) => void) | undefined;
}) {
  const isOpen = expander.isOpen(file.path, undefined);
  const expansion = expander.get(file.path, undefined);
  const change = file.file.change;
  return (
    <section className="diff-file" aria-label={file.path}>
      <h4 className="diff-file-head">
        <span className="mono">
          {change?.oldPath ? `${change.oldPath} → ${file.path}` : file.path}
        </span>{" "}
        <span className="secondary">
          {file.file.binary ? "binary" : lineCounts(file.file.linesAdded, file.file.linesRemoved)}
          {file.changed > 0 && ` · ${plural(file.changed, "definition")}`}
          {file.file.symbols?.parseErrors &&
            " · parsed with errors: some definitions may be missing"}
        </span>
      </h4>
      {file.symbols.length > 0 && (
        <SymbolRows file={file} nodes={file.symbols} expander={expander} onAnchor={onAnchor} />
      )}
      {file.byLines && (
        <p className="symbol-row">
          <span className="secondary">By lines: {file.byLines}.</span>{" "}
          {!file.file.binary && (
            <button
              type="button"
              className="link-button"
              aria-expanded={isOpen}
              disabled={expander.busy && !expansion}
              onClick={() => {
                expander.toggle(file.path, undefined);
              }}
            >
              {isOpen ? "Hide lines" : "Show lines"}
            </button>
          )}
        </p>
      )}
      {file.symbols.length > 0 && !file.byLines && (
        <p className="symbol-row">
          <button
            type="button"
            className="link-button"
            aria-expanded={isOpen}
            disabled={expander.busy && !expansion}
            onClick={() => {
              expander.toggle(file.path, undefined);
            }}
          >
            {isOpen ? "Hide the file's lines" : "Show the file's lines"}
          </button>
        </p>
      )}
      {isOpen && expansion && (
        <ExpandedView expansion={expansion} symbol={undefined} onAnchor={onAnchor} />
      )}
    </section>
  );
}

export function ProposalDiff({
  connection,
  repo,
  commit,
  onAnchor,
  onSessionInvalid,
}: {
  connection: Connection;
  repo: string;
  commit: string;
  /** Set when the page can comment on a line. */
  onAnchor: ((anchor: LineAnchor) => void) | undefined;
  onSessionInvalid: () => void;
}) {
  const { responses, error, more } = useSymbolDiff(connection, repo, commit, onSessionInvalid);
  const expander = useExpander(connection, repo, commit, onSessionInvalid);
  const summary = responses?.[0]?.symbolSummary;
  // Each page is ordered on its own and appended, so a later page never
  // moves rows already read.
  const files = responses ? responses.flatMap((r) => diffFiles(r.files)) : [];
  const total = responses?.[0]?.totalFiles ?? 0;
  return (
    <section className="proposal-section" aria-labelledby="proposal-diff">
      <h3 id="proposal-diff">What changed</h3>
      {error && <ErrorView error={error} />}
      {!responses && !error && <p className="secondary">Reading the diff.</p>}
      {summary && (
        <p className="secondary">
          {summary.diffTooLarge
            ? "The diff is too large for symbols: every file shows by lines."
            : `${plural(summary.added + summary.removed + summary.changed + summary.moved, "definition")} changed in ${plural(summary.filesCovered, "file")}: ${String(summary.added)} added, ${String(summary.changed)} changed, ${String(summary.removed)} removed, ${String(summary.moved)} moved.`}
          {summary.filesUnsupported + summary.filesSkipped > 0 &&
            ` ${plural(summary.filesUnsupported + summary.filesSkipped, "file")} by lines.`}
        </p>
      )}
      {responses && files.length === 0 && <p className="secondary">No files changed.</p>}
      {files.map((file) => (
        <FileSection key={file.path} file={file} expander={expander} onAnchor={onAnchor} />
      ))}
      {more && (
        <p>
          <button type="button" onClick={more}>
            Show more files ({String(total - files.length)} left)
          </button>
        </p>
      )}
    </section>
  );
}
