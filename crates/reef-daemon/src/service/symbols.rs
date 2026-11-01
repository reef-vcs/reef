//! `reef.api.v1.SymbolService` (R2.14, ADR 0042): `IndexSymbols`,
//! `UploadScipDocuments` and `ListSymbols` over [`crate::symbols`].
//!
//! Revisions resolve as every read's do, snapshotting a workspace first
//! when they use `@`. Passes and uploads run on the blocking pool;
//! listings on the read pool, as `ListFiles` does. A listing's page token
//! pins the operation the first page read, and its cursor is the last
//! occurrence returned: its sequence number and path.

use std::sync::Arc;
use std::time::{Duration, Instant};

use prost::Message as _;
use reef_api::v1::symbol_service_server::SymbolService;
use reef_api::v1::{
    self, IndexSymbolsRequest, IndexSymbolsResponse, ListSymbolsRequest, ListSymbolsResponse,
    ScipDocumentRejection, SymbolIndexStatus, SymbolLanguageCount, SymbolSkipCount,
    UploadScipDocumentsRequest, UploadScipDocumentsResponse,
};
use reef_lib::jj::CommitIdHex;
use reef_lib::jj::files;
use reef_lib::jj::revset::{self, RevisionError};
use reef_symbols::{Kind, Role};
use tonic::{Request, Response, Status};

use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{
    Target, bad_token, op_id, repo_at, snapshot_first, target, uses_working_copy,
};
use crate::daemon::AppState;
use crate::error::status;
use crate::model::Timestamp;
use crate::symbols::query::{self, Filters, Listed, Lookup};
use crate::symbols::store::Source;
use crate::symbols::upload::{self, MAX_DOCUMENTS};
use crate::symbols::{IndexStatus, MAX_WAIT, State, SymbolError};

const LIST_SYMBOLS: &str = "/reef.api.v1.SymbolService/ListSymbols";

/// How long `IndexSymbols` waits when the request does not say.
const DEFAULT_WAIT: Duration = Duration::from_secs(10);

/// Serves `SymbolService`.
#[derive(Debug)]
pub struct SymbolsHandler {
    state: Arc<AppState>,
}

impl SymbolsHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> SymbolsHandler {
        SymbolsHandler { state }
    }

    fn enabled(&self) -> Result<(), Status> {
        if self.state.symbols.enabled() {
            Ok(())
        } else {
            Err(status(SymbolError::Disabled))
        }
    }
}

/// `status` as the API reports it.
pub(crate) fn status_message(status: &IndexStatus) -> SymbolIndexStatus {
    let counts = &status.counts;
    SymbolIndexStatus {
        commit_id: status.commit_id.clone(),
        state: match status.state {
            State::NotIndexed => v1::SymbolIndexState::NotIndexed,
            State::Indexing => v1::SymbolIndexState::Indexing,
            State::Complete => v1::SymbolIndexState::Complete,
        }
        .into(),
        files: counts.files,
        files_indexed: counts.indexed,
        files_skipped: counts.skipped_total(),
        skipped: counts
            .skipped
            .iter()
            .map(|(reason, files)| SymbolSkipCount {
                reason: i32::from(*reason),
                files: *files,
            })
            .collect(),
        scip_files: counts.scip_files,
        languages: counts
            .languages
            .iter()
            .map(|(language, files)| SymbolLanguageCount {
                language: language.clone(),
                files: *files,
            })
            .collect(),
        indexed_at: status
            .indexed_at
            .map(|ms| super::convert::timestamp(Timestamp(ms))),
    }
}

pub(super) fn range(r: reef_symbols::Range) -> v1::SourceRange {
    v1::SourceRange {
        start_line: r.start_line,
        start_column: r.start_col,
        end_line: r.end_line,
        end_column: r.end_col,
    }
}

pub(super) fn occurrence(listed: &Listed) -> v1::SymbolOccurrence {
    let o = &listed.occurrence;
    let flag = |bit| o.flags & bit != 0;
    v1::SymbolOccurrence {
        path: listed.path.clone(),
        symbol: o.symbol.clone().unwrap_or_default(),
        name: o.name.clone(),
        kind: i32::from(o.kind as u8),
        role: match o.role {
            Role::Definition => v1::SymbolRole::Definition,
            Role::Reference => v1::SymbolRole::Reference,
        }
        .into(),
        range: Some(range(o.range)),
        definition_range: o.definition_range.map(range),
        enclosing_symbol: listed.enclosing_symbol.clone().unwrap_or_default(),
        source: match listed.source {
            Source::Tags => v1::SymbolSource::Tags,
            Source::Scip => v1::SymbolSource::Scip,
        }
        .into(),
        language: listed.language.clone(),
        is_import: flag(reef_symbols::flags::IMPORT),
        is_write: flag(reef_symbols::flags::WRITE),
        is_test: flag(reef_symbols::flags::TEST),
        is_generated: flag(reef_symbols::flags::GENERATED),
    }
}

pub(super) fn invalid_query(field: &'static str, message: impl Into<String>) -> Status {
    status(SymbolError::InvalidQuery {
        field,
        message: message.into(),
    })
}

/// The page cursor `seq:path` of the last occurrence returned.
fn cursor_of(listed: &Listed) -> String {
    format!("{}:{}", listed.seq, listed.path)
}

fn parse_cursor(cursor: &str) -> Option<(String, u32)> {
    let (seq, path) = cursor.split_once(':')?;
    Some((path.to_owned(), seq.parse().ok()?))
}

/// The filters of a listing request.
fn filters(request: &ListSymbolsRequest) -> Result<Filters, Status> {
    let roles = request
        .roles
        .iter()
        .map(|&role| {
            u8::try_from(role)
                .ok()
                .and_then(Role::from_u8)
                .ok_or_else(|| invalid_query("roles", format!("{role} is not a symbol role")))
        })
        .collect::<Result<_, _>>()?;
    let kinds = request
        .kinds
        .iter()
        .map(|&kind| {
            u8::try_from(kind)
                .ok()
                .and_then(Kind::from_u8)
                .filter(|k| *k != Kind::Unspecified)
                .ok_or_else(|| invalid_query("kinds", format!("{kind} is not a symbol kind")))
        })
        .collect::<Result<_, _>>()?;
    let source = match v1::SymbolSource::try_from(request.source) {
        Ok(v1::SymbolSource::Tags) => Some(Source::Tags),
        Ok(v1::SymbolSource::Scip) => Some(Source::Scip),
        Ok(v1::SymbolSource::Unspecified) => None,
        Err(_) => return Err(invalid_query("source", "not a symbol source")),
    };
    Ok(Filters {
        roles,
        kinds,
        source,
    })
}

/// Snapshots the target's workspace first when `revision` needs it, and
/// resolves `revision` at the head (or `at_operation`, or the pinned
/// operation): the commit and the operation read.
async fn resolve(
    state: &AppState,
    target: &Target,
    revision: &str,
    at_operation: &str,
    pinned: Option<String>,
    skip_snapshot: bool,
    agent: Option<String>,
) -> Result<(CommitIdHex, String), Status> {
    if revision.trim().is_empty() {
        return Err(status(RevisionError::Required));
    }
    if pinned.is_none() && at_operation.is_empty() && !skip_snapshot {
        let needed = uses_working_copy(state, target, vec![revision.to_owned()]).await?;
        snapshot_first(state, target, needed, Instant::now(), agent).await?;
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let reads = Arc::clone(&state.reads);
    let read_target = target.clone();
    let revision = revision.to_owned();
    let at_operation = at_operation.to_owned();
    state
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
            let resolved = revset::resolve_revision(&repo, &revision, &target.scope())
                .await
                .map_err(status)?;
            Ok::<_, Status>((resolved.commit_id, op_id(&repo)))
        })
        .await
        .map_err(status)?
}

#[tonic::async_trait]
impl SymbolService for SymbolsHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn index_symbols(
        &self,
        request: Request<IndexSymbolsRequest>,
    ) -> Result<Response<IndexSymbolsResponse>, Status> {
        self.enabled()?;
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let (commit, at) = resolve(
            state,
            &target,
            &request.revision,
            &request.at_operation,
            None,
            request.skip_snapshot,
            agent,
        )
        .await?;
        let wait = match request.wait {
            None => DEFAULT_WAIT,
            Some(wait) => Duration::try_from(wait).unwrap_or_default().min(MAX_WAIT),
        };
        let index = Arc::clone(&state.symbols);
        let found = index
            .ensure(state, target.repo.id, commit, wait)
            .await
            .map_err(status)?;
        Ok(Response::new(IndexSymbolsResponse {
            status: Some(status_message(&found)),
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn upload_scip_documents(
        &self,
        request: Request<UploadScipDocumentsRequest>,
    ) -> Result<Response<UploadScipDocumentsResponse>, Status> {
        self.enabled()?;
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        if request.documents.is_empty() || request.documents.len() > MAX_DOCUMENTS {
            return Err(status(SymbolError::InvalidUpload {
                field: "documents",
                message: format!(
                    "{} documents; send 1 to {MAX_DOCUMENTS}",
                    request.documents.len()
                ),
            }));
        }
        let tool = request.tool.clone().unwrap_or_default();
        if tool.name.trim().is_empty() {
            return Err(status(SymbolError::InvalidUpload {
                field: "tool",
                message: "the indexer's name is required".into(),
            }));
        }
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let (commit, _) = resolve(
            state,
            &target,
            &request.revision,
            "",
            None,
            request.skip_snapshot,
            agent,
        )
        .await?;
        let index = Arc::clone(&state.symbols);
        let store = index.store(target.repo.id).map_err(status)?;
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let documents: Vec<upload::Document> = request
            .documents
            .into_iter()
            .map(|d| upload::Document {
                path: d.path,
                content_digest: d.content_digest,
                bytes: d.document,
            })
            .collect();
        let repo_id = target.repo.id;
        let at = commit.clone();
        let (outcome, found) = tokio::task::spawn_blocking(move || {
            let tree = pollster::block_on(files::commit_tree(&head, &at))?;
            let key = files::tree_key(&tree);
            let outcome = upload::run(
                &index,
                &store,
                &tree,
                (&tool.name, &tool.version),
                documents,
            )?;
            store.add_scip_files(&key, outcome.newly_covered)?;
            let found = index.status_of(&store, repo_id, &key, at.as_str())?;
            Ok::<_, SymbolError>((outcome, found))
        })
        .await
        .map_err(|error| status(SymbolError::Pass(error.to_string())))?
        .map_err(status)?;
        Ok(Response::new(UploadScipDocumentsResponse {
            commit_id: commit.to_string(),
            accepted: outcome.accepted,
            rejected: outcome
                .rejected
                .into_iter()
                .map(|r| ScipDocumentRejection {
                    path: r.path,
                    reason: r.reason as i32,
                    message: r.message,
                })
                .collect(),
            status: Some(status_message(&found)),
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_symbols(
        &self,
        request: Request<ListSymbolsRequest>,
    ) -> Result<Response<ListSymbolsResponse>, Status> {
        self.enabled()?;
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_SYMBOLS,
            request.page_size,
            &request.page_token,
            &ListSymbolsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let lookup = match (request.name.as_str(), request.symbol.as_str()) {
            ("", "") if request.path.is_empty() => {
                return Err(invalid_query("path", "set path, name or symbol"));
            }
            ("", "") => None,
            (_, "") => Some(Lookup::Name(&request.name)),
            ("", symbol) => {
                if let Err(error) = reef_symbols::symbol::parse(symbol) {
                    return Err(invalid_query("symbol", error.to_string()));
                }
                Some(Lookup::Symbol(symbol))
            }
            _ => return Err(invalid_query("symbol", "set name or symbol, not both")),
        };
        let lookup = lookup.map(|l| match l {
            Lookup::Name(n) => (true, n.to_owned()),
            Lookup::Symbol(s) => (false, s.to_owned()),
        });
        if !request.path.is_empty() {
            files::check_path(&request.path, false).map_err(status)?;
        }
        let filters = filters(&request)?;
        let after = page
            .cursor()
            .map(|c| parse_cursor(c).ok_or_else(bad_token))
            .transpose()?;
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let (commit, at) = resolve(
            state,
            &target,
            &request.revision,
            &request.at_operation,
            page.pinned().map(str::to_owned),
            request.skip_snapshot,
            agent,
        )
        .await?;
        let index = Arc::clone(&state.symbols);
        let store = index.store(target.repo.id).map_err(status)?;
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let repo_id = target.repo.id;
        let limit = page.size();
        let dir = request.path.clone();
        let read_commit = commit.clone();
        let (found, tree_status) = state
            .reads
            .run(&target.handle, move || async move {
                let tree = files::commit_tree(&head, &read_commit).await?;
                let key = files::tree_key(&tree);
                let tree_status = index.status_of(&store, repo_id, &key, read_commit.as_str())?;
                let after = after.as_ref().map(|(path, seq)| (path.as_str(), *seq));
                let found = match &lookup {
                    None => query::by_path(
                        &index,
                        &store,
                        &tree,
                        read_commit.as_str(),
                        &dir,
                        &filters,
                        after,
                        limit,
                    )?,
                    Some((is_name, text)) => query::by_lookup(
                        &store,
                        &tree,
                        &tree_status,
                        if *is_name {
                            Lookup::Name(text)
                        } else {
                            Lookup::Symbol(text)
                        },
                        &dir,
                        &filters,
                        after,
                        limit,
                    )?,
                };
                Ok::<_, SymbolError>((found, tree_status))
            })
            .await
            .map_err(status)?
            .map_err(status)?;
        let mut symbols = Vec::with_capacity(found.items.len());
        let mut bytes = 0;
        let mut trimmed = false;
        let mut last = None;
        for listed in &found.items {
            let message = occurrence(listed);
            let size = message.encoded_len();
            if !symbols.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            symbols.push(message);
            last = Some(listed);
        }
        let next_page_token = match last {
            Some(last) if found.more || trimmed => page.token_at(&cursor_of(last), &at),
            _ => String::new(),
        };
        Ok(Response::new(ListSymbolsResponse {
            symbols,
            next_page_token,
            commit_id: commit.to_string(),
            at_operation: at,
            status: Some(status_message(&tree_status)),
        }))
    }
}
