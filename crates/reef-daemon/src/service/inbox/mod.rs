//! `InboxService` (R3.33, ADR 0088; #372, ADR 0091): reads each
//! repository's sources through the per-source READ handlers themselves,
//! places them (`crate::inbox`), moves the caller's marks, and streams the
//! inbox's changes from the board [`watch`] keeps while someone watches.
//! The only stored state is `inbox_marks`.

mod session;
pub mod watch;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use reef_api::v1::change_service_server::ChangeService as _;
use reef_api::v1::comment_service_server::CommentService as _;
use reef_api::v1::inbox_service_server::InboxService;
use reef_api::v1::workspace_service_server::WorkspaceService as _;
use reef_api::v1::{
    self, GetInboxRequest, GetInboxResponse, MarkInboxSeenRequest, MarkInboxSeenResponse,
};
use reef_lib::error::{Classify, ErrorKind};
use tonic::{Request, Response, Status};

use crate::daemon::AppState;
use crate::db::inbox::DigestBase;
use crate::error::status;
use crate::inbox::{self, Sources};
use crate::model::{RepoId, Timestamp};

/// Repositories and `digest_from` entries one request names, at most.
const MAX_REPOS: usize = 256;
/// The page size each source is read in; every page is read, so a
/// bucket's total counts every item.
const PAGE: i32 = 1000;

/// Who reads or marks, resolved at the boundary: the default person on the
/// socket and through the web UI, as every web write records it
/// (`lifecycle::identity_for`); the identity a remote token links.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Viewer {
    Identity(String),
    /// A remote token linked to none, or no default identity: digests
    /// start 24 hours ago, and marks are refused.
    Nobody,
}

/// Why the inbox refused a request.
#[derive(Debug, thiserror::Error)]
pub(crate) enum InboxError {
    /// `REMOTE_IDENTITY_REQUIRED`: a mark from a caller with no identity.
    #[error("a mark is a person's: this remote token is linked to no identity")]
    IdentityRequired,
}

impl Classify for InboxError {
    fn kind(&self) -> ErrorKind {
        ErrorKind::PermissionDenied
    }
}

impl InboxError {
    pub(crate) fn reason(&self) -> (&'static str, Vec<(&'static str, String)>) {
        match self {
            Self::IdentityRequired => (reef_api::error::REMOTE_IDENTITY_REQUIRED, Vec::new()),
        }
    }
}

pub(crate) fn viewer<T>(state: &AppState, request: &Request<T>) -> Viewer {
    let id = match super::remote_of(request) {
        Some(caller) => state
            .db
            .remote_tokens()
            .ok()
            .and_then(|rows| rows.into_iter().find(|row| row.id == caller.token_id))
            .and_then(|row| row.identity_id),
        None => state.identity.default_id().map(str::to_owned),
    };
    id.map_or(Viewer::Nobody, Viewer::Identity)
}

/// The handler.
pub struct InboxHandler {
    state: Arc<AppState>,
}

impl InboxHandler {
    /// A handler over `state`.
    pub fn new(state: Arc<AppState>) -> InboxHandler {
        InboxHandler { state }
    }
}

/// The repositories `names` names, or every registered one, in order.
pub(crate) fn repos(state: &AppState, names: &[String]) -> Result<Vec<RepoId>, Status> {
    if names.len() > MAX_REPOS {
        return Err(Status::invalid_argument(format!(
            "at most {MAX_REPOS} repositories"
        )));
    }
    if names.is_empty() {
        return registered(state);
    }
    let mut seen = HashSet::new();
    let mut repos = Vec::new();
    for name in names {
        let id = state.manager.resolve_repo(name).map_err(status)?.id;
        if seen.insert(id) {
            repos.push(id);
        }
    }
    Ok(repos)
}

/// Every registered repository, in registration order.
pub(crate) fn registered(state: &AppState) -> Result<Vec<RepoId>, Status> {
    let mut rows = state.db.repositories().map_err(status)?;
    rows.sort_by_key(|row| (row.registered_at, row.id));
    Ok(rows.into_iter().map(|row| row.id).collect())
}

/// One repository's section, its digest from the page's kept base `from`
/// when it has one, with `workspaces` its workspaces needing attention;
/// any failure to read the repository, or to list the workspaces, is its
/// `error`, so the other sections still return.
async fn section(
    state: &Arc<AppState>,
    repo: RepoId,
    viewer: &Viewer,
    from: Option<i64>,
    workspaces: Result<Vec<v1::Workspace>, Status>,
) -> v1::InboxSection {
    match read_section(state, repo, viewer, from, workspaces).await {
        Ok(section) => section,
        Err(error) => v1::InboxSection {
            repo_id: repo.as_hex(),
            error: Some(inbox_error(&error)),
            ..v1::InboxSection::default()
        },
    }
}

async fn read_section(
    state: &Arc<AppState>,
    repo: RepoId,
    viewer: &Viewer,
    from: Option<i64>,
    workspaces: Result<Vec<v1::Workspace>, Status>,
) -> Result<v1::InboxSection, Status> {
    let buckets = buckets(state, repo, workspaces?).await?;
    let landed = digest(state, repo, viewer, from, None).await?;
    Ok(v1::InboxSection {
        repo_id: repo.as_hex(),
        needs_you_now_total: u32::try_from(buckets.needs_you_now_total).unwrap_or(u32::MAX),
        watching_total: u32::try_from(buckets.watching_total).unwrap_or(u32::MAX),
        needs_you_now: buckets
            .needs_you_now
            .into_iter()
            .map(|item| item.message(repo))
            .collect(),
        watching: buckets
            .watching
            .into_iter()
            .map(|item| item.message(repo))
            .collect(),
        landed: Some(landed),
        error: None,
    })
}

/// `repo`'s items in their buckets, with `workspaces` its workspaces
/// needing attention: what every viewer sees alike.
pub(crate) async fn buckets(
    state: &Arc<AppState>,
    repo: RepoId,
    workspaces: Vec<v1::Workspace>,
) -> Result<inbox::Buckets, Status> {
    let hex = repo.as_hex();
    let proposals = proposals(state, &hex).await?;
    let help = comments(
        state,
        v1::ListCommentsRequest {
            repo: hex.clone(),
            kinds: vec![v1::CommentKind::HelpRequest.into()],
            help_states: vec![v1::HelpState::Open.into()],
            ..v1::ListCommentsRequest::default()
        },
    )
    .await?;
    let instructions = comments(
        state,
        v1::ListCommentsRequest {
            repo: hex.clone(),
            kinds: vec![v1::CommentKind::Instruction.into()],
            states: [
                v1::InstructionState::AwaitingConfirmation,
                v1::InstructionState::Pending,
                v1::InstructionState::Delivered,
                v1::InstructionState::Running,
            ]
            .into_iter()
            .map(Into::into)
            .collect(),
            ..v1::ListCommentsRequest::default()
        },
    )
    .await?;
    let assignments = assignments(state, &hex).await?;
    Ok(inbox::classify(Sources {
        repo,
        proposals,
        help,
        instructions,
        assignments,
        workspaces,
        now: Timestamp::now(),
    }))
}

/// `repo`'s digest for `viewer`: from `from` (`GetInbox`'s `digest_from`),
/// else the viewer's mark, else a day ago. The newest landings' subjects
/// come from `subjects` when the caller holds them (a board's), and are
/// read otherwise.
pub(crate) async fn digest(
    state: &Arc<AppState>,
    repo: RepoId,
    viewer: &Viewer,
    from: Option<i64>,
    subjects: Option<&HashMap<String, String>>,
) -> Result<v1::LandedDigest, Status> {
    let (base, base_seen) = base(state, repo, viewer, from)?;
    let landed = state
        .db
        .landed_since(repo, base, inbox::DIGEST_NEWEST)
        .map_err(status)?;
    let read;
    let subjects = if let Some(subjects) = subjects {
        subjects
    } else {
        read = read_subjects(state, &repo.as_hex(), &landed.newest).await?;
        &read
    };
    Ok(inbox::digest(base, base_seen, landed, subjects))
}

/// `repo`'s newest landed changes, newest first: every viewer's newest
/// landings after any base are among them.
pub(crate) fn newest_landed(
    state: &AppState,
    repo: RepoId,
) -> Result<Vec<crate::db::inbox::LandedRow>, Status> {
    let base = DigestBase {
        seq: 0,
        from_mark: false,
    };
    Ok(state
        .db
        .landed_since(repo, base, inbox::DIGEST_NEWEST)
        .map_err(status)?
        .newest)
}

/// Where `repo`'s digest starts, and since when: the page's kept base
/// `from`, else the viewer's mark, else the newest transition a day ago.
fn base(
    state: &AppState,
    repo: RepoId,
    viewer: &Viewer,
    from: Option<i64>,
) -> Result<(DigestBase, Timestamp), Status> {
    let db = &state.db;
    let mark = match viewer {
        Viewer::Identity(id) => db.inbox_marks(id, &[repo]).map_err(status)?.remove(&repo),
        Viewer::Nobody => None,
    };
    let default_since = Timestamp(Timestamp::now().0 - inbox::DIGEST_DEFAULT_MS);
    Ok(match (from, mark) {
        (Some(seq), Some(mark)) if seq == mark.seq => (
            DigestBase {
                seq,
                from_mark: true,
            },
            mark.seen_at,
        ),
        (Some(seq), _) => {
            let at = db.transition_time(repo, seq).map_err(status)?;
            let base = DigestBase {
                seq,
                from_mark: false,
            };
            (base, at.unwrap_or(default_since))
        }
        (None, Some(mark)) => (
            DigestBase {
                seq: mark.seq,
                from_mark: true,
            },
            mark.seen_at,
        ),
        (None, None) => {
            let seq = db.transition_seq_at(repo, default_since).map_err(status)?;
            (
                DigestBase {
                    seq,
                    from_mark: false,
                },
                default_since,
            )
        }
    })
}

/// Every open proposal of `repo`, page by page.
async fn proposals(state: &Arc<AppState>, repo: &str) -> Result<Vec<v1::ProposalEntry>, Status> {
    let changes = super::changes::ChangesHandler::new(Arc::clone(state));
    let mut proposals = Vec::new();
    let mut token = String::new();
    loop {
        let page = changes
            .list_proposals(Request::new(v1::ListProposalsRequest {
                repo: repo.to_owned(),
                page_size: PAGE,
                page_token: token,
                ..v1::ListProposalsRequest::default()
            }))
            .await?
            .into_inner();
        proposals.extend(page.entries);
        if page.next_page_token.is_empty() {
            return Ok(proposals);
        }
        token = page.next_page_token;
    }
}

/// Every conflict assignment of `repo` nobody owns or that waits for a
/// person, page by page.
async fn assignments(
    state: &Arc<AppState>,
    repo: &str,
) -> Result<Vec<v1::ConflictAssignment>, Status> {
    let mut assignments = Vec::new();
    let mut token = String::new();
    loop {
        let page = super::assignments::list(
            state,
            Request::new(v1::ListConflictAssignmentsRequest {
                repo: repo.to_owned(),
                states: vec![
                    v1::ConflictAssignmentState::Unowned.into(),
                    v1::ConflictAssignmentState::Blocked.into(),
                ],
                page_size: PAGE,
                page_token: token,
                ..v1::ListConflictAssignmentsRequest::default()
            }),
        )
        .await?
        .into_inner();
        assignments.extend(page.assignments);
        if page.next_page_token.is_empty() {
            return Ok(assignments);
        }
        token = page.next_page_token;
    }
}

/// Every comment `request` matches, page by page.
async fn comments(
    state: &Arc<AppState>,
    request: v1::ListCommentsRequest,
) -> Result<Vec<v1::Comment>, Status> {
    let handler = super::comments::CommentHandler::new(Arc::clone(state));
    let mut comments = Vec::new();
    let mut token = String::new();
    loop {
        let page = handler
            .list_comments(Request::new(v1::ListCommentsRequest {
                page_size: PAGE,
                page_token: token,
                ..request.clone()
            }))
            .await?
            .into_inner();
        comments.extend(page.comments);
        if page.next_page_token.is_empty() {
            return Ok(comments);
        }
        token = page.next_page_token;
    }
}

/// The descriptions of the landed changes `rows` name, by change id; one
/// whose proposal is gone has none.
pub(crate) async fn read_subjects(
    state: &Arc<AppState>,
    repo: &str,
    rows: &[crate::db::inbox::LandedRow],
) -> Result<HashMap<String, String>, Status> {
    if rows.is_empty() {
        return Ok(HashMap::new());
    }
    let changes = super::changes::ChangesHandler::new(Arc::clone(state));
    let entries = changes
        .list_proposals(Request::new(v1::ListProposalsRequest {
            repo: repo.to_owned(),
            states: vec![v1::LifecycleState::Landed.into()],
            changes: rows.iter().map(|row| row.change_id.clone()).collect(),
            page_size: i32::try_from(rows.len()).unwrap_or(i32::MAX),
            ..v1::ListProposalsRequest::default()
        }))
        .await?
        .into_inner()
        .entries;
    let mut subjects: HashMap<String, String> = rows
        .iter()
        .map(|row| (row.change_id.clone(), String::new()))
        .collect();
    for entry in entries {
        if let Some(change) = entry.change {
            subjects.insert(entry.change_id, change.description);
        }
    }
    Ok(subjects)
}

/// Active workspaces needing attention, by repository, page by page: one
/// listing for every section, since each `ListWorkspaces` call also sums
/// the machine's disk use. `repo` names the one repository read, or is
/// empty for every one.
pub(crate) async fn attention(
    state: &Arc<AppState>,
    repo: &str,
) -> Result<HashMap<String, Vec<v1::Workspace>>, Status> {
    let handler = super::workspaces::WorkspacesService::new(Arc::clone(state));
    let mut by_repo: HashMap<String, Vec<v1::Workspace>> = HashMap::new();
    let mut token = String::new();
    loop {
        let page = handler
            .list_workspaces(Request::new(v1::ListWorkspacesRequest {
                repo: repo.to_owned(),
                states: vec![v1::WorkspaceState::Active.into()],
                page_size: PAGE,
                page_token: token,
                ..v1::ListWorkspacesRequest::default()
            }))
            .await?
            .into_inner();
        for workspace in page.workspaces {
            if workspace.attention() != v1::Attention::Unspecified {
                by_repo
                    .entry(workspace.repo_id.clone())
                    .or_default()
                    .push(workspace);
            }
        }
        if page.next_page_token.is_empty() {
            return Ok(by_repo);
        }
        token = page.next_page_token;
    }
}

/// A read failure as a section carries it.
pub(crate) fn inbox_error(error: &Status) -> v1::InboxError {
    v1::InboxError {
        reason: error_reason(error),
        message: error.message().to_owned(),
    }
}

fn error_reason(error: &Status) -> String {
    reef_api::error::detail(error)
        .map(|detail| detail.reason)
        .unwrap_or_default()
}

type WatchInboxStream =
    std::pin::Pin<Box<dyn futures::Stream<Item = Result<v1::WatchInboxResponse, Status>> + Send>>;

#[tonic::async_trait]
impl InboxService for InboxHandler {
    type WatchInboxStream = WatchInboxStream;

    #[tracing::instrument(skip_all)]
    async fn watch_inbox(
        &self,
        request: Request<v1::WatchInboxRequest>,
    ) -> Result<Response<Self::WatchInboxStream>, Status> {
        let viewer = viewer(&self.state, &request);
        let request = request.into_inner();
        let named = if request.repos.is_empty() {
            None
        } else {
            Some(repos(&self.state, &request.repos)?)
        };
        let slot = self
            .state
            .subscriptions
            .admit(self.state.config.subscribe.max_subscriptions)
            .map_err(status)?;
        let watch = self.state.inbox.watch(&self.state);
        let frames = session::spawn(
            Arc::clone(&self.state),
            session::Start {
                viewer,
                named,
                watch,
                slot,
            },
        );
        Ok(Response::new(Box::pin(
            tokio_stream::wrappers::ReceiverStream::new(frames),
        )))
    }

    #[tracing::instrument(skip_all)]
    async fn get_inbox(
        &self,
        request: Request<GetInboxRequest>,
    ) -> Result<Response<GetInboxResponse>, Status> {
        let viewer = viewer(&self.state, &request);
        let request = request.into_inner();
        if request.digest_from.len() > MAX_REPOS {
            return Err(Status::invalid_argument(format!(
                "at most {MAX_REPOS} digest_from entries"
            )));
        }
        let repos = repos(&self.state, &request.repos)?;
        let mut from = HashMap::new();
        for mark in &request.digest_from {
            // A base for a repository forgotten since, or not read here, is
            // a page's stale state, not an error of this read.
            let id = match self.state.manager.resolve_repo(&mark.repo) {
                Ok(record) => record.id,
                Err(error) => {
                    let refused = status(error);
                    if refused.code() == tonic::Code::NotFound {
                        continue;
                    }
                    return Err(refused);
                }
            };
            if !repos.contains(&id) {
                continue;
            }
            from.insert(id, i64::try_from(mark.seq).unwrap_or(i64::MAX));
        }
        // A listing that fails is every section's error, not the read's.
        let scope = match repos.as_slice() {
            [one] => one.as_hex(),
            _ => String::new(),
        };
        let mut attention = attention(&self.state, &scope).await;
        let mut sections = Vec::with_capacity(repos.len());
        for repo in repos {
            let workspaces = match &mut attention {
                Ok(by_repo) => Ok(by_repo.remove(&repo.as_hex()).unwrap_or_default()),
                Err(error) => Err(error.clone()),
            };
            sections.push(
                section(
                    &self.state,
                    repo,
                    &viewer,
                    from.get(&repo).copied(),
                    workspaces,
                )
                .await,
            );
        }
        Ok(Response::new(GetInboxResponse {
            sections,
            refresh_operations: inbox::refresh_operations(),
            refresh_presence: inbox::refresh_presence(),
        }))
    }

    #[tracing::instrument(skip_all)]
    async fn mark_inbox_seen(
        &self,
        request: Request<MarkInboxSeenRequest>,
    ) -> Result<Response<MarkInboxSeenResponse>, Status> {
        let Viewer::Identity(identity) = viewer(&self.state, &request) else {
            return Err(status(InboxError::IdentityRequired));
        };
        let channel = super::identity::channel_of(&self.state, &request);
        let request = request.into_inner();
        if request.marks.is_empty() || request.marks.len() > MAX_REPOS {
            return Err(Status::invalid_argument(format!(
                "between 1 and {MAX_REPOS} marks"
            )));
        }
        let mut resolved = Vec::with_capacity(request.marks.len());
        let mut seen = HashSet::new();
        for mark in &request.marks {
            let id = self
                .state
                .manager
                .resolve_repo(&mark.repo)
                .map_err(status)?
                .id;
            if !seen.insert(id) {
                return Err(Status::invalid_argument(format!(
                    "two marks for repository {id}"
                )));
            }
            resolved.push((id, i64::try_from(mark.seq).unwrap_or(i64::MAX)));
        }
        let now = Timestamp::now();
        let mut marks = Vec::with_capacity(resolved.len());
        for (repo, seq) in resolved {
            let moved = self
                .state
                .db
                .advance_inbox_mark(&identity, repo, seq, now, channel.web_session())
                .map_err(status)?;
            if moved.moved() {
                // A web write's audit (S-253): the row keeps the last
                // session that moved the mark; the log keeps every move.
                tracing::info!(
                    target: "reef_daemon::inbox::audit",
                    identity_id = %identity,
                    repo_id = %repo,
                    from_seq = moved.from,
                    to_seq = moved.to,
                    channel = channel.name(),
                    web_session = channel.web_session().unwrap_or_default(),
                    "inbox mark moved"
                );
            }
            if moved.moved() {
                self.state.inbox.marks_moved();
            }
            marks.push(v1::InboxMark {
                repo: repo.as_hex(),
                seq: moved.to.unsigned_abs(),
            });
        }
        Ok(Response::new(MarkInboxSeenResponse { marks }))
    }
}
