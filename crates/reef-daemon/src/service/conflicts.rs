//! `reef.api.v1.ConflictService` (R3.25, ADR 0065): jj's stored conflicts
//! as structured records, read-only.
//!
//! Every read derives the objects from jj again (`reef_lib::jj::conflict`):
//! the terms, each term's source (proposed by jj's label, verified by
//! content), the conflicted files and, on request, their hunks. Each side's
//! provenance is R3.04's records of the source's change, read and
//! re-verified with `provenance::read::read_records` on the blocking pool,
//! once per change and commit a response names.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::{Instant, SystemTime, UNIX_EPOCH};

use prost::Message as _;
use reef_api::v1::conflict_service_server::ConflictService;
use reef_api::v1::{
    self, GetConflictRequest, GetConflictResponse, ListConflictsRequest, ListConflictsResponse,
};
use reef_lib::error::{Classify, ErrorKind};
use reef_lib::jj::CommitIdHex;
use reef_lib::jj::conflict::{
    self, Attribution, ConflictObject, FileHunks, FileKind, ReadOptions, Role, Source, ValueType,
};
use reef_lib::jj::diff;
use reef_lib::jj::log::Describer;
use reef_lib::jj::revset::{self, RevisionError};
use tonic::{Request, Response, Status};

use super::convert;
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{
    bad_token, op_id, positional, repo_at, snapshot_first, target, uses_working_copy,
};
use crate::daemon::AppState;
use crate::db::{DbError, ProvenanceFilter};
use crate::error::{Advice, Advise, status};
use crate::model::{RepoId, Timestamp};

const LIST_CONFLICTS: &str = "/reef.api.v1.ConflictService/ListConflicts";
const GET_CONFLICT: &str = "/reef.api.v1.ConflictService/GetConflict";
/// `ListConflicts`' revset when the request leaves it empty.
pub const DEFAULT_REVSET: &str = "mutable() & conflicts()";
/// Conflicts per `ListConflicts` page when the request says 0, and at most.
const LIST_DEFAULT: usize = 50;
const LIST_MAX: usize = 200;
/// Files per `ListConflicts` entry.
const LIST_FILES: usize = 20;
/// Files per `GetConflict` page when the request says 0, and at most.
const GET_DEFAULT: usize = 200;
const GET_MAX: usize = 1000;
/// Bytes of each term a file's hunks read by default, and at most.
const DEFAULT_FILE_BYTES: usize = 1 << 20;
const MAX_FILE_BYTES: u64 = 16 << 20;
/// Provenance records read per change.
const RECORDS_READ: usize = 200;
/// Sessions listed per side.
const SESSIONS_LISTED: usize = 10;
/// Changes whose provenance one response reads, at most (S-129).
const PROVENANCE_CHANGES: usize = 64;
/// Bytes of hunks one `GetConflict` page computes before it ends.
const PAGE_HUNK_BYTES: usize = 2 << 20;

/// A conflict read that could not be asked for, or failed outside jj.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ConflictReadError {
    /// `max_file_bytes` over 16 MiB.
    #[error("max_file_bytes is {got}; at most {max}")]
    MaxFileBytes {
        /// What was asked.
        got: u64,
        /// The limit.
        max: u64,
    },
    /// A blocking task failed.
    #[error("conflict read failed: {0}")]
    Task(String),
}

impl Classify for ConflictReadError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::MaxFileBytes { .. } => ErrorKind::InvalidArgument,
            Self::Task(_) => ErrorKind::Internal,
        }
    }
}

impl Advise for ConflictReadError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::MaxFileBytes { .. } => Some(Advice::hint(
                "Ask for at most 16 MiB per term; a larger file is reported `too_large`, and `reef file read -r <source commit>` reads it whole.",
            )),
            Self::Task(_) => None,
        }
    }
}

impl ConflictReadError {
    /// The reason and metadata of `ErrorDetail`, for `crate::error`.
    pub(crate) fn reason(&self) -> Option<(&'static str, Vec<(&'static str, String)>)> {
        match self {
            Self::MaxFileBytes { max, .. } => Some((
                reef_api::error::MAX_FILE_BYTES_TOO_LARGE,
                vec![("max", max.to_string())],
            )),
            Self::Task(_) => None,
        }
    }
}

fn task(error: &tokio::task::JoinError) -> Status {
    status(ConflictReadError::Task(error.to_string()))
}

/// Serves `ConflictService`.
#[derive(Debug)]
pub struct ConflictHandler {
    state: Arc<AppState>,
}

impl ConflictHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> ConflictHandler {
        ConflictHandler { state }
    }
}

/// One conflict as read on the repository thread, before its provenance.
struct Read {
    change: v1::Change,
    object: ConflictObject,
    sources: Vec<Option<Source>>,
    hunks: HashMap<String, FileHunks>,
    committed_at: SystemTime,
}

/// A change and the commit its provenance summary is about.
type ProvenanceKey = (String, String);

fn millis(time: SystemTime) -> i64 {
    match time.duration_since(UNIX_EPOCH) {
        Ok(after) => i64::try_from(after.as_millis()).unwrap_or(i64::MAX),
        Err(before) => -i64::try_from(before.duration().as_millis()).unwrap_or(i64::MAX),
    }
}

/// A change's newest records, read and verified once per response, and
/// how many it has in all.
struct ChangeRecords {
    records: Vec<v1::ProvenanceRecord>,
    total: u64,
}

fn change_records(
    state: &AppState,
    repo: RepoId,
    change_id: &str,
) -> Result<ChangeRecords, DbError> {
    let db = &state.db;
    let filter = ProvenanceFilter {
        change_ids: vec![change_id.to_owned()],
        ..ProvenanceFilter::default()
    };
    let rows = db.list_provenance(repo, &filter, None, RECORDS_READ)?;
    let total = if rows.len() < RECORDS_READ {
        u64::try_from(rows.len()).unwrap_or(u64::MAX)
    } else {
        db.provenance_count_by_change(repo, change_id)?
    };
    Ok(ChangeRecords {
        records: crate::provenance::read::read_records(db, rows, false)?,
        total,
    })
}

/// Unix milliseconds of `time`.
fn timestamp_millis(time: &prost_types::Timestamp) -> i64 {
    time.seconds
        .saturating_mul(1000)
        .saturating_add(i64::from(time.nanos.div_euclid(1_000_000)))
}

/// The provenance summary of `change`'s records (`change_id`), about the
/// commit `commit_id` committed at `committed_at`. Blocking.
fn side_provenance(
    state: &AppState,
    repo: RepoId,
    change_id: &str,
    change: &ChangeRecords,
    commit_id: &str,
    committed_at: SystemTime,
) -> Result<v1::ConflictSideProvenance, DbError> {
    let db = &state.db;
    let records = &change.records;
    let valid = |record: &&v1::ProvenanceRecord| record.signature() == v1::SignatureCheck::Valid;
    let invalid = records.iter().filter(|record| !valid(record)).count();
    let mut commit_record = records
        .iter()
        .find(|record| record.commit_id == commit_id)
        .cloned();
    if commit_record.is_none()
        && let Some(stored) = db.provenance_by_commit(repo, commit_id)?
        && stored.row.change_id == change_id
    {
        commit_record = crate::provenance::read::read_records(db, vec![stored], false)?
            .into_iter()
            .next();
    }
    // The newest record that verifies, at or before the commit, else the
    // newest that verifies: a record that does not verify never speaks
    // for a side.
    let at = millis(committed_at);
    let latest_record = records
        .iter()
        .filter(valid)
        .find(|record| {
            record
                .recorded_at
                .as_ref()
                .is_some_and(|t| timestamp_millis(t) <= at)
        })
        .or_else(|| records.iter().find(valid))
        .cloned();
    let mut order: Vec<&str> = Vec::new();
    let mut counts: HashMap<&str, u64> = HashMap::new();
    for record in records {
        if record.session_id.is_empty() {
            continue;
        }
        let count = counts.entry(record.session_id.as_str()).or_insert(0);
        if *count == 0 {
            order.push(record.session_id.as_str());
        }
        *count += 1;
    }
    let mut sessions = Vec::new();
    for session in order.iter().take(SESSIONS_LISTED) {
        if let Some(row) = db.provenance_session(session)? {
            sessions.push(
                state
                    .provenance
                    .session_message(&row, counts.get(session).copied().unwrap_or(0))?,
            );
        }
    }
    Ok(v1::ConflictSideProvenance {
        change_id: change_id.to_owned(),
        commit_record,
        latest_record,
        sessions,
        sessions_total: u32::try_from(order.len()).unwrap_or(u32::MAX),
        records: change.total,
        invalid_records: u64::try_from(invalid).unwrap_or(u64::MAX),
    })
}

/// The provenance summaries `reads` need: the conflicted changes', and
/// each `Verified` side's, each change's records read once, and at most
/// [`PROVENANCE_CHANGES`] changes a response. Blocking.
fn provenance_for(
    state: &AppState,
    repo: RepoId,
    reads: &[Read],
) -> Result<HashMap<ProvenanceKey, v1::ConflictSideProvenance>, DbError> {
    let mut wanted: Vec<(ProvenanceKey, SystemTime)> = Vec::new();
    let mut seen: HashSet<ProvenanceKey> = HashSet::new();
    for read in reads {
        let key = (read.change.change_id.clone(), read.change.commit_id.clone());
        if seen.insert(key.clone()) {
            wanted.push((key, read.committed_at));
        }
        for (term, source) in read.object.terms.iter().zip(&read.sources) {
            let Some(source) = source.as_ref().filter(|source| {
                term.role == Role::Side && source.attribution == Attribution::Verified
            }) else {
                continue;
            };
            let key = (source.change_id.to_string(), source.commit_id.to_string());
            if seen.insert(key.clone()) {
                wanted.push((key, source.committed_at));
            }
        }
    }
    let mut changes: HashMap<String, ChangeRecords> = HashMap::new();
    let mut found = HashMap::with_capacity(wanted.len());
    for ((change, commit), at) in wanted {
        if !changes.contains_key(&change) {
            if changes.len() >= PROVENANCE_CHANGES {
                continue;
            }
            let records = change_records(state, repo, &change)?;
            changes.insert(change.clone(), records);
        }
        let summary = side_provenance(state, repo, &change, &changes[&change], &commit, at)?;
        found.insert((change, commit), summary);
    }
    Ok(found)
}

fn attribution(source: Option<&Source>) -> v1::ConflictAttribution {
    match source.map(|s| s.attribution) {
        Some(Attribution::Verified) => v1::ConflictAttribution::Verified,
        Some(Attribution::Label) => v1::ConflictAttribution::Label,
        None => v1::ConflictAttribution::None,
    }
}

fn file_kind(kind: FileKind) -> v1::ConflictFileKind {
    match kind {
        FileKind::Content => v1::ConflictFileKind::Content,
        FileKind::Executable => v1::ConflictFileKind::Executable,
        FileKind::AddAdd => v1::ConflictFileKind::AddAdd,
        FileKind::ModifyDelete => v1::ConflictFileKind::ModifyDelete,
        FileKind::Type => v1::ConflictFileKind::Type,
        FileKind::Other => v1::ConflictFileKind::Other,
    }
}

fn value_type(kind: ValueType) -> v1::ConflictValueType {
    match kind {
        ValueType::Absent => v1::ConflictValueType::Absent,
        ValueType::File => v1::ConflictValueType::File,
        ValueType::Symlink => v1::ConflictValueType::Symlink,
        ValueType::Tree => v1::ConflictValueType::Tree,
        ValueType::Submodule => v1::ConflictValueType::Submodule,
    }
}

fn hunk(hunk: &conflict::Hunk) -> v1::ConflictHunk {
    v1::ConflictHunk {
        resolved: hunk.resolved,
        text: hunk.text.clone(),
        terms: hunk
            .terms
            .iter()
            .map(|term| v1::ConflictHunkTerm {
                index: term.index,
                text: term.text.clone(),
                start_line: term.start_line,
                line_count: term.line_count,
            })
            .collect(),
    }
}

/// A term's source as a message; empty with `NONE` when there is none.
fn source_message(source: Option<&Source>) -> v1::ConflictSource {
    v1::ConflictSource {
        commit_id: source.map(|s| s.commit_id.to_string()).unwrap_or_default(),
        change_id: source.map(|s| s.change_id.to_string()).unwrap_or_default(),
        attribution: attribution(source).into(),
        description: source.map(|s| s.description.clone()).unwrap_or_default(),
        author: source.map(|s| v1::Signature {
            name: s.author.name.clone(),
            email: s.author.email.clone(),
            at: Some(convert::system_timestamp(s.author.at)),
        }),
        committed_at: source.map(|s| convert::system_timestamp(s.committed_at)),
    }
}

/// `read` as a message, with its files.
fn message(
    read: Read,
    provenance: &HashMap<ProvenanceKey, v1::ConflictSideProvenance>,
    values: bool,
) -> v1::Conflict {
    let Read {
        change,
        object,
        sources,
        hunks,
        ..
    } = read;
    let terms = object
        .terms
        .iter()
        .enumerate()
        .map(|(index, term)| {
            let source = sources.get(index).and_then(Option::as_ref);
            let mut message = v1::ConflictTerm {
                index: term.index,
                label: term.label.clone(),
                origin: term.origin.clone(),
                source: Some(source_message(source)),
                provenance: source
                    .filter(|s| term.role == Role::Side && s.attribution == Attribution::Verified)
                    .and_then(|s| {
                        provenance
                            .get(&(s.change_id.to_string(), s.commit_id.to_string()))
                            .cloned()
                    }),
                ..v1::ConflictTerm::default()
            };
            message.set_role(match term.role {
                Role::Side => v1::ConflictTermRole::Side,
                Role::Base => v1::ConflictTermRole::Base,
            });
            message
        })
        .collect();
    let files = object
        .files
        .iter()
        .map(|file| {
            let mut message = v1::ConflictFile {
                path: file.path.clone(),
                id: file.id.clone(),
                inherited: file.inherited,
                terms: if values {
                    file.terms
                        .iter()
                        .map(|value| {
                            let mut message = v1::ConflictValue {
                                index: value.index,
                                id: value.id.clone(),
                                executable: value.executable,
                                ..v1::ConflictValue::default()
                            };
                            message.set_type(value_type(value.kind));
                            message
                        })
                        .collect()
                } else {
                    Vec::new()
                },
                ..v1::ConflictFile::default()
            };
            message.set_kind(file_kind(file.kind));
            match hunks.get(&file.path) {
                Some(FileHunks::Hunks(hunks)) => {
                    message.hunks = hunks.iter().map(hunk).collect();
                }
                Some(FileHunks::Binary) => message.binary = true,
                Some(FileHunks::TooLarge) => message.too_large = true,
                Some(FileHunks::NotFiles) | None => {}
            }
            message
        })
        .collect();
    let change_provenance = provenance
        .get(&(change.change_id.clone(), change.commit_id.clone()))
        .cloned();
    let mut message = v1::Conflict {
        id: object.id.clone(),
        change: Some(change),
        arity: object.arity,
        terms,
        change_provenance,
        files,
        files_total: object.files_total,
        introduced: object.introduced,
        too_wide: object.too_wide,
        ..v1::Conflict::default()
    };
    message.set_kind(v1::ConflictKind::Tree);
    message
}

/// The page size a request asks for, between `default` and `max`.
fn size_of(page_size: i32, page: &PageRequest, default: usize, max: usize) -> usize {
    if page_size == 0 {
        default
    } else {
        page.size().min(max)
    }
}

/// The end of the operation `repo` is loaded at, for a lifecycle as of it.
fn op_end(repo: &reef_lib::jj::ReadonlyRepo) -> Timestamp {
    Timestamp::from(reef_lib::jj::oplog::entry(repo.operation()).end)
}

#[tonic::async_trait]
impl ConflictService for ConflictHandler {
    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_conflicts(
        &self,
        request: Request<ListConflictsRequest>,
    ) -> Result<Response<ListConflictsResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let page = PageRequest::new(
            LIST_CONFLICTS,
            request.page_size,
            &request.page_token,
            &ListConflictsRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let limit = size_of(request.page_size, &page, LIST_DEFAULT, LIST_MAX);
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let revset_text = if request.revset.trim().is_empty() {
            DEFAULT_REVSET.to_owned()
        } else {
            request.revset.clone()
        };
        let (skip, expected) = match page.cursor() {
            Some(cursor) if page.pinned().is_some() => positional(cursor)?,
            Some(_) => return Err(bad_token()),
            None => (0, String::new()),
        };
        if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(state, &target, vec![revset_text.clone()]).await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let (stop, _guard) = state.reads.stop();
        let pinned = page.pinned().map(str::to_owned);
        let at_operation = request.at_operation.clone();
        let introduced_only = request.introduced_only;
        let read_target = target.clone();
        let (found, positions, more, resume, at, ended) = state
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
                let parsed = revset::parse(&repo, &revset_text, &target.scope()).map_err(status)?;
                let listed = conflict::list(
                    &repo,
                    &parsed,
                    skip,
                    limit,
                    LIST_FILES,
                    introduced_only,
                    &|| stop.requested(),
                )
                .await
                .map_err(status)?;
                if skip > 0
                    && listed
                        .last_skipped
                        .as_ref()
                        .map(ToString::to_string)
                        .as_deref()
                        != Some(expected.as_str())
                {
                    return Err(bad_token());
                }
                let describer = Describer::new(&repo, &target.scope()).map_err(status)?;
                let mut found = Vec::with_capacity(listed.conflicts.len());
                let positions: Vec<usize> =
                    listed.conflicts.iter().map(|item| item.position).collect();
                for item in listed.conflicts {
                    let info = describer.describe(&item.commit_id).await.map_err(status)?;
                    let sources = conflict::attribute(&repo, &item.commit_id, &item.object)
                        .await
                        .map_err(status)?;
                    found.push(Read {
                        committed_at: info.committer.at,
                        change: convert::change(&info),
                        object: item.object,
                        sources,
                        hunks: HashMap::new(),
                    });
                }
                let resume = listed
                    .resume
                    .map(|(position, commit)| (position, commit.to_string()));
                Ok::<_, Status>((
                    found,
                    positions,
                    listed.more,
                    resume,
                    op_id(&repo),
                    op_end(&repo),
                ))
            })
            .await
            .map_err(status)??;
        let mut found = found;
        let as_of = (!request.at_operation.is_empty() || page.pinned().is_some())
            .then(|| (ended, at.clone()));
        let mut changes: Vec<v1::Change> = found.iter().map(|read| read.change.clone()).collect();
        crate::lifecycle::messages::fill(state, target.repo.id, changes.iter_mut(), as_of).await;
        for (read, change) in found.iter_mut().zip(changes) {
            read.change = change;
        }
        let summaries = {
            let state = Arc::clone(state);
            let repo = target.repo.id;
            tokio::task::spawn_blocking(move || {
                let summaries = provenance_for(&state, repo, &found);
                (found, summaries)
            })
            .await
            .map_err(|error| task(&error))?
        };
        let (found, summaries) = summaries;
        let summaries = summaries.map_err(status)?;
        let mut conflicts = Vec::with_capacity(found.len());
        let mut bytes = 0;
        let mut trimmed = false;
        let mut last_kept: Option<(usize, String)> = None;
        for (read, position) in found.into_iter().zip(positions) {
            let commit = read.change.commit_id.clone();
            let message = message(read, &summaries, false);
            let size = message.encoded_len();
            if !conflicts.is_empty() && bytes + size > MAX_PAGE_BYTES {
                trimmed = true;
                break;
            }
            bytes += size;
            conflicts.push(message);
            last_kept = Some((position, commit));
        }
        let resume = if trimmed {
            last_kept
        } else if more {
            resume
        } else {
            None
        };
        if let Err(error) = crate::assign::look(state, target.repo.id).await {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot bring conflict assignments up to date");
        }
        super::assignments::fill(state, target.repo.id, &mut conflicts);
        let next_page_token = match resume {
            Some((position, commit)) => page.token_at(&format!("{position}:{commit}"), &at),
            None => String::new(),
        };
        Ok(Response::new(ListConflictsResponse {
            conflicts,
            next_page_token,
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_conflict(
        &self,
        request: Request<GetConflictRequest>,
    ) -> Result<Response<GetConflictResponse>, Status> {
        let arrived = Instant::now();
        let agent = super::caller_of(&request)?;
        let request = request.into_inner();
        let page = PageRequest::new(
            GET_CONFLICT,
            request.page_size,
            &request.page_token,
            &GetConflictRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let limit = size_of(request.page_size, &page, GET_DEFAULT, GET_MAX);
        let state = &self.state;
        let target = target(state, &request.repo, &request.workspace)?;
        let revision = if request.revision.trim().is_empty() {
            if target.workspace.is_none() {
                return Err(status(RevisionError::Required));
            }
            "@".to_owned()
        } else {
            request.revision.clone()
        };
        if request.max_file_bytes > MAX_FILE_BYTES {
            return Err(status(ConflictReadError::MaxFileBytes {
                got: request.max_file_bytes,
                max: MAX_FILE_BYTES,
            }));
        }
        let max_file_bytes = match request.max_file_bytes {
            0 => DEFAULT_FILE_BYTES,
            n => usize::try_from(n).unwrap_or(usize::MAX),
        };
        diff::check_paths(&request.paths).map_err(status)?;
        if let Some(cursor) = page.cursor() {
            diff::check_paths(&[cursor.to_owned()]).map_err(|_| bad_token())?;
        }
        if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
            let needed = uses_working_copy(state, &target, vec![revision.clone()]).await?;
            snapshot_first(state, &target, needed, arrived, agent).await?;
        }
        let head = state.reads.current(&target.handle).await.map_err(status)?;
        let reads = Arc::clone(&state.reads);
        let pinned = page.pinned().map(str::to_owned);
        let at_operation = request.at_operation.clone();
        let options = ReadOptions {
            paths: request.paths.clone(),
            after: page.cursor().map(str::to_owned),
            limit,
        };
        let want_hunks = request.hunks;
        let read_target = target.clone();
        let (read, more, at, ended) = state
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
                let commit: CommitIdHex = resolved.commit_id.clone();
                let mut object = conflict::read(&repo, &commit, &options)
                    .await
                    .map_err(status)?;
                let sources = conflict::attribute(&repo, &commit, &object)
                    .await
                    .map_err(status)?;
                let mut hunks = HashMap::new();
                if want_hunks && !object.too_wide {
                    let merge = repo
                        .settings()
                        .merge_options()
                        .map_err(|error| status(diff::DiffError::Settings(error)))?;
                    let tree = reef_lib::jj::files::commit_tree(&repo, &commit)
                        .await
                        .map_err(status)?;
                    // The page ends after the file whose hunks pass the
                    // page's budget (S-129); the next page starts after it.
                    let mut computed = 0usize;
                    let mut kept = object.files.len();
                    for (position, file) in object.files.iter().enumerate() {
                        if computed > PAGE_HUNK_BYTES {
                            kept = position;
                            break;
                        }
                        if !matches!(file.kind, FileKind::Content | FileKind::AddAdd) {
                            continue;
                        }
                        let found = conflict::hunks(&tree, &file.path, &merge, max_file_bytes)
                            .await
                            .map_err(status)?;
                        if let FileHunks::Hunks(hunks) = &found {
                            computed += hunks
                                .iter()
                                .map(|hunk| {
                                    hunk.text.len()
                                        + hunk.terms.iter().map(|t| t.text.len()).sum::<usize>()
                                })
                                .sum::<usize>();
                        }
                        hunks.insert(file.path.clone(), found);
                    }
                    if kept < object.files.len() {
                        object.files.truncate(kept);
                        object.more = true;
                    }
                }
                let describer = Describer::new(&repo, &target.scope()).map_err(status)?;
                let info = describer.describe(&commit).await.map_err(status)?;
                let more = object.more;
                Ok::<_, Status>((
                    Read {
                        committed_at: info.committer.at,
                        change: convert::change(&info),
                        object,
                        sources,
                        hunks,
                    },
                    more,
                    op_id(&repo),
                    op_end(&repo),
                ))
            })
            .await
            .map_err(status)??;
        let mut read = read;
        let as_of = (!request.at_operation.is_empty() || page.pinned().is_some())
            .then(|| (ended, at.clone()));
        crate::lifecycle::messages::fill(
            state,
            target.repo.id,
            std::iter::once(&mut read.change),
            as_of,
        )
        .await;
        let (read, summaries) = {
            let state = Arc::clone(state);
            let repo = target.repo.id;
            tokio::task::spawn_blocking(move || {
                let reads = vec![read];
                let summaries = provenance_for(&state, repo, &reads);
                (reads, summaries)
            })
            .await
            .map_err(|error| task(&error))?
        };
        let summaries = summaries.map_err(status)?;
        let read = read
            .into_iter()
            .next()
            .ok_or_else(|| status(ConflictReadError::Task("the read was lost".to_owned())))?;
        let mut conflict = message(read, &summaries, true);
        // Trim the page's files to the response limit.
        let mut trimmed = false;
        let mut size = conflict.encoded_len();
        while size > MAX_PAGE_BYTES && conflict.files.len() > 1 {
            conflict.files.pop();
            trimmed = true;
            size = conflict.encoded_len();
        }
        // One file still over: it goes without its hunks.
        if size > MAX_PAGE_BYTES
            && let Some(file) = conflict.files.last_mut()
        {
            file.hunks.clear();
            file.too_large = true;
        }
        let next_page_token = match conflict.files.last() {
            Some(last) if more || trimmed => page.token_at(&last.path, &at),
            _ => String::new(),
        };
        if let Err(error) = crate::assign::look(state, target.repo.id).await {
            tracing::warn!(error = %reef_lib::error::ErrorChain(&error), "cannot bring conflict assignments up to date");
        }
        super::assignments::fill(state, target.repo.id, std::slice::from_mut(&mut conflict));
        Ok(Response::new(GetConflictResponse {
            conflict: Some(conflict),
            next_page_token,
            at_operation: at,
        }))
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn list_conflict_assignments(
        &self,
        request: Request<v1::ListConflictAssignmentsRequest>,
    ) -> Result<Response<v1::ListConflictAssignmentsResponse>, Status> {
        super::assignments::list(&self.state, request).await
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn get_conflict_assignment(
        &self,
        request: Request<v1::GetConflictAssignmentRequest>,
    ) -> Result<Response<v1::GetConflictAssignmentResponse>, Status> {
        super::assignments::get(&self.state, request).await
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn claim_conflict(
        &self,
        request: Request<v1::ClaimConflictRequest>,
    ) -> Result<Response<v1::ClaimConflictResponse>, Status> {
        super::assignments::claim(&self.state, request).await
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn release_conflict(
        &self,
        request: Request<v1::ReleaseConflictRequest>,
    ) -> Result<Response<v1::ReleaseConflictResponse>, Status> {
        super::assignments::release(&self.state, request).await
    }

    #[tracing::instrument(skip_all, fields(repo, workspace_id))]
    async fn assign_conflict(
        &self,
        request: Request<v1::AssignConflictRequest>,
    ) -> Result<Response<v1::AssignConflictResponse>, Status> {
        super::assignments::assign(&self.state, request).await
    }
}
