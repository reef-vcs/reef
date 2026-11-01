// `/repos/<repo>/smartlog` (R2.19): its parameters and links, apart from
// Smartlog.tsx so the nav and other pages link here without importing it.
import { DEFAULT_LIMIT, MAX_LIMIT, SMARTLOG_REVSET } from "../graph/revset";
import { hexFromEither, reverseHex } from "../ui/ids";
import { href } from "../ui/router";

export interface SmartlogParams {
  repo: string;
  revset: string;
  limit: number;
  /** Hex, or empty. */
  change: string;
  at: string;
}

/** The route's parameters from its query string, with defaults. */
export function smartlogParams(repo: string, search: string): SmartlogParams {
  const query = new URLSearchParams(search);
  const limit = Number.parseInt(query.get("limit") ?? "", 10);
  return {
    repo,
    revset: query.get("revset") || SMARTLOG_REVSET,
    limit: Number.isFinite(limit) && limit > 0 ? Math.min(limit, MAX_LIMIT) : DEFAULT_LIMIT,
    change: hexFromEither(query.get("change") ?? "") ?? "",
    at: query.get("at") ?? "",
  };
}

export function smartlogHref(params: Partial<SmartlogParams> & { repo: string }): string {
  return href(`/repos/${encodeURIComponent(params.repo)}/smartlog`, {
    revset: params.revset === SMARTLOG_REVSET ? undefined : params.revset,
    limit: params.limit && params.limit !== DEFAULT_LIMIT ? String(params.limit) : undefined,
    change: params.change ? reverseHex(params.change) : undefined,
    at: params.at,
  });
}
