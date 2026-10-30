//! `reef.api.v1.DiffService`: `GetDiff` (R2.02). A page token pins the
//! operation the first page read, and its cursor is the last path
//! returned; every page streams the whole diff to count `total_files`, and
//! reads file contents only for its own files.
//!
//! With `symbols` (R2.15, ADR 0043), the page's files also carry their
//! symbol changes from `symbols::diff`: computed on the read pool
//! after the diff, then, when a change needs call sites, the new tree's
//! pass is awaited without a read permit and the lookups read again.

use std::sync::Arc;
use std::time::Instant;

use prost::Message as _;
use reef_api::v1::diff_service_server::DiffService;
use reef_api::v1::{self, GetDiffRequest, GetDiffResponse};
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::files;
use reef_lib::jj::revset::{self, RevisionError};
use tonic::{Request, Response, Status};

use super::convert::{self, known};
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{bad_token, op_id, repo_at, snapshot_first, target, uses_working_copy};
use super::symbols::{invalid_query, occurrence, range, status_message};
use crate::daemon::AppState;
use crate::error::status;
use crate::symbols::diff::{
    self as symbol_diff, CallStatus, DEFAULT_CALL_SITES, FileSymbols, MAX_CALL_SITES, MAX_EXPAND,
    SymbolChange, SymbolDiff, SymbolRequest,
};
use crate::symbols::store::Source;
use crate::symbols::{State, SymbolError};

const GET_DIFF: &str = "/reef.api.v1.DiffService/GetDiff";
/// Hunk lines per file when the request says 0.
const DEFAULT_MAX_LINES: usize = 1000;
/// Bytes of hunk lines per file at most.
const MAX_HUNK_BYTES: usize = 1 << 20;
/// A side larger than this is reported `too_large`, not read.
const MAX_FILE_BYTES: usize = 16 << 20;
/// Context lines when the request leaves them unset.
const DEFAULT_CONTEXT: u32 = 3;

/// Serves `DiffService`.
#[derive(Debug)]
pub struct DiffHandler {
    state: Arc<AppState>,
}

impl DiffHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> DiffHandler {
        DiffHandler { state }
    }
}

/// The symbol part of a request, checked: `None` without `symbols` or
/// `expand_symbols`.
fn symbol_request(
    request: &GetDiffRequest,
    format: v1::DiffFormat,
) -> Result<Option<SymbolRequest>, Status> {
    let wanted = request.symbols || !request.expand_symbols.is_empty();
    if !request.expand_symbols.is_empty() {
        if format != v1::DiffFormat::Hunks {
            return Err(invalid_query(
                "expand_symbols",
                "expand_symbols needs DIFF_FORMAT_HUNKS",
            ));
        }
        if request.expand_symbols.len() > MAX_EXPAND {
            return Err(invalid_query(
                "expand_symbols",
                format!("at most {MAX_EXPAND} entries"),
            ));
        }
        if request.expand_symbols.iter().any(|e| e.trim().is_empty()) {
            return Err(invalid_query("expand_symbols", "an entry is empty"));
        }
    }
    let source: v1::SymbolSource = known("symbol_source", request.symbol_source).map_err(status)?;
    if !wanted {
        if request.max_call_sites.is_some() {
            return Err(invalid_query(
                "max_call_sites",
                "max_call_sites needs symbols or expand_symbols",
            ));
        }
        if source != v1::SymbolSource::Unspecified {
            return Err(invalid_query(
                "symbol_source",
                "symbol_source needs symbols or expand_symbols",
            ));
        }
        return Ok(None);
    }
    Ok(Some(SymbolRequest {
        call_sites: request
            .max_call_sites
            .map_or(DEFAULT_CALL_SITES, |n| n.min(MAX_CALL_SITES))
            .try_into()
            .unwrap_or(usize::MAX),
        expand: request.expand_symbols.clone(),
        source: match source {
            v1::SymbolSource::Unspecified => None,
            v1::SymbolSource::Tags => Some(Source::Tags),
            v1::SymbolSource::Scip => Some(Source::Scip),
        },
    }))
}

fn source_of(source: Option<Source>) -> v1::SymbolSource {
    match source {
        None => v1::SymbolSource::Unspecified,
        Some(Source::Tags) => v1::SymbolSource::Tags,
        Some(Source::Scip) => v1::SymbolSource::Scip,
    }
}

fn change_message(change: &SymbolChange) -> v1::SymbolChange {
    use reef_symbols::diff::ChangeKind;
    v1::SymbolChange {
        kind: match change.kind {
            ChangeKind::Added => v1::SymbolChangeKind::Added,
            ChangeKind::Removed => v1::SymbolChangeKind::Removed,
            ChangeKind::Changed => v1::SymbolChangeKind::Changed,
            ChangeKind::Moved => v1::SymbolChangeKind::Moved,
        }
        .into(),
        symbol: change.symbol.clone(),
        old_symbol: change.old_symbol.clone().unwrap_or_default(),
        name: change.name.clone(),
        symbol_kind: i32::from(change.symbol_kind as u8),
        enclosing_symbol: change.enclosing_symbol.clone().unwrap_or_default(),
        old_path: change.old_path.clone().unwrap_or_default(),
        old_range: change.old_range.map(range),
        new_range: change.new_range.map(range),
        lines_added: change.lines_added,
        lines_removed: change.lines_removed,
        nested_only: change.nested_only,
        call_site_status: change.call_status as i32,
        call_site_count: change.call_count,
        call_sites: change.call_sites.iter().map(occurrence).collect(),
        name_definitions: change.name_definitions,
    }
}

fn file_symbols_message(file: &FileSymbols) -> v1::FileSymbolDiff {
    v1::FileSymbolDiff {
        coverage: file.coverage as i32,
        skip_reason: file.skip_reason.map_or(0, i32::from),
        source: source_of(file.source).into(),
        parse_errors: file.parse_errors,
        changes: file.changes.iter().map(change_message).collect(),
        truncated: file.truncated,
    }
}

fn summary_message(diff: &SymbolDiff) -> v1::SymbolDiffSummary {
    let s = &diff.summary;
    v1::SymbolDiffSummary {
        added: s.added,
        removed: s.removed,
        changed: s.changed,
        moved: s.moved,
        files_covered: s.files_covered,
        files_unsupported: s.files_unsupported,
        files_skipped: s.files_skipped,
        diff_too_large: s.diff_too_large,
        to_index: s.to_index.as_ref().map(status_message),
    }
}

/// Drops call sites from a file's last changes backwards until its message
/// fits a page, so no response passes tonic's 4 MiB.
fn fit(message: &mut v1::FileDiff) {
    let mut over = message.encoded_len().saturating_sub(MAX_PAGE_BYTES);
    let Some(symbols) = message.symbols.as_mut() else {
        return;
    };
    for change in symbols.changes.iter_mut().rev() {
        if over == 0 {
            break;
        }
        if change.call_sites.is_empty() {
            continue;
        }
        let before = change.encoded_len();
        change.call_sites.clear();
        change.call_site_status = v1::CallSiteStatus::PageLimit.into();
        over = over.saturating_sub(before - change.encoded_len());
    }
}

fn format_of(format: v1::DiffFormat) -> DiffFormat {
    match format {
        v1::DiffFormat::Unspecified | v1::DiffFormat::Summary => DiffFormat::Summary,
        v1::DiffFormat::Stat => DiffFormat::Stat,
        v1::DiffFormat::Hunks => DiffFormat::Hunks,
    }
}

#[tonic::async_trait]
impl DiffService for DiffHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_diff(
        &self,
        request: Request<GetDiffRequest>,
    ) -> Result<Response<GetDiffResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let page = PageRequest::new(
            GET_DIFF,
            request.page_size,
            &request.page_token,
            &GetDiffRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let to_text = if request.to_revision.trim().is_empty() {
            if target.workspace.is_none() {
                return Err(status(RevisionError::WorkspaceRequired {
                    revision: String::new(),
                }));
            }
            "@".to_owned()
        } else {
            request.to_revision.clone()
        };
        let format = known("format", request.format).map_err(status)?;
        let symbols = symbol_request(&request, format)?;
        if symbols.is_some() && !state.symbols.enabled() {
            return Err(status(SymbolError::Disabled));
        }
        let options = DiffOptions {
            format: format_of(format),
            context: usize::try_from(request.context_lines.unwrap_or(DEFAULT_CONTEXT))
                .unwrap_or(usize::MAX),
            max_lines: match request.max_lines_per_file {
                0 => DEFAULT_MAX_LINES,
                n => usize::try_from(n).unwrap_or(usize::MAX),
            },
            max_hunk_bytes: MAX_HUNK_BYTES,
            max_file_bytes: MAX_FILE_BYTES,
            paths: diff::check_paths(&request.paths).map_err(status)?,
            after: page
                .cursor()
                .map(|cursor| diff::check_paths(&[cursor.to_owned()]))
                .transpose()
                .map_err(|_| bad_token())?
                .and_then(|mut paths| paths.pop()),
            limit: page.size(),
            pairs: symbols.is_some(),
        };
        if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(
                state,
                &target,
                vec![to_text.clone(), request.from_revision.clone()],
            )
            .await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let (stop, _guard) = state.reads.stop();
        let pinned = page.pinned().map(str::to_owned);
        let read_target = target.clone();
        let symbol_options = options.clone();
        let from_text = request.from_revision.clone();
        let at_operation = request.at_operation.clone();
        let (mut found, at) = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                let repo = repo_at(
                    &reads,
                    target.repo.id,
                    &head,
                    pinned.as_deref(),
                    &at_operation,
                )
                .await?;
                let scope = target.scope();
                let to = revset::resolve_revision(&repo, &to_text, &scope)
                    .await
                    .map_err(status)?;
                let from = if from_text.trim().is_empty() {
                    DiffBase::Parents
                } else {
                    let from = revset::resolve_revision(&repo, &from_text, &scope)
                        .await
                        .map_err(status)?;
                    DiffBase::Commit(from.commit_id)
                };
                let found =
                    diff::diff_page(&repo, &from, &to.commit_id, &options, &|| stop.requested())
                        .await
                        .map_err(status)?;
                Ok::<_, Status>((found, op_id(&repo)))
            })
            .await
            .map_err(status)??;
        let symbol_diff = match &symbols {
            Some(request) => {
                Some(symbol_diff_of(state, &target, &mut found, request, symbol_options).await?)
            }
            None => None,
        };
        let mut files = Vec::with_capacity(found.files.len());
        let mut bytes = 0;
        let mut trimmed = false;
        let mut last: Option<&str> = None;
        let expanding = symbols.as_ref().is_some_and(|s| !s.expand.is_empty());
        for file in &found.files {
            let mut message = convert::file_diff(file);
            if let Some(diff) = &symbol_diff {
                let symbols = diff.files.get(&file.path);
                if expanding {
                    let Some(expanded) = symbols.and_then(|s| s.hunks.as_ref()) else {
                        last = Some(&file.path);
                        continue;
                    };
                    message.hunks = expanded.hunks.iter().map(convert::hunk).collect();
                    message.truncated = expanded.truncated;
                    message.old_no_final_newline = expanded.old_no_final_newline;
                    message.new_no_final_newline = expanded.new_no_final_newline;
                }
                message.symbols = symbols.map(file_symbols_message);
                fit(&mut message);
            }
            let size = message.encoded_len();
            if !files.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            files.push(message);
            last = Some(&file.path);
        }
        let next_page_token = match last {
            Some(last) if found.more || trimmed => page.token_at(last, &at),
            _ => String::new(),
        };
        Ok(Response::new(GetDiffResponse {
            files,
            next_page_token,
            from_commit_ids: found.from.iter().map(ToString::to_string).collect(),
            to_commit_id: found.to.to_string(),
            at_operation: at,
            total_files: u32::try_from(found.total).unwrap_or(u32::MAX),
            symbol_summary: symbol_diff.as_ref().map(summary_message),
        }))
    }
}

/// The symbol diff of `found`'s page: computed on the read pool, then call
/// sites once the new tree is indexed, waiting for its pass without a read
/// permit. A failure other than the index being disabled leaves the page's
/// files skipped, not the diff failed.
async fn symbol_diff_of(
    state: &Arc<AppState>,
    target: &super::reading::Target,
    found: &mut diff::DiffPage,
    request: &SymbolRequest,
    options: DiffOptions,
) -> Result<SymbolDiff, Status> {
    let index = Arc::clone(&state.symbols);
    let repo = target.repo.id;
    let store = index.store(repo).map_err(status)?;
    let page: Vec<String> = found.files.iter().map(|f| f.path.clone()).collect();
    let pairs = found.pairs.take();
    let to = found.to.clone();
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let computed = {
        let (index, store, head, to, page, request) = (
            Arc::clone(&index),
            Arc::clone(&store),
            Arc::clone(&head),
            to.clone(),
            page.clone(),
            request.clone(),
        );
        state
            .reads
            .run(&target.handle, move || async move {
                let tree = files::commit_tree(&head, &to).await?;
                symbol_diff::compute(
                    &index,
                    &store,
                    &tree,
                    pairs.as_deref(),
                    &page,
                    &request,
                    &options,
                )
            })
            .await
            .map_err(status)?
    };
    let mut computed = match computed {
        Ok(computed) => computed,
        Err(SymbolError::Disabled) => return Err(status(SymbolError::Disabled)),
        Err(error) => {
            tracing::warn!(repo = %repo, commit = %to, error = %error, "symbol diff failed");
            return Ok(symbol_diff::skipped(&page));
        }
    };
    if !computed.needs_lookups() {
        return Ok(computed);
    }
    let wait = index.call_site_wait();
    match index.ensure(state, repo, to.clone(), wait).await {
        Ok(tree_status) if tree_status.state == State::Complete => {
            computed.summary.to_index = Some(tree_status);
            let request = request.clone();
            let (mut computed, looked_up) = state
                .reads
                .run(&target.handle, move || async move {
                    let looked_up = match files::commit_tree(&head, &to).await {
                        Ok(tree) => {
                            symbol_diff::call_sites(&index, &store, &tree, &mut computed, &request)
                        }
                        Err(error) => Err(error.into()),
                    };
                    (computed, looked_up)
                })
                .await
                .map_err(status)?;
            if let Err(error) = looked_up {
                // The changes stand; only their call sites are missing.
                tracing::warn!(repo = %repo, error = %error, "call-site lookup failed");
                computed.mark_lookups(CallStatus::IndexIncomplete);
            }
            Ok(computed)
        }
        Ok(tree_status) => {
            computed.summary.to_index = Some(tree_status);
            computed.mark_lookups(CallStatus::IndexIncomplete);
            Ok(computed)
        }
        Err(error) => {
            tracing::warn!(repo = %repo, error = %error, "indexing for call sites failed");
            computed.mark_lookups(CallStatus::IndexIncomplete);
            Ok(computed)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file whose call sites would pass a page loses them from its last
    /// changes backwards, and fits (R2.15, test 12's bound).
    #[test]
    fn fit_drops_call_sites_from_the_end() {
        let site = v1::SymbolOccurrence {
            path: "x".repeat(200),
            ..v1::SymbolOccurrence::default()
        };
        let change = v1::SymbolChange {
            call_site_status: v1::CallSiteStatus::Found.into(),
            call_sites: vec![site; 100],
            ..v1::SymbolChange::default()
        };
        let mut message = v1::FileDiff {
            symbols: Some(v1::FileSymbolDiff {
                changes: vec![change; 1000],
                ..v1::FileSymbolDiff::default()
            }),
            ..v1::FileDiff::default()
        };
        assert!(message.encoded_len() > MAX_PAGE_BYTES);
        fit(&mut message);
        assert!(message.encoded_len() <= MAX_PAGE_BYTES);
        let changes = &message.symbols.as_ref().unwrap().changes;
        assert_eq!(changes[0].call_sites.len(), 100);
        assert_eq!(changes[0].call_site_status(), v1::CallSiteStatus::Found);
        assert_eq!(
            changes[999].call_site_status(),
            v1::CallSiteStatus::PageLimit
        );
        assert!(changes[999].call_sites.is_empty());
    }
}
