//! Queries on `comments` and `comment_events` (R3.12, ADR 0069).
//!
//! A comment is written `pending` with its creation event before the
//! operation that records it, and committed with the operation's id; each
//! later move of an instruction is an event written the same way, whose
//! commit also updates the comment's instruction columns
//! ([`Db::commit_comment_event`]). Reads see committed comments only. Both
//! tables are per repository and deleted with it; GC maps operation ids
//! ([`Db::map_comment_operations`]) and deletes nothing.

use std::collections::HashMap;
use std::fmt::Write as _;

use rusqlite::types::Value;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::evidence::text_at;
use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// The states an instruction is unfinished in.
pub const OPEN_STATES: [&str; 4] = ["awaiting_confirmation", "pending", "delivered", "running"];

/// An instruction's columns of a comment row.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct InstructionColumns {
    /// Its state's text (`reef_api`'s names in snake case).
    pub state: String,
    /// Why it got there: `request`, `reproposed`, `change_final`,
    /// `expired`.
    pub cause: String,
    /// The estimate, as JSON.
    pub estimate: String,
    /// Its digest.
    pub estimate_digest: String,
    /// `proposal`, `provenance` or `none`.
    pub target_source: String,
    /// The target's provenance session.
    pub target_session_id: Option<String>,
    /// The client session it declared.
    pub target_client_session: Option<String>,
    /// The harness it declared.
    pub target_harness: Option<String>,
    /// Its agent.
    pub target_agent: Option<String>,
    /// Its workspace.
    pub target_workspace_id: Option<String>,
    /// The client session that took it.
    pub claimed_client_session: Option<String>,
    /// The workspace that took it.
    pub claimed_workspace_id: Option<String>,
    /// The agent that took it.
    pub claimed_agent: Option<String>,
    /// The remote token that took it.
    pub claimed_token: Option<String>,
    /// `cli`, `hook`, `mcp` or `sdk`.
    pub channel: Option<String>,
    /// Whether a session other than the target took it.
    pub taken: bool,
    /// What its session said, or why it ended.
    pub summary: Option<String>,
    /// When it was confirmed.
    pub confirmed_at: Option<Timestamp>,
    /// When it was taken.
    pub delivered_at: Option<Timestamp>,
    /// When it started.
    pub started_at: Option<Timestamp>,
    /// When it ended.
    pub finished_at: Option<Timestamp>,
}

/// One comment, as `comments` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `cmt_` and 16 hex digits.
    pub id: String,
    /// Whether its operation is known to exist.
    pub committed: bool,
    /// The change, in hex.
    pub change_id: String,
    /// Its proposal round when written.
    pub round: u32,
    /// The commit its author saw.
    pub commit_id: String,
    /// The anchor's path.
    pub path: Option<String>,
    /// `new` or `old`, with a path.
    pub side: Option<String>,
    /// The anchor's first line.
    pub start_line: Option<u32>,
    /// Its last line.
    pub end_line: Option<u32>,
    /// The comment it replies to.
    pub reply_to: Option<String>,
    /// `note` or `instruction`.
    pub kind: String,
    /// The text.
    pub text: String,
    /// Its SHA-256 in hex.
    pub text_sha256: String,
    /// The call's `reef-agent`.
    pub agent: Option<String>,
    /// `human` or `agent`.
    pub actor: String,
    /// The identity recorded.
    pub identity_id: Option<String>,
    /// Its name.
    pub identity_name: Option<String>,
    /// The named workspace's provenance session.
    pub session_id: Option<String>,
    /// The remote token the call came through.
    pub remote_token: Option<String>,
    /// Its name then.
    pub remote_token_name: Option<String>,
    /// When the pending row was written.
    pub created_at: Timestamp,
    /// The operation that recorded it.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// The operation's end.
    pub recorded_at: Option<Timestamp>,
    /// For an instruction, its columns.
    pub instruction: Option<InstructionColumns>,
    /// When the row last changed.
    pub updated_at: Timestamp,
}

/// One comment write, as `comment_events` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CommentEventRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `cev_` and 16 hex digits.
    pub id: String,
    /// Its place among the repository's committed events.
    pub seq: Option<i64>,
    /// Whether its operation is known to exist.
    pub committed: bool,
    /// The comment.
    pub comment_id: String,
    /// Its change.
    pub change_id: String,
    /// `comment`, `instruct`, `confirm`, `claim`, `start`, `finish`,
    /// `cancel` or `release`.
    pub event: String,
    /// An instruction's state before; `None` for a creation.
    pub from_state: Option<String>,
    /// Its state after; `None` for a note.
    pub to_state: Option<String>,
    /// Why.
    pub cause: String,
    /// For `reproposed`, the transition.
    pub transition_id: Option<String>,
    /// The call's `reef-agent`.
    pub agent: Option<String>,
    /// `human`, `agent` or `daemon`.
    pub actor: String,
    /// The identity recorded.
    pub identity_id: Option<String>,
    /// The remote token the call came through.
    pub remote_token: Option<String>,
    /// What the session said.
    pub summary: Option<String>,
    /// For a claim, the client session.
    pub claimed_client_session: Option<String>,
    /// For a claim, the workspace.
    pub claimed_workspace_id: Option<String>,
    /// For a claim, the agent.
    pub claimed_agent: Option<String>,
    /// For a claim, the remote token.
    pub claimed_token: Option<String>,
    /// For a claim, how.
    pub channel: Option<String>,
    /// For a claim, whether it took another session's instruction.
    pub taken: bool,
    /// The operation that recorded it.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// When the pending row was written.
    pub created_at: Timestamp,
    /// The operation's end.
    pub recorded_at: Option<Timestamp>,
}

/// Which comments a listing returns: the fields AND, an empty list matches
/// everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct CommentFilter {
    /// Change id prefixes in hex.
    pub changes: Vec<String>,
    /// `note`, `instruction`.
    pub kinds: Vec<String>,
    /// Instruction states; a state filter leaves notes out.
    pub states: Vec<String>,
    /// Only instructions this client session is the target or claimer of.
    pub client_session: Option<String>,
    /// Only comments written after this.
    pub created_after: Option<Timestamp>,
}

/// What a claim looks for: an instruction in `pending` that matches.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClaimQuery {
    /// One comment by id.
    pub comment_id: Option<String>,
    /// The client sessions the caller speaks for.
    pub client_sessions: Vec<String>,
    /// The workspace the caller works in.
    pub workspace_id: Option<String>,
    /// The change the caller works on.
    pub change_id: Option<String>,
    /// Take an instruction for another session on `change_id`.
    pub take: bool,
}

const COMMENT_COLUMNS: &str = "repo_id, id, status, change_id, round, commit_id, path, side,
     start_line, end_line, reply_to, kind, text, text_sha256, agent, actor, identity_id,
     identity_name, session_id, remote_token, remote_token_name, created_at, operation_id,
     operation_collected, recorded_at, state, cause, estimate, estimate_digest, target_source,
     target_session_id, target_client_session, target_harness, target_agent,
     target_workspace_id, claimed_client_session, claimed_workspace_id, claimed_agent,
     claimed_token, channel, taken, summary, confirmed_at, delivered_at, started_at,
     finished_at, updated_at";

const EVENT_COLUMNS: &str = "repo_id, id, seq, status, comment_id, change_id, event, from_state,
     to_state, cause, transition_id, agent, actor, identity_id, remote_token, summary,
     claimed_client_session, claimed_workspace_id, claimed_agent, claimed_token, channel, taken,
     operation_id, operation_collected, created_at, recorded_at";

impl CommentEventRow {
    /// An event of `repo` with every other field empty, written now: the
    /// base the write paths fill in.
    pub fn blank(repo: RepoId) -> CommentEventRow {
        CommentEventRow {
            repo_id: repo,
            id: String::new(),
            seq: None,
            committed: false,
            comment_id: String::new(),
            change_id: String::new(),
            event: String::new(),
            from_state: None,
            to_state: None,
            cause: String::new(),
            transition_id: None,
            agent: None,
            actor: String::new(),
            identity_id: None,
            remote_token: None,
            summary: None,
            claimed_client_session: None,
            claimed_workspace_id: None,
            claimed_agent: None,
            claimed_token: None,
            channel: None,
            taken: false,
            operation_id: None,
            operation_collected: false,
            created_at: Timestamp::now(),
            recorded_at: None,
        }
    }
}

fn small(value: Option<i64>) -> Option<u32> {
    value.and_then(|value| u32::try_from(value).ok())
}

fn at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Timestamp>> {
    Ok(row.get::<_, Option<i64>>(idx)?.map(Timestamp))
}

fn comment_from_row(row: &Row<'_>) -> rusqlite::Result<CommentRow> {
    let state: Option<String> = text_at(row, 25)?;
    let instruction = match state {
        Some(state) => Some(InstructionColumns {
            state,
            cause: text_at(row, 26)?.unwrap_or_default(),
            estimate: text_at(row, 27)?.unwrap_or_default(),
            estimate_digest: text_at(row, 28)?.unwrap_or_default(),
            target_source: text_at(row, 29)?.unwrap_or_default(),
            target_session_id: text_at(row, 30)?,
            target_client_session: text_at(row, 31)?,
            target_harness: text_at(row, 32)?,
            target_agent: text_at(row, 33)?,
            target_workspace_id: text_at(row, 34)?,
            claimed_client_session: text_at(row, 35)?,
            claimed_workspace_id: text_at(row, 36)?,
            claimed_agent: text_at(row, 37)?,
            claimed_token: text_at(row, 38)?,
            channel: text_at(row, 39)?,
            taken: row.get::<_, i64>(40)? != 0,
            summary: text_at(row, 41)?,
            confirmed_at: at(row, 42)?,
            delivered_at: at(row, 43)?,
            started_at: at(row, 44)?,
            finished_at: at(row, 45)?,
        }),
        None => None,
    };
    Ok(CommentRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        committed: row.get::<_, String>(2)? == "committed",
        change_id: row.get(3)?,
        round: small(row.get(4)?).unwrap_or(0),
        commit_id: row.get(5)?,
        path: text_at(row, 6)?,
        side: text_at(row, 7)?,
        start_line: small(row.get(8)?),
        end_line: small(row.get(9)?),
        reply_to: text_at(row, 10)?,
        kind: row.get(11)?,
        text: row.get(12)?,
        text_sha256: row.get(13)?,
        agent: text_at(row, 14)?,
        actor: row.get(15)?,
        identity_id: text_at(row, 16)?,
        identity_name: text_at(row, 17)?,
        session_id: text_at(row, 18)?,
        remote_token: text_at(row, 19)?,
        remote_token_name: text_at(row, 20)?,
        created_at: Timestamp(row.get(21)?),
        operation_id: text_at(row, 22)?,
        operation_collected: row.get::<_, i64>(23)? != 0,
        recorded_at: at(row, 24)?,
        instruction,
        updated_at: Timestamp(row.get(46)?),
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<CommentEventRow> {
    Ok(CommentEventRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        seq: row.get(2)?,
        committed: row.get::<_, String>(3)? == "committed",
        comment_id: row.get(4)?,
        change_id: row.get(5)?,
        event: row.get(6)?,
        from_state: text_at(row, 7)?,
        to_state: text_at(row, 8)?,
        cause: row.get(9)?,
        transition_id: text_at(row, 10)?,
        agent: text_at(row, 11)?,
        actor: row.get(12)?,
        identity_id: text_at(row, 13)?,
        remote_token: text_at(row, 14)?,
        summary: text_at(row, 15)?,
        claimed_client_session: text_at(row, 16)?,
        claimed_workspace_id: text_at(row, 17)?,
        claimed_agent: text_at(row, 18)?,
        claimed_token: text_at(row, 19)?,
        channel: text_at(row, 20)?,
        taken: row.get::<_, i64>(21)? != 0,
        operation_id: text_at(row, 22)?,
        operation_collected: row.get::<_, i64>(23)? != 0,
        created_at: Timestamp(row.get(24)?),
        recorded_at: at(row, 25)?,
    })
}

fn opt_line(line: Option<u32>) -> Option<i64> {
    line.map(i64::from)
}

/// `column IN (…)` for `list`, appended with its values, or nothing for an
/// empty list.
fn in_clause(sql: &mut String, values: &mut Vec<Value>, column: &str, list: &[String]) {
    if list.is_empty() {
        return;
    }
    let marks = vec!["?"; list.len()].join(", ");
    values.extend(list.iter().map(|item| Value::Text(item.clone())));
    let _ = write!(sql, " AND {column} IN ({marks})");
}

/// The median of `values`, which must not be empty.
fn median(values: &mut [i64]) -> i64 {
    values.sort_unstable();
    let middle = values.len() / 2;
    if values.len().is_multiple_of(2) {
        i64::midpoint(values[middle - 1], values[middle])
    } else {
        values[middle]
    }
}

impl Db {
    /// Writes `comment`, pending, with its creation event `event`.
    pub fn insert_pending_comment(
        &self,
        comment: &CommentRow,
        event: &CommentEventRow,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let i = comment.instruction.as_ref();
            tx.execute(
                &format!(
                    "INSERT INTO comments({COMMENT_COLUMNS}) VALUES (?1, ?2, 'pending', ?3, ?4, ?5,
                     ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21,
                     NULL, 0, NULL, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29, ?30, ?31, NULL, NULL,
                     NULL, NULL, NULL, 0, NULL, ?32, NULL, NULL, NULL, ?21)"
                ),
                params![
                    comment.repo_id,
                    comment.id,
                    comment.change_id,
                    i64::from(comment.round),
                    comment.commit_id,
                    comment.path,
                    comment.side,
                    opt_line(comment.start_line),
                    opt_line(comment.end_line),
                    comment.reply_to,
                    comment.kind,
                    comment.text,
                    comment.text_sha256,
                    comment.agent,
                    comment.actor,
                    comment.identity_id,
                    comment.identity_name,
                    comment.session_id,
                    comment.remote_token,
                    comment.remote_token_name,
                    comment.created_at.0,
                    i.map(|i| i.state.clone()),
                    i.map(|i| i.cause.clone()),
                    i.map(|i| i.estimate.clone()),
                    i.map(|i| i.estimate_digest.clone()),
                    i.map(|i| i.target_source.clone()),
                    i.and_then(|i| i.target_session_id.clone()),
                    i.and_then(|i| i.target_client_session.clone()),
                    i.and_then(|i| i.target_harness.clone()),
                    i.and_then(|i| i.target_agent.clone()),
                    i.and_then(|i| i.target_workspace_id.clone()),
                    i.and_then(|i| i.confirmed_at.map(|at| at.0)),
                ],
            )?;
            insert_event_in(tx, event)?;
            Ok(())
        })
    }

    /// Writes `event`, pending.
    pub fn insert_pending_comment_event(&self, event: &CommentEventRow) -> Result<(), DbError> {
        self.in_transaction(|tx| insert_event_in(tx, event))
    }

    /// Deletes the pending event `id`, and its comment when that is still
    /// pending: its operation was never written.
    pub fn delete_pending_comment_event(&self, repo: RepoId, id: &str) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let comment: Option<String> = tx
                .query_row(
                    "SELECT comment_id FROM comment_events
                     WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
                    params![repo, id],
                    |row| row.get(0),
                )
                .optional()?;
            tx.execute(
                "DELETE FROM comment_events WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
                params![repo, id],
            )?;
            if let Some(comment) = comment {
                tx.execute(
                    "DELETE FROM comments WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
                    params![repo, comment],
                )?;
            }
            Ok(())
        })
    }

    /// Commits the pending event `id` with its operation and end time,
    /// giving it the repository's next `seq`; commits its comment when that
    /// is pending, and applies the event's move to the comment's
    /// instruction columns when the comment is still in the event's `from`
    /// state. Returns the committed event; `None` when no such pending
    /// event exists.
    pub fn commit_comment_event(
        &self,
        repo: RepoId,
        id: &str,
        operation_id: &str,
        recorded_at: Timestamp,
    ) -> Result<Option<CommentEventRow>, DbError> {
        self.in_transaction(|tx| commit_event_in(tx, repo, id, operation_id, recorded_at))
    }

    /// Every pending event, of every repository, oldest first.
    pub fn pending_comment_events(&self) -> Result<Vec<CommentEventRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM comment_events
                 WHERE status = 'pending' ORDER BY created_at, id"
            ))?;
            statement
                .query_map([], event_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// The committed comment `id`.
    pub fn comment(&self, repo: RepoId, id: &str) -> Result<Option<CommentRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COMMENT_COLUMNS} FROM comments
                     WHERE repo_id = ?1 AND id = ?2 AND status = 'committed'"
                ),
                params![repo, id],
                comment_from_row,
            )
            .optional()
        })
    }

    /// The committed comments that reply to `id`, oldest first, at most
    /// `limit`.
    pub fn comment_replies(
        &self,
        repo: RepoId,
        id: &str,
        limit: usize,
    ) -> Result<Vec<CommentRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {COMMENT_COLUMNS} FROM comments
                 WHERE repo_id = ?1 AND reply_to = ?2 AND status = 'committed'
                 ORDER BY created_at, id LIMIT ?3"
            ))?;
            statement
                .query_map(
                    params![repo, id, i64::try_from(limit).unwrap_or(i64::MAX)],
                    comment_from_row,
                )?
                .collect::<Result<_, _>>()
        })
    }

    /// A page of `repo`'s committed comments matching `filter`, oldest
    /// first, after `(created_at, id)`, at most `limit`.
    pub fn comment_page(
        &self,
        repo: RepoId,
        filter: &CommentFilter,
        after: Option<(Timestamp, &str)>,
        limit: usize,
    ) -> Result<Vec<CommentRow>, DbError> {
        let mut sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM comments WHERE repo_id = ? AND status = 'committed'"
        );
        let mut values: Vec<Value> = vec![Value::Text(repo.to_string())];
        if !filter.changes.is_empty() {
            let ors: Vec<&str> = filter.changes.iter().map(|_| "change_id LIKE ?").collect();
            values.extend(
                filter
                    .changes
                    .iter()
                    .map(|prefix| Value::Text(format!("{prefix}%"))),
            );
            let _ = write!(sql, " AND ({})", ors.join(" OR "));
        }
        in_clause(&mut sql, &mut values, "kind", &filter.kinds);
        in_clause(&mut sql, &mut values, "state", &filter.states);
        if let Some(session) = &filter.client_session {
            sql.push_str(" AND (target_client_session = ? OR claimed_client_session = ?)");
            values.push(Value::Text(session.clone()));
            values.push(Value::Text(session.clone()));
        }
        if let Some(after) = filter.created_after {
            sql.push_str(" AND created_at > ?");
            values.push(Value::Integer(after.0));
        }
        if let Some((at, id)) = after {
            sql.push_str(" AND (created_at > ? OR (created_at = ? AND id > ?))");
            values.push(Value::Integer(at.0));
            values.push(Value::Integer(at.0));
            values.push(Value::Text(id.to_owned()));
        }
        sql.push_str(" ORDER BY created_at, id LIMIT ?");
        values.push(Value::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        self.with(|conn| {
            let mut statement = conn.prepare(&sql)?;
            statement
                .query_map(params_from_iter(values), comment_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// How many comments `change` has, pending ones included.
    pub fn comment_count(&self, repo: RepoId, change: &str) -> Result<u64, DbError> {
        self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM comments WHERE repo_id = ?1 AND change_id = ?2",
                params![repo, change],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
    }

    /// How many unfinished instructions `change` has, and how many the
    /// target `session` (a client session, else a provenance session) has.
    pub fn open_instruction_counts(
        &self,
        repo: RepoId,
        change: &str,
        session: Option<&str>,
    ) -> Result<(u64, u64), DbError> {
        self.with(|conn| {
            let open = "state IN ('awaiting_confirmation', 'pending', 'delivered', 'running')";
            let by_change: i64 = conn.query_row(
                &format!(
                    "SELECT count(*) FROM comments
                     WHERE repo_id = ?1 AND change_id = ?2 AND {open}"
                ),
                params![repo, change],
                |row| row.get(0),
            )?;
            let by_session: i64 = match session {
                Some(session) => conn.query_row(
                    &format!(
                        "SELECT count(*) FROM comments WHERE {open}
                         AND (target_client_session = ?1 OR target_session_id = ?1)"
                    ),
                    params![session],
                    |row| row.get(0),
                )?,
                None => 0,
            };
            Ok((
                u64::try_from(by_change).unwrap_or(0),
                u64::try_from(by_session).unwrap_or(0),
            ))
        })
    }

    /// The oldest committed `pending` instruction of `repo` that `query`
    /// matches.
    pub fn claimable_instruction(
        &self,
        repo: RepoId,
        query: &ClaimQuery,
    ) -> Result<Option<CommentRow>, DbError> {
        let mut sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM comments
             WHERE repo_id = ? AND status = 'committed' AND kind = 'instruction'
             AND state = 'pending'"
        );
        let mut values: Vec<Value> = vec![Value::Text(repo.to_string())];
        if let Some(id) = &query.comment_id {
            sql.push_str(" AND id = ?");
            values.push(Value::Text(id.clone()));
        }
        let mut ors: Vec<String> = Vec::new();
        if !query.client_sessions.is_empty() {
            let marks = vec!["?"; query.client_sessions.len()].join(", ");
            ors.push(format!("target_client_session IN ({marks})"));
            values.extend(
                query
                    .client_sessions
                    .iter()
                    .map(|session| Value::Text(session.clone())),
            );
        }
        if let Some(workspace) = &query.workspace_id {
            ors.push("(target_client_session IS NULL AND target_workspace_id = ?)".to_owned());
            values.push(Value::Text(workspace.clone()));
        }
        if let Some(change) = &query.change_id {
            ors.push("(target_source = 'none' AND change_id = ?)".to_owned());
            values.push(Value::Text(change.clone()));
            if query.take {
                ors.push("change_id = ?".to_owned());
                values.push(Value::Text(change.clone()));
            }
        }
        if ors.is_empty() {
            return Ok(None);
        }
        let _ = write!(sql, " AND ({})", ors.join(" OR "));
        sql.push_str(" ORDER BY confirmed_at, created_at, id LIMIT 1");
        self.with(|conn| {
            conn.query_row(&sql, params_from_iter(values), comment_from_row)
                .optional()
        })
    }

    /// `repo`'s committed instructions on `change` in one of `states`.
    pub fn instructions_of_change(
        &self,
        repo: RepoId,
        change: &str,
        states: &[&str],
    ) -> Result<Vec<CommentRow>, DbError> {
        let mut sql = format!(
            "SELECT {COMMENT_COLUMNS} FROM comments
             WHERE repo_id = ? AND change_id = ? AND status = 'committed' AND kind = 'instruction'"
        );
        let mut values = vec![
            Value::Text(repo.to_string()),
            Value::Text(change.to_owned()),
        ];
        let states: Vec<String> = states.iter().map(|state| (*state).to_owned()).collect();
        in_clause(&mut sql, &mut values, "state", &states);
        sql.push_str(" ORDER BY created_at, id");
        self.with(|conn| {
            let mut statement = conn.prepare(&sql)?;
            statement
                .query_map(params_from_iter(values), comment_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// Committed unfinished instructions, of every repository, untouched
    /// since before `before`, oldest first, at most `limit`.
    pub fn stale_instructions(
        &self,
        before: Timestamp,
        limit: usize,
    ) -> Result<Vec<CommentRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {COMMENT_COLUMNS} FROM comments
                 WHERE status = 'committed'
                 AND state IN ('awaiting_confirmation', 'pending', 'delivered', 'running')
                 AND updated_at < ?1 ORDER BY updated_at, id LIMIT ?2"
            ))?;
            statement
                .query_map(
                    params![before.0, i64::try_from(limit).unwrap_or(i64::MAX)],
                    comment_from_row,
                )?
                .collect::<Result<_, _>>()
        })
    }

    /// `repo`'s committed instructions a committed transition made moot,
    /// each with that transition's id: unconfirmed or pending ones on a
    /// change now landed or superseded (`change_final`), and delivered or
    /// running ones on a change proposed again since their delivery
    /// (`reproposed`). What the sweep reconciles, so a transition whose
    /// notice was lost (a restart, a row committed at start) still counts.
    pub fn moot_instructions(
        &self,
        repo: RepoId,
    ) -> Result<Vec<(CommentRow, &'static str, String)>, DbError> {
        let columns = COMMENT_COLUMNS
            .split(',')
            .map(|column| format!("c.{}", column.trim()))
            .collect::<Vec<_>>()
            .join(", ");
        self.with(|conn| {
            let mut out = Vec::new();
            let mut finals = conn.prepare(&format!(
                "SELECT {columns}, l.transition_id FROM comments c
                 JOIN lifecycle l ON l.repo_id = c.repo_id AND l.change_id = c.change_id
                 WHERE c.repo_id = ?1 AND c.status = 'committed' AND c.kind = 'instruction'
                 AND c.state IN ('awaiting_confirmation', 'pending')
                 AND l.state IN ('landed', 'superseded')"
            ))?;
            let rows = finals.query_map([repo], |row| {
                Ok((comment_from_row(row)?, row.get::<_, String>(47)?))
            })?;
            for row in rows {
                let (comment, transition) = row?;
                out.push((comment, "change_final", transition));
            }
            let mut reproposed = conn.prepare(&format!(
                "SELECT {columns}, (SELECT t.id FROM lifecycle_transitions t
                   WHERE t.repo_id = c.repo_id AND t.change_id = c.change_id
                   AND t.status = 'committed' AND t.to_state = 'proposed' AND t.cause = 'request'
                   AND t.recorded_at >= c.delivered_at ORDER BY t.seq LIMIT 1) AS transition
                 FROM comments c
                 WHERE c.repo_id = ?1 AND c.status = 'committed' AND c.kind = 'instruction'
                 AND c.state IN ('delivered', 'running') AND c.delivered_at IS NOT NULL
                 AND transition IS NOT NULL"
            ))?;
            let rows = reproposed.query_map([repo], |row| {
                Ok((comment_from_row(row)?, row.get::<_, String>(47)?))
            })?;
            for row in rows {
                let (comment, transition) = row?;
                out.push((comment, "reproposed", transition));
            }
            Ok(out)
        })
    }

    /// The repositories with an unfinished instruction.
    pub fn repos_with_open_instructions(&self) -> Result<Vec<RepoId>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT repo_id FROM comments WHERE status = 'committed'
                 AND state IN ('awaiting_confirmation', 'pending', 'delivered', 'running')",
            )?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()
        })
    }

    /// The provenance session of `change`'s latest proposal that named
    /// one (R3.09), else of its latest provenance record (R3.04), with
    /// which of the two it was.
    pub fn instruction_target_session(
        &self,
        repo: RepoId,
        change: &str,
    ) -> Result<Option<(String, &'static str)>, DbError> {
        self.with(|conn| {
            let proposal: Option<String> = conn
                .query_row(
                    "SELECT session_id FROM lifecycle_transitions
                     WHERE repo_id = ?1 AND change_id = ?2 AND status = 'committed'
                     AND to_state = 'proposed' AND cause = 'request' AND session_id IS NOT NULL
                     ORDER BY seq DESC LIMIT 1",
                    params![repo, change],
                    |row| row.get(0),
                )
                .optional()?;
            if let Some(session) = proposal {
                return Ok(Some((session, "proposal")));
            }
            let record: Option<String> = conn
                .query_row(
                    "SELECT session_id FROM provenance WHERE repo_id = ?1 AND change_id = ?2
                     ORDER BY recorded_at DESC, sequence DESC LIMIT 1",
                    params![repo, change],
                    |row| row.get(0),
                )
                .optional()?;
            Ok(record.map(|session| (session, "provenance")))
        })
    }

    /// When `client_session` was last seen: the latest of its sessions'
    /// starts and declarations, its provenance records, and its claims.
    pub fn client_session_last_seen(
        &self,
        client_session: &str,
    ) -> Result<Option<Timestamp>, DbError> {
        self.with(|conn| {
            let at: Option<i64> = conn.query_row(
                "SELECT max(at) FROM (
                   SELECT max(max(started_at, coalesce(declared_at, 0))) AS at
                     FROM provenance_sessions WHERE client_session = ?1
                   UNION ALL SELECT max(recorded_at) FROM provenance WHERE client_session = ?1
                   UNION ALL SELECT max(delivered_at) FROM comments
                     WHERE claimed_client_session = ?1)",
                params![client_session],
                |row| row.get(0),
            )?;
            Ok(at.map(Timestamp))
        })
    }

    /// The latest finished run of each check (R3.05) of `repo` on `tree`: how many, their wall
    /// time and their CPU time, in milliseconds.
    pub fn job_usage_on_tree(&self, repo: RepoId, tree: &str) -> Result<(u64, u64, u64), DbError> {
        self.with(|conn| {
            // The latest finished run of each distinct check (by spec), so
            // retries and repeated runs count once: what running them again
            // costs. At most 500 of the newest jobs are looked at.
            let (count, wall, cpu): (i64, i64, i64) = conn.query_row(
                "SELECT count(*), coalesce(sum(wall), 0), coalesce(sum(cpu), 0) FROM (
                   SELECT coalesce(usage_wall_ms, elapsed_ms) AS wall,
                          coalesce(usage_cpu_ms, 0) AS cpu,
                          row_number() OVER (PARTITION BY spec_digest ORDER BY finished_at DESC)
                            AS newest
                   FROM (SELECT * FROM jobs WHERE repo_id = ?1 AND tree_id = ?2
                         AND finished_at IS NOT NULL AND state = 'completed'
                         ORDER BY finished_at DESC LIMIT 500))
                 WHERE newest = 1",
                params![repo, tree],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
            let positive = |value: i64| u64::try_from(value).unwrap_or(0);
            Ok((positive(count), positive(wall), positive(cpu)))
        })
    }

    /// The median wall time from delivery to done of the latest `limit`
    /// instructions that finished DONE: on `change`, else to
    /// `client_session`, else in `repo`; with how many and which basis.
    pub fn instruction_agent_time(
        &self,
        repo: RepoId,
        change: &str,
        client_session: Option<&str>,
        limit: usize,
    ) -> Result<Option<(u64, u32, &'static str)>, DbError> {
        let limit = i64::try_from(limit).unwrap_or(i64::MAX);
        self.with(|conn| {
            let base = "SELECT finished_at - delivered_at FROM comments
                 WHERE kind = 'instruction' AND state = 'done' AND status = 'committed'
                 AND delivered_at IS NOT NULL AND finished_at IS NOT NULL";
            let collect = |sql: &str, values: Vec<Value>| -> rusqlite::Result<Vec<i64>> {
                let mut statement = conn.prepare(sql)?;
                statement
                    .query_map(params_from_iter(values), |row| row.get::<_, i64>(0))?
                    .collect()
            };
            let mut tries: Vec<(String, Vec<Value>, &'static str)> = vec![(
                format!(
                    "{base} AND repo_id = ? AND change_id = ? ORDER BY finished_at DESC LIMIT ?"
                ),
                vec![
                    Value::Text(repo.to_string()),
                    Value::Text(change.to_owned()),
                    Value::Integer(limit),
                ],
                "change",
            )];
            if let Some(session) = client_session {
                tries.push((
                    format!(
                        "{base} AND (target_client_session = ? OR claimed_client_session = ?)
                         ORDER BY finished_at DESC LIMIT ?"
                    ),
                    vec![
                        Value::Text(session.to_owned()),
                        Value::Text(session.to_owned()),
                        Value::Integer(limit),
                    ],
                    "session",
                ));
            }
            tries.push((
                format!("{base} AND repo_id = ? ORDER BY finished_at DESC LIMIT ?"),
                vec![Value::Text(repo.to_string()), Value::Integer(limit)],
                "repository",
            ));
            for (sql, values, basis) in tries {
                let mut times = collect(&sql, values)?;
                times.retain(|time| *time >= 0);
                if !times.is_empty() {
                    let samples = u32::try_from(times.len()).unwrap_or(u32::MAX);
                    let middle = u64::try_from(median(&mut times)).unwrap_or(0);
                    return Ok(Some((middle, samples, basis)));
                }
            }
            Ok(None)
        })
    }

    /// Maps the operation ids of `repo`'s committed comments and events
    /// through GC's rewrite (old id to new), and marks the ones it dropped
    /// collected. Returns how many were mapped and how many marked.
    pub fn map_comment_operations(
        &self,
        repo: RepoId,
        rewritten: &HashMap<String, String>,
    ) -> Result<(usize, usize), DbError> {
        self.in_transaction(|tx| {
            let (mut mapped, mut collected) = (0, 0);
            for table in ["comments", "comment_events"] {
                let rows: Vec<(String, String)> = {
                    let mut statement = tx.prepare(&format!(
                        "SELECT id, operation_id FROM {table}
                         WHERE repo_id = ?1 AND status = 'committed' AND operation_collected = 0
                         AND operation_id IS NOT NULL"
                    ))?;
                    statement
                        .query_map([repo], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<Result<_, _>>()?
                };
                for (id, old) in rows {
                    if let Some(new) = rewritten.get(&old) {
                        tx.execute(
                            &format!(
                                "UPDATE {table} SET operation_id = ?3 WHERE repo_id = ?1 AND id = ?2"
                            ),
                            params![repo, id, new],
                        )?;
                        mapped += 1;
                    } else {
                        tx.execute(
                            &format!(
                                "UPDATE {table} SET operation_collected = 1
                                 WHERE repo_id = ?1 AND id = ?2"
                            ),
                            params![repo, id],
                        )?;
                        collected += 1;
                    }
                }
            }
            Ok((mapped, collected))
        })
    }

    /// The committed events of `comment`, in order.
    pub fn comment_events(
        &self,
        repo: RepoId,
        comment: &str,
    ) -> Result<Vec<CommentEventRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM comment_events
                 WHERE repo_id = ?1 AND comment_id = ?2 AND status = 'committed' ORDER BY seq"
            ))?;
            statement
                .query_map(params![repo, comment], event_from_row)?
                .collect::<Result<_, _>>()
        })
    }
}

fn insert_event_in(tx: &Transaction<'_>, event: &CommentEventRow) -> Result<(), DbError> {
    tx.execute(
        &format!(
            "INSERT INTO comment_events({EVENT_COLUMNS}) VALUES (?1, ?2, NULL, 'pending', ?3, ?4,
             ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18, ?19, ?20, NULL, 0,
             ?21, NULL)"
        ),
        params![
            event.repo_id,
            event.id,
            event.comment_id,
            event.change_id,
            event.event,
            event.from_state,
            event.to_state,
            event.cause,
            event.transition_id,
            event.agent,
            event.actor,
            event.identity_id,
            event.remote_token,
            event.summary,
            event.claimed_client_session,
            event.claimed_workspace_id,
            event.claimed_agent,
            event.claimed_token,
            event.channel,
            i64::from(event.taken),
            event.created_at.0,
        ],
    )?;
    Ok(())
}

/// [`Db::commit_comment_event`] inside `tx`.
fn commit_event_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    id: &str,
    operation_id: &str,
    recorded_at: Timestamp,
) -> Result<Option<CommentEventRow>, DbError> {
    let event = tx
        .query_row(
            &format!(
                "SELECT {EVENT_COLUMNS} FROM comment_events
                 WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'"
            ),
            params![repo, id],
            event_from_row,
        )
        .optional()?;
    let Some(event) = event else {
        return Ok(None);
    };
    let seq: i64 = tx.query_row(
        "SELECT coalesce(max(seq), 0) + 1 FROM comment_events WHERE repo_id = ?1",
        params![repo],
        |row| row.get(0),
    )?;
    tx.execute(
        "UPDATE comment_events SET status = 'committed', seq = ?3, operation_id = ?4,
         recorded_at = ?5 WHERE repo_id = ?1 AND id = ?2",
        params![repo, id, seq, operation_id, recorded_at.0],
    )?;
    let created = event.from_state.is_none();
    if created {
        tx.execute(
            "UPDATE comments SET status = 'committed', operation_id = ?3, recorded_at = ?4,
             updated_at = ?4,
             confirmed_at = CASE WHEN state = 'pending' THEN ?4 ELSE confirmed_at END
             WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
            params![repo, event.comment_id, operation_id, recorded_at.0],
        )?;
    } else if let (Some(from), Some(to)) = (&event.from_state, &event.to_state) {
        // A move applies only while the comment is in its `from` state: an
        // event committed late, at start, after another moved it, is kept
        // for the record and changes nothing.
        let at = recorded_at.0;
        let finished = matches!(to.as_str(), "done" | "failed" | "cancelled");
        let claim = event.event == "claim";
        let release = event.event == "release";
        tx.execute(
            "UPDATE comments SET state = ?4, cause = ?5, updated_at = ?6,
               summary = coalesce(?7, summary),
               confirmed_at = CASE WHEN ?3 = 'awaiting_confirmation' AND ?4 = 'pending'
                                   THEN ?6 ELSE confirmed_at END,
               delivered_at = CASE WHEN ?8 THEN ?6 WHEN ?9 THEN NULL ELSE delivered_at END,
               started_at = CASE WHEN ?4 = 'running' THEN ?6 ELSE started_at END,
               finished_at = CASE WHEN ?10 THEN ?6 ELSE finished_at END,
               claimed_client_session = CASE WHEN ?8 THEN ?11 WHEN ?9 THEN NULL
                                             ELSE claimed_client_session END,
               claimed_workspace_id = CASE WHEN ?8 THEN ?12 WHEN ?9 THEN NULL
                                           ELSE claimed_workspace_id END,
               claimed_agent = CASE WHEN ?8 THEN ?13 WHEN ?9 THEN NULL ELSE claimed_agent END,
               claimed_token = CASE WHEN ?8 THEN ?14 WHEN ?9 THEN NULL ELSE claimed_token END,
               channel = CASE WHEN ?8 THEN ?15 WHEN ?9 THEN NULL ELSE channel END,
               taken = CASE WHEN ?8 THEN ?16 WHEN ?9 THEN 0 ELSE taken END
             WHERE repo_id = ?1 AND id = ?2 AND state = ?3",
            params![
                repo,
                event.comment_id,
                from,
                to,
                event.cause,
                at,
                event.summary,
                claim,
                release,
                finished,
                event.claimed_client_session,
                event.claimed_workspace_id,
                event.claimed_agent,
                event.claimed_token,
                event.channel,
                i64::from(event.taken),
            ],
        )?;
    }
    Ok(Some(CommentEventRow {
        seq: Some(seq),
        committed: true,
        operation_id: Some(operation_id.to_owned()),
        recorded_at: Some(recorded_at),
        ..event
    }))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn medians() {
        assert_eq!(median(&mut [5]), 5);
        assert_eq!(median(&mut [9, 1, 5]), 5);
        assert_eq!(median(&mut [4, 2]), 3);
    }
}
