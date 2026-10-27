//! `reef.api.v1.FileService` (R2.04, ADR 0032): `ReadFile` and `ListFiles`
//! on the read pool as R2.02's reads are, and `EditFiles` through
//! [`crate::workspace::Manager::rewrite`] as R2.03's writes are.
//!
//! A listing's page token pins the operation the first page read, and its
//! cursor is the last path returned.

use std::sync::Arc;
use std::time::Instant;

use prost::Message as _;
use reef_api::v1::file_service_server::FileService;
use reef_api::v1::{
    self, EditFilesRequest, EditFilesResponse, FileEntry, ListFilesRequest, ListFilesResponse,
    OperationKind, ReadFileRequest, ReadFileResponse, file_edit,
};
use reef_lib::jj::files::{self, Edit, EditAction, FileEntryInfo, ListOptions};
use reef_lib::jj::revset::{self, RevisionError};
use tonic::{Request, Response, Status};

use super::changes::write_change;
use super::convert;
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{bad_token, op_id, repo_at, snapshot_first, target, uses_working_copy};
use crate::daemon::AppState;
use crate::error::status;
use crate::workspace::{EditTarget, Write};

const LIST_FILES: &str = "/reef.api.v1.FileService/ListFiles";

/// Serves `FileService`.
#[derive(Debug)]
pub struct FilesHandler {
    state: Arc<AppState>,
}

impl FilesHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> FilesHandler {
        FilesHandler { state }
    }
}

fn entry(info: &FileEntryInfo) -> FileEntry {
    FileEntry {
        path: info.path.clone(),
        r#type: convert::entry_type(Some(info.kind)).into(),
        file_id: info.id.clone().unwrap_or_default(),
    }
}

/// A request's edit as [`files::check_edits`] takes it.
fn edit_of(edit: v1::FileEdit) -> Edit {
    let action = edit.action.map(|action| match action {
        file_edit::Action::Content(bytes) => EditAction::Content(bytes),
        file_edit::Action::Replace(replace) => EditAction::Replace {
            old: replace.old_text,
            new: replace.new_text,
            all: replace.all,
        },
        file_edit::Action::Delete(_) => EditAction::Delete,
        file_edit::Action::CopyFrom(path) => EditAction::CopyFrom(path),
    });
    Edit {
        path: edit.path,
        action,
        executable: edit.executable,
    }
}

#[tonic::async_trait]
impl FileService for FilesHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn read_file(
        &self,
        request: Request<ReadFileRequest>,
    ) -> Result<Response<ReadFileResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        if request.revision.trim().is_empty() {
            return Err(status(RevisionError::Required));
        }
        let path = files::check_path(&request.path, false).map_err(status)?;
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        if request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(state, &target, vec![request.revision.clone()]).await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let read_target = target.clone();
        let (read, commit_id, at) = state
            .reads
            .run(&target.handle, move || async move {
                let target = read_target;
                let repo =
                    repo_at(&reads, target.repo.id, &head, None, &request.at_operation).await?;
                let resolved = revset::resolve_revision(&repo, &request.revision, &target.scope())
                    .await
                    .map_err(status)?;
                let read = files::read_file(
                    &repo,
                    &resolved.commit_id,
                    &path,
                    request.offset,
                    request.limit,
                )
                .await
                .map_err(status)?;
                Ok::<_, Status>((read, resolved.commit_id, op_id(&repo)))
            })
            .await
            .map_err(status)??;
        Ok(Response::new(ReadFileResponse {
            entry: Some(entry(&read.entry)),
            content: read.content,
            size: read.size,
            commit_id: commit_id.to_string(),
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_files(
        &self,
        request: Request<ListFilesRequest>,
    ) -> Result<Response<ListFilesResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_FILES,
            request.page_size,
            &request.page_token,
            &ListFilesRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        if request.revision.trim().is_empty() {
            return Err(status(RevisionError::Required));
        }
        let dir = if request.path.is_empty() {
            None
        } else {
            Some(files::check_path(&request.path, false).map_err(status)?)
        };
        let after = page
            .cursor()
            .map(|cursor| files::check_path(cursor, false))
            .transpose()
            .map_err(|_| bad_token())?;
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(state, &target, vec![request.revision.clone()]).await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let options = ListOptions {
            dir,
            recursive: request.recursive,
            after,
            limit: page.size(),
        };
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let (stop, _guard) = state.reads.stop();
        let pinned = page.pinned().map(str::to_owned);
        let read_target = target.clone();
        let revision = request.revision.clone();
        let at_operation = request.at_operation.clone();
        let (found, commit_id, at) = state
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
                let found =
                    files::list_files(&repo, &resolved.commit_id, &options, &|| stop.requested())
                        .await
                        .map_err(status)?;
                Ok::<_, Status>((found, resolved.commit_id, op_id(&repo)))
            })
            .await
            .map_err(status)??;
        let mut entries = Vec::with_capacity(found.entries.len());
        let mut bytes = 0;
        let mut trimmed = false;
        for info in &found.entries {
            let message = entry(info);
            let size = message.encoded_len();
            if !entries.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            entries.push(message);
        }
        let next_page_token = match entries.last() {
            Some(last) if found.more || trimmed => page.token_at(&last.path, &at),
            _ => String::new(),
        };
        Ok(Response::new(ListFilesResponse {
            entries,
            next_page_token,
            commit_id: commit_id.to_string(),
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn edit_files(
        &self,
        request: Request<EditFilesRequest>,
    ) -> Result<Response<EditFilesResponse>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        let target = target(&self.state, &request.repo, &request.workspace)?;
        let edit_target = match request.target {
            Some(v1::edit_files_request::Target::NewOn(revision)) => EditTarget::NewOn(revision),
            Some(v1::edit_files_request::Target::Revision(revision)) => {
                EditTarget::Rewrite(revision)
            }
            None => EditTarget::Rewrite(String::new()),
        };
        let write = Write::Edit {
            target: edit_target,
            expected: request.expected_commit_id,
            edits: request.edits.into_iter().map(edit_of).collect(),
            description: request.description,
        };
        let (rewritten, operation) =
            write_change(&self.state, &target, agent, write, OperationKind::Edit).await?;
        Ok(Response::new(EditFilesResponse {
            change: rewritten.change.as_ref().map(convert::change),
            operation,
        }))
    }
}
