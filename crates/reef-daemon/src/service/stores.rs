//! `reef.api.v1.StoreService`: build and list shared dependency stores
//! (R1.20).

use std::collections::{HashMap, HashSet};
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use futures::Stream;
use reef_api::v1::store_service_server::StoreService;
use reef_api::v1::{
    self, BuildStoreRequest, BuildStoreResponse, ListStoresRequest, ListStoresResponse,
    OperationKind, StoreInfo, build_store_response, output_line,
};
use reef_lib::error::{Classify, ErrorKind};
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::Instrument as _;

use super::convert::{Written, timestamp};
use super::page::PageRequest;
use crate::daemon::AppState;
use crate::error::status;
use crate::model::{RepoId, WorkspaceState};
use crate::store::build::{
    self, BuildOutput, BuildRequest, BuildResult, BuildStarted, OutputLine, OutputStream,
};
use crate::store::{StoreError, StoreKey};

/// How many events a build may queue for a slow client before its output
/// lines are dropped (and counted).
const EVENT_BUFFER: usize = 1024;

/// A request's timeout that is not a duration.
#[derive(Debug, thiserror::Error)]
#[error("timeout must be a positive duration")]
pub struct InvalidTimeout;

impl Classify for InvalidTimeout {
    fn kind(&self) -> ErrorKind {
        ErrorKind::InvalidArgument
    }
}

/// Serves `StoreService`.
#[derive(Debug)]
pub struct StoresService {
    state: Arc<AppState>,
}

impl StoresService {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> StoresService {
        StoresService { state }
    }
}

impl From<&OutputLine> for v1::OutputLine {
    fn from(line: &OutputLine) -> Self {
        let stream = match line.stream {
            OutputStream::Stdout => output_line::Stream::Stdout,
            OutputStream::Stderr => output_line::Stream::Stderr,
        };
        Self {
            text: line.text.clone(),
            stream: stream.into(),
        }
    }
}

impl From<&BuildStarted> for v1::BuildStarted {
    fn from(started: &BuildStarted) -> Self {
        Self {
            argv: started.argv.clone(),
            env: started
                .env
                .iter()
                .map(|(key, value)| format!("{key}={value}"))
                .collect(),
            commit_id: started.rev.clone(),
        }
    }
}

impl From<&BuildOutput> for build_store_response::Event {
    fn from(output: &BuildOutput) -> Self {
        match output {
            BuildOutput::Started(started) => Self::Started(v1::BuildStarted::from(started)),
            BuildOutput::Line(line) => Self::Line(v1::OutputLine::from(line)),
        }
    }
}

/// A build's result as a message, with its release operation read back.
async fn build_result(
    state: &AppState,
    repo: RepoId,
    result: &BuildResult,
) -> Result<v1::BuildResult, Status> {
    let release_operation = super::written_operation(
        state,
        repo,
        result.op_id.as_deref(),
        Written {
            kind: OperationKind::ReleaseWorkspace,
            snapshot: None,
            workspace: None,
            agent: build::BUILD_OWNER,
        },
    )
    .await?;
    Ok(v1::BuildResult {
        name: result.key.name.clone(),
        lockfile_hash: result.key.lockfile_hash.clone(),
        path: result.path.display().to_string(),
        bytes: result.bytes,
        sealed: result.sealed,
        exit_code: result.exit_code,
        release_operation,
        dropped_lines: result.dropped_lines,
        already_built: result.already_built,
    })
}

/// The request's timeout, or [`build::DEFAULT_TIMEOUT`].
fn timeout(request: Option<&prost_types::Duration>) -> Result<Duration, InvalidTimeout> {
    let Some(duration) = request else {
        return Ok(build::DEFAULT_TIMEOUT);
    };
    let secs = u64::try_from(duration.seconds).map_err(|_| InvalidTimeout)?;
    let nanos = u32::try_from(duration.nanos).map_err(|_| InvalidTimeout)?;
    let timeout = Duration::new(secs, nanos);
    if timeout.is_zero() {
        return Err(InvalidTimeout);
    }
    Ok(timeout)
}

type BuildStream = Pin<Box<dyn Stream<Item = Result<BuildStoreResponse, Status>> + Send>>;

#[tonic::async_trait]
impl StoreService for StoresService {
    type BuildStoreStream = BuildStream;

    #[tracing::instrument(skip_all, fields(repo = %request.get_ref().repo, store = %request.get_ref().name))]
    async fn build_store(
        &self,
        request: Request<BuildStoreRequest>,
    ) -> Result<Response<Self::BuildStoreStream>, Status> {
        let agent = super::agent_of(request.metadata())?;
        let request = request.into_inner();
        if !self.state.config.file.store.allow_builds {
            return Err(status(StoreError::BuildsOff));
        }
        let manager = Arc::clone(&self.state.manager);
        let repo = manager.resolve_repo(&request.repo).map_err(status)?;
        let build = BuildRequest {
            repo: repo.id,
            name: request.name,
            rev: Some(request.revision).filter(|rev| !rev.trim().is_empty()),
            timeout: timeout(request.timeout.as_ref()).map_err(status)?,
            force: request.force,
            agent,
        };
        let (events, stream) = mpsc::channel(EVENT_BUFFER);
        let (lines, mut output) = mpsc::channel(EVENT_BUFFER);
        let shutdown = self.state.shutdown.clone();
        let state = Arc::clone(&self.state);
        let repo_id = repo.id;
        // Its own task, so a client that disconnects does not cut the build
        // between its command and its seal or release.
        let job = tokio::spawn(
            build::run(manager, build, lines, shutdown).instrument(tracing::Span::current()),
        );
        tokio::spawn(async move {
            // Ends once the build has dropped every sender: after its last
            // line. A line the client has no room for is dropped and
            // counted; once the client is gone the rest are drained, and
            // the build goes on.
            let mut dropped = 0u64;
            while let Some(item) = output.recv().await {
                let event = BuildStoreResponse {
                    event: Some(build_store_response::Event::from(&item)),
                };
                match events.try_send(Ok(event)) {
                    Ok(()) => {}
                    Err(mpsc::error::TrySendError::Full(_)) => dropped += 1,
                    Err(mpsc::error::TrySendError::Closed(_)) => {
                        while output.recv().await.is_some() {}
                        break;
                    }
                }
            }
            let last = match job.await {
                Ok(Ok(mut result)) => {
                    result.dropped_lines = dropped;
                    build_result(&state, repo_id, &result)
                        .await
                        .map(|result| BuildStoreResponse {
                            event: Some(build_store_response::Event::Result(result)),
                        })
                }
                Ok(Err(error)) => Err(status(error)),
                Err(error) => Err(Status::internal(format!(
                    "the store build task failed: {error}"
                ))),
            };
            let _ = events.send(last).await;
        });
        Ok(Response::new(Box::pin(ReceiverStream::new(stream))))
    }

    #[tracing::instrument(skip_all, fields(repo = %request.get_ref().repo))]
    async fn list_stores(
        &self,
        request: Request<ListStoresRequest>,
    ) -> Result<Response<ListStoresResponse>, Status> {
        let request = request.into_inner();
        let page = PageRequest::new(
            "/reef.api.v1.StoreService/ListStores",
            request.page_size,
            &request.page_token,
            &ListStoresRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let manager = &self.state.manager;
        let in_use = manager.stores_in_use();
        let rows = self.state.db.stores().map_err(status)?;
        let shown: Option<HashSet<StoreKey>> = if request.repo.trim().is_empty() {
            None
        } else {
            let repo = manager.resolve_repo(&request.repo).map_err(status)?.id;
            let mut keys: HashSet<StoreKey> = rows
                .iter()
                .filter(|row| row.repo_id == repo)
                .map(|row| key_of(&row.name, &row.lockfile_hash))
                .collect();
            for entry in manager
                .list(Some(repo), &[WorkspaceState::Active], None)
                .map_err(status)?
                .into_iter()
                .filter_map(|(record, _)| manager.entry(record.id))
            {
                keys.extend(entry.stores().into_iter().map(|store| store.key));
            }
            Some(keys)
        };
        let rows: Vec<_> = rows
            .into_iter()
            .filter(|row| row.sealed)
            .filter(|row| {
                shown
                    .as_ref()
                    .is_none_or(|keys| keys.contains(&key_of(&row.name, &row.lockfile_hash)))
            })
            .collect();
        // By name, then hash; a NUL cannot occur in either.
        let (rows, next_page_token) =
            page.page(rows, |row| format!("{}\0{}", row.name, row.lockfile_hash));
        let sizes: HashMap<StoreKey, u64> = {
            let paths: Vec<_> = rows
                .iter()
                .map(|row| (key_of(&row.name, &row.lockfile_hash), row.path.clone()))
                .collect();
            tokio::task::spawn_blocking(move || {
                paths
                    .into_iter()
                    .map(|(key, path)| {
                        let bytes = crate::backend::fs_util::allocated_bytes(&path).unwrap_or(0);
                        (key, bytes)
                    })
                    .collect()
            })
            .await
            .map_err(|error| Status::internal(format!("measuring stores failed: {error}")))?
        };
        let stores = rows
            .iter()
            .map(|row| {
                let key = key_of(&row.name, &row.lockfile_hash);
                StoreInfo {
                    name: row.name.clone(),
                    lockfile_hash: row.lockfile_hash.clone(),
                    path: row.path.display().to_string(),
                    bytes: sizes.get(&key).copied().unwrap_or(0),
                    built_at: row.built_at.map(timestamp),
                    sealed: row.sealed,
                    in_use_by: in_use.get(&key).copied().unwrap_or(0),
                    repo_id: row.repo_id.as_hex(),
                }
            })
            .collect();
        Ok(Response::new(ListStoresResponse {
            stores,
            next_page_token,
        }))
    }
}

fn key_of(name: &str, hash: &str) -> StoreKey {
    StoreKey {
        name: name.to_owned(),
        lockfile_hash: hash.to_owned(),
    }
}
