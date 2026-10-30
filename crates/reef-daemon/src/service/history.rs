//! `reef.api.v1.HistoryService` (R2.16, ADR 0044): `QueryChanges` over
//! [`crate::history`]. The request is checked, run as one query on a reader
//! of the repository's index on the blocking pool, and paged newest
//! activity first; the page token's cursor is the last row's `last_at` and
//! row id.

use std::sync::Arc;
use std::sync::atomic::Ordering;

use prost::Message as _;
use prost_types::Timestamp;
use reef_api::v1::history_service_server::HistoryService;
use reef_api::v1::{
    self, ChangeActivity, ChangeGroup, HistoryChange, HistoryCoverage, QueryChangesRequest,
    QueryChangesResponse, SymbolTouch,
};
use reef_lib::jj::oplog::{self, Frontier};
use tonic::{Request, Response, Status};

use super::convert::known;
use super::operations::{InvalidChangeFilter, change_prefix};
use super::page::{MAX_PAGE_BYTES, PageRequest};
use super::reading::{bad_token, target};
use crate::daemon::AppState;
use crate::error::status;
use crate::history::HistoryError;
use crate::history::query::{self, Dimension, Query};
use crate::history::store::{self as history_store, META_BACKFILL, META_FRONTIER, Outcome};
use crate::symbols::SymbolError;

const QUERY_CHANGES: &str = "/reef.api.v1.HistoryService/QueryChanges";

/// Entries per repeated filter field at most.
const MAX_ENTRIES: usize = 100;
/// Operations `coverage.behind` counts at most.
const MAX_BEHIND: usize = 10_000;

/// Serves `HistoryService`.
#[derive(Debug)]
pub struct HistoryHandler {
    state: Arc<AppState>,
}

impl HistoryHandler {
    /// A service over `state`.
    pub fn new(state: Arc<AppState>) -> HistoryHandler {
        HistoryHandler { state }
    }
}

fn invalid(field: &'static str, message: impl Into<String>) -> Status {
    status(HistoryError::InvalidQuery {
        field,
        message: message.into(),
    })
}

fn ms(at: &Timestamp) -> i64 {
    at.seconds.saturating_mul(1000) + i64::from(at.nanos / 1_000_000)
}

fn ts(ms: i64) -> Timestamp {
    Timestamp {
        seconds: ms.div_euclid(1000),
        nanos: i32::try_from(ms.rem_euclid(1000) * 1_000_000).unwrap_or(0),
    }
}

/// `entries` checked: at most [`MAX_ENTRIES`], none empty.
fn entries(field: &'static str, entries: &[String]) -> Result<Vec<String>, Status> {
    if entries.len() > MAX_ENTRIES {
        return Err(invalid(field, format!("at most {MAX_ENTRIES} entries")));
    }
    entries
        .iter()
        .map(|entry| {
            let entry = entry.trim();
            if entry.is_empty() {
                Err(invalid(field, "an entry is empty"))
            } else {
                Ok(entry.to_owned())
            }
        })
        .collect()
}

/// The request as a [`Query`], with the page it asks for. Workspaces given
/// as paths are looked up in `repo`.
#[expect(clippy::too_many_lines, reason = "one check per field")]
fn query_of(
    state: &AppState,
    repo: &str,
    request: &QueryChangesRequest,
    page: &PageRequest,
) -> Result<Query, Status> {
    let symbols = entries("symbols", &request.symbols)?;
    let paths: Vec<String> = entries("paths", &request.paths)?
        .into_iter()
        .map(|path| path.trim_matches('/').to_owned())
        .collect();
    if paths.iter().any(String::is_empty) {
        return Err(invalid("paths", "an entry is empty"));
    }
    let mut symbol_changes = Vec::new();
    for kind in &request.symbol_changes {
        let kind: v1::SymbolChangeKind = known("symbol_changes", *kind).map_err(status)?;
        if kind == v1::SymbolChangeKind::Unspecified {
            return Err(invalid("symbol_changes", "unspecified"));
        }
        symbol_changes.push(i64::from(i32::from(kind)));
    }
    if !symbol_changes.is_empty() && symbols.is_empty() {
        return Err(invalid("symbol_changes", "needs `symbols`"));
    }
    let agents = {
        if request.agents.len() > MAX_ENTRIES {
            return Err(invalid("agents", format!("at most {MAX_ENTRIES} entries")));
        }
        request.agents.clone()
    };
    let mut workspaces = Vec::new();
    for workspace in entries("workspaces", &request.workspaces)? {
        let hex = workspace.len() >= 4
            && workspace
                .bytes()
                .all(|b| matches!(b, b'0'..=b'9' | b'a'..=b'f'));
        if hex {
            // Matched against the ids recorded, released ones included.
            workspaces.push(workspace);
        } else if workspace.starts_with('/') {
            let record = state
                .manager
                .resolve_workspace(repo, &workspace)
                .map_err(status)?;
            workspaces.push(record.id.as_hex());
        } else {
            return Err(invalid(
                "workspaces",
                "an id, a prefix of four or more lowercase hex characters, or an absolute path",
            ));
        }
    }
    if request.changes.len() > MAX_ENTRIES {
        return Err(invalid("changes", format!("at most {MAX_ENTRIES} entries")));
    }
    let mut changes = Vec::new();
    for change in &request.changes {
        let prefix = change_prefix(change).ok_or_else(|| {
            status(InvalidChangeFilter {
                change: change.clone(),
            })
        })?;
        changes.push(prefix);
    }
    let mut outcomes = Vec::new();
    for outcome in &request.outcomes {
        let outcome: v1::ChangeOutcome = known("outcomes", *outcome).map_err(status)?;
        outcomes.push(match outcome {
            v1::ChangeOutcome::Unspecified => return Err(invalid("outcomes", "unspecified")),
            v1::ChangeOutcome::Open => Outcome::Open,
            v1::ChangeOutcome::Landed => Outcome::Landed,
            v1::ChangeOutcome::Abandoned => Outcome::Abandoned,
            v1::ChangeOutcome::Undone => Outcome::Undone,
        });
    }
    let since = request.since.as_ref().map(ms);
    let until = request.until.as_ref().map(ms);
    if let (Some(since), Some(until)) = (since, until)
        && since > until
    {
        return Err(invalid("since", "after `until`"));
    }
    let mut group_by = Vec::new();
    for dimension in &request.group_by {
        let dimension: v1::ChangeDimension = known("group_by", *dimension).map_err(status)?;
        let dimension = match dimension {
            v1::ChangeDimension::Unspecified => return Err(invalid("group_by", "unspecified")),
            v1::ChangeDimension::Agent => Dimension::Agent,
            v1::ChangeDimension::Outcome => Dimension::Outcome,
            v1::ChangeDimension::Workspace => Dimension::Workspace,
        };
        if !group_by.contains(&dimension) {
            group_by.push(dimension);
        }
    }
    let after = page
        .cursor()
        .map(|cursor| {
            let (last_at, id) = cursor.split_once(':').ok_or_else(bad_token)?;
            Ok::<_, Status>((
                last_at.parse().map_err(|_| bad_token())?,
                id.parse().map_err(|_| bad_token())?,
            ))
        })
        .transpose()?;
    Ok(Query {
        symbols,
        paths,
        symbol_changes,
        agents,
        workspaces,
        changes,
        outcomes,
        since,
        until,
        include_empty: request.include_empty,
        group_by,
        limit: page.size(),
        after,
    })
}

fn change_message(change: query::Row) -> HistoryChange {
    let touches_state = match &change.touched_commit {
        None => v1::TouchState::Pending,
        Some(commit) if *commit == change.commit_id => v1::TouchState::Current,
        Some(_) => v1::TouchState::Stale,
    };
    HistoryChange {
        change_id: change.change_id,
        commit_id: change.commit_id,
        outcome: i32::try_from(change.outcome).unwrap_or(0),
        divergent: change.divergent,
        empty: touches_state != v1::TouchState::Pending && change.empty == Some(true),
        subject: change.subject,
        author_email: change.author_email,
        first_at: Some(ts(change.first_at)),
        last_at: Some(ts(change.last_at)),
        landed_at: change.landed_at.map(ts),
        ended_at: change.ended_at.map(ts),
        activity: change
            .activity
            .into_iter()
            .map(|a| ChangeActivity {
                agent: a.agent,
                workspace_id: a.workspace_id,
                first_at: Some(ts(a.first_at)),
                last_at: Some(ts(a.last_at)),
                operations: u32::try_from(a.operations).unwrap_or(u32::MAX),
                last_operation_id: a.last_operation_id.unwrap_or_default(),
            })
            .collect(),
        activity_count: u32::try_from(change.activity_count).unwrap_or(u32::MAX),
        touches: change
            .touches
            .into_iter()
            .map(|t| SymbolTouch {
                symbol: t.symbol,
                old_symbol: t.old_symbol.unwrap_or_default(),
                name: t.name,
                kind: i32::try_from(t.kind).unwrap_or(0),
                change: i32::try_from(t.change_kind).unwrap_or(0),
                path: t.path,
            })
            .collect(),
        touch_count: u32::try_from(change.touch_count).unwrap_or(u32::MAX),
        matched_paths: change.matched_paths,
        paths_changed: u32::try_from(change.paths_changed).unwrap_or(u32::MAX),
        paths_truncated: change.paths_truncated,
        symbols_changed: u32::try_from(change.symbols_changed).unwrap_or(u32::MAX),
        touches_state: touches_state.into(),
        symbols_coverage: change
            .symbols_coverage
            .and_then(|c| i32::try_from(c).ok())
            .unwrap_or(0),
    }
}

#[expect(
    clippy::too_many_lines,
    reason = "the query, the coverage, and the page"
)]
#[tonic::async_trait]
impl HistoryService for HistoryHandler {
    async fn query_changes(
        &self,
        request: Request<QueryChangesRequest>,
    ) -> Result<Response<QueryChangesResponse>, Status> {
        let request = request.into_inner();
        let state = &self.state;
        if !state.history.enabled() {
            return Err(status(HistoryError::Disabled));
        }
        // Before the token is read: a token is never for a grouped query.
        if !request.group_by.is_empty() && !request.page_token.is_empty() {
            return Err(invalid("page_token", "a grouped query is not paged"));
        }
        let page = PageRequest::new(
            QUERY_CHANGES,
            request.page_size,
            &request.page_token,
            &QueryChangesRequest {
                page_size: 0,
                page_token: String::new(),
                ..request.clone()
            },
        )
        .map_err(status)?;
        let target = target(state, &request.repo, "")?;
        let query = query_of(state, &target.repo.id.as_hex(), &request, &page)?;
        if !query.symbols.is_empty() && !state.symbols.enabled() {
            return Err(status(SymbolError::Disabled));
        }
        let held = state.history.for_query(target.repo.id).map_err(status)?;
        let store = Arc::clone(&held.store);
        let run = Arc::clone(&store);
        let (answer, (oldest, pending), frontier, backfill) =
            tokio::task::spawn_blocking(move || {
                run.read(|conn| {
                    Ok((
                        query::run(conn, &query)?,
                        query::coverage(conn)?,
                        history_store::meta(conn, META_FRONTIER)?,
                        history_store::meta(conn, META_BACKFILL)?,
                    ))
                })
            })
            .await
            .map_err(|error| Status::internal(error.to_string()))?
            .map_err(|error| status(HistoryError::from(error)))?;

        let behind = match (frontier, state.events.tip(target.repo.id)) {
            (Some(frontier), Some(tip)) => {
                let frontier = Frontier::new(
                    frontier
                        .split(',')
                        .filter_map(|id| id.parse().ok())
                        .collect::<Vec<_>>(),
                );
                state
                    .reads
                    .run(&target.handle, move || async move {
                        let heads = oplog::op_heads(&tip).await.ok()?;
                        let range = oplog::walk_after(&tip, &heads, &frontier, MAX_BEHIND)
                            .await
                            .ok()?;
                        Some(if range.truncated {
                            MAX_BEHIND
                        } else {
                            range.operations.len()
                        })
                    })
                    .await
                    .ok()
                    .flatten()
                    .unwrap_or(0)
            }
            _ => 0,
        };
        let coverage = HistoryCoverage {
            oldest_at: oldest.map(ts),
            backfilling: backfill.is_some()
                || held.backfilling.load(Ordering::Relaxed) && oldest.is_none(),
            behind: u32::try_from(behind).unwrap_or(u32::MAX),
            pending: u32::try_from(pending).unwrap_or(u32::MAX),
            symbols: state.symbols.enabled(),
        };
        if !answer.groups.is_empty() || !request.group_by.is_empty() {
            return Ok(Response::new(QueryChangesResponse {
                changes: Vec::new(),
                next_page_token: String::new(),
                groups: answer
                    .groups
                    .into_iter()
                    .map(|g| ChangeGroup {
                        agent: g.agent.unwrap_or_default(),
                        workspace_id: g.workspace_id.unwrap_or_default(),
                        outcome: g.outcome.and_then(|o| i32::try_from(o).ok()).unwrap_or(0),
                        count: u32::try_from(g.count).unwrap_or(u32::MAX),
                    })
                    .collect(),
                groups_truncated: answer.groups_truncated,
                coverage: Some(coverage),
            }));
        }
        let mut changes = Vec::with_capacity(answer.changes.len());
        let mut bytes = 0;
        let mut more = answer.more;
        let mut last: Option<(i64, i64)> = None;
        for change in answer.changes {
            let key = (change.last_at, change.id);
            let message = change_message(change);
            let size = message.encoded_len();
            if !changes.is_empty() && bytes + size > MAX_PAGE_BYTES {
                more = true;
                break;
            }
            bytes += size;
            changes.push(message);
            last = Some(key);
        }
        let next_page_token = match last {
            Some((last_at, id)) if more => page.token_at(&format!("{last_at}:{id}"), ""),
            _ => String::new(),
        };
        Ok(Response::new(QueryChangesResponse {
            changes,
            next_page_token,
            groups: Vec::new(),
            groups_truncated: false,
            coverage: Some(coverage),
        }))
    }
}
