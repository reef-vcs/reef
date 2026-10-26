//! `reef.api.v1.DiffService`: `GetDiff` (R2.02). A page token pins the
//! operation the first page read, and its cursor is the last path
//! returned; every page streams the whole diff to count `total_files`, and
//! reads file contents only for its own files.

use std::sync::Arc;
use std::time::Instant;

use prost::Message as _;
use reef_api::v1::diff_service_server::DiffService;
use reef_api::v1::{self, GetDiffRequest, GetDiffResponse};
use reef_lib::jj::diff::{self, DiffBase, DiffFormat, DiffOptions};
use reef_lib::jj::revset::{self, RevisionError};
use tonic::{Request, Response, Status};

use super::convert::{self, known};
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{bad_token, op_id, repo_at, snapshot_first, target, uses_working_copy};
use crate::daemon::AppState;
use crate::error::status;

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
        let options = DiffOptions {
            format: format_of(known("format", request.format).map_err(status)?),
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
        };
        if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(
                state,
                &target,
                vec![to_text.clone(), request.from_revision.clone()],
            )
            .await?;
            snapshot_first(state, &target, needed, arrived).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let (stop, _guard) = state.reads.stop();
        let pinned = page.pinned().map(str::to_owned);
        let read_target = target.clone();
        let from_text = request.from_revision.clone();
        let at_operation = request.at_operation.clone();
        let (found, at) = state
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
        let mut files = Vec::with_capacity(found.files.len());
        let mut bytes = 0;
        let mut trimmed = false;
        for file in &found.files {
            let message = convert::file_diff(file);
            let size = message.encoded_len();
            if !files.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            files.push(message);
        }
        let next_page_token = match files.last().and_then(|file| file.change.as_ref()) {
            Some(last) if found.more || trimmed => page.token_at(&last.path, &at),
            _ => String::new(),
        };
        Ok(Response::new(GetDiffResponse {
            files,
            next_page_token,
            from_commit_ids: found.from.iter().map(ToString::to_string).collect(),
            to_commit_id: found.to.to_string(),
            at_operation: at,
            total_files: u32::try_from(found.total).unwrap_or(u32::MAX),
        }))
    }
}
