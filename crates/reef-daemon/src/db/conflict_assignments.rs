//! Queries on `conflict_assignments` and `conflict_assignment_events`
//! (R3.27, ADR 0068).
//!
//! An assignment row is one change's record that it introduces a conflict
//! someone must resolve; at most one per (repository, change, kind) is open
//! (`closed_at` NULL). Every change of a row is written with an event in
//! the same transaction ([`Db::record_assignments`]), the event taking the
//! repository's next `seq`. Both tables are per repository and deleted with
//! it ([`Db::delete_repository`]); GC deletes closed assignments, with their
//! events, after their retention ([`Db::delete_closed_assignments`]).

use std::fmt::Write as _;

use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::evidence::{integer_at, text_at};
use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// Who owns an assignment, as its row holds it: a provenance session and
/// its workspace, or (for a person's takeover) only an identity.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct OwnerColumns {
    /// The provenance session; `None` for a person.
    pub session_id: Option<String>,
    /// Its workspace.
    pub workspace_id: Option<String>,
    /// That workspace's owner text.
    pub workspace_owner: Option<String>,
    /// The operator identity.
    pub identity_id: Option<String>,
    /// `human`, `agent` or `ci`.
    pub actor: Option<String>,
    /// The agent label.
    pub agent: Option<String>,
    /// The declared client session.
    pub client_session: Option<String>,
    /// When the session ended, as last seen.
    pub ended_at: Option<Timestamp>,
}

impl OwnerColumns {
    /// Whether nobody is named.
    pub fn is_empty(&self) -> bool {
        self.session_id.is_none() && self.identity_id.is_none()
    }
}

/// One row of `conflict_assignments`. States, reasons and kinds are their
/// column texts (`crate::assign`'s names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `asg_` and 16 hex digits.
    pub id: String,
    /// The change, in hex.
    pub change_id: String,
    /// `tree`.
    pub kind: String,
    /// The conflicted commit, as last looked at.
    pub commit_id: String,
    /// The conflict's id, as last looked at.
    pub conflict_id: String,
    /// The conflict's id when the row opened.
    pub first_conflict_id: String,
    /// Conflicted files the change introduces.
    pub introduced: u32,
    /// Conflicted files in all.
    pub files_total: u32,
    /// `assigned`, `queued`, `taken`, `unowned` or `closed`.
    pub state: String,
    /// Why.
    pub reason: String,
    /// Who owns it.
    pub owner: OwnerColumns,
    /// -100 to 100.
    pub priority: i32,
    /// When it last entered the queue.
    pub queued_at: Option<Timestamp>,
    /// Claims made on it.
    pub claims: u32,
    /// Events it has.
    pub events: u32,
    /// When it opened.
    pub opened_at: Timestamp,
    /// When it last changed.
    pub updated_at: Timestamp,
    /// When its conflict id last changed.
    pub conflict_changed_at: Timestamp,
    /// When it closed.
    pub closed_at: Option<Timestamp>,
    /// The change's commit when it closed.
    pub closed_commit_id: Option<String>,
    /// The session whose record names `closed_commit_id`.
    pub resolved_session_id: Option<String>,
}

/// One row of `conflict_assignment_events`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct AssignmentEventRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `cae_` and 16 hex digits.
    pub id: String,
    /// Its place in the repository's events; set when recorded.
    pub seq: i64,
    /// The assignment.
    pub assignment_id: String,
    /// The change.
    pub change_id: String,
    /// The conflict id then.
    pub conflict_id: String,
    /// The state before; `None` when the assignment opened.
    pub from_state: Option<String>,
    /// The state after.
    pub to_state: String,
    /// Why.
    pub reason: String,
    /// The owner afterwards.
    pub owner: OwnerColumns,
    /// The owner's workspace before, when the event moved it away.
    pub from_workspace_id: Option<String>,
    /// The priority afterwards.
    pub priority: i32,
    /// The caller's reason.
    pub note: Option<String>,
    /// `human`, `agent` or `daemon`.
    pub by_actor: String,
    /// The caller's agent.
    pub by_agent: Option<String>,
    /// The caller's identity.
    pub by_identity_id: Option<String>,
    /// The caller's provenance session.
    pub by_session_id: Option<String>,
    /// The remote token the call came through.
    pub by_remote_token: Option<String>,
    /// When.
    pub at: Timestamp,
}

/// Which assignments a listing returns: the fields AND.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct AssignmentFilter {
    /// Only these states; empty for every open one.
    pub states: Vec<String>,
    /// Only those this workspace owns.
    pub workspace_id: Option<String>,
    /// Only these changes, by hex prefix.
    pub change_prefixes: Vec<String>,
    /// Only those whose owner declared this agent.
    pub agent: Option<String>,
    /// Only those whose owner has this identity id.
    pub identity_id: Option<String>,
}

/// The states that are open.
pub const OPEN_ASSIGNMENT_STATES: [&str; 4] = ["assigned", "queued", "taken", "unowned"];

const COLUMNS: &str = "repo_id, id, change_id, kind, commit_id, conflict_id, first_conflict_id, \
     introduced, files_total, state, reason, owner_session_id, owner_workspace_id, \
     owner_workspace_owner, owner_identity_id, owner_actor, owner_agent, owner_client_session, \
     owner_ended_at, priority, queued_at, claims, events, opened_at, updated_at, \
     conflict_changed_at, closed_at, closed_commit_id, resolved_session_id";

const EVENT_COLUMNS: &str = "repo_id, id, seq, assignment_id, change_id, conflict_id, from_state, \
     to_state, reason, owner_session_id, owner_workspace_id, owner_workspace_owner, \
     owner_identity_id, owner_actor, owner_agent, owner_client_session, owner_ended_at, \
     from_workspace_id, priority, note, by_actor, by_agent, by_identity_id, by_session_id, \
     by_remote_token, at";

fn time_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<Option<Timestamp>> {
    Ok(match row.get::<_, rusqlite::types::Value>(idx)? {
        rusqlite::types::Value::Integer(n) => Some(Timestamp(n)),
        _ => None,
    })
}

fn u32_at(row: &Row<'_>, idx: usize) -> rusqlite::Result<u32> {
    Ok(u32::try_from(integer_at(row, idx)?.max(0)).unwrap_or(u32::MAX))
}

fn owner_at(row: &Row<'_>, first: usize) -> rusqlite::Result<OwnerColumns> {
    Ok(OwnerColumns {
        session_id: text_at(row, first)?,
        workspace_id: text_at(row, first + 1)?,
        workspace_owner: text_at(row, first + 2)?,
        identity_id: text_at(row, first + 3)?,
        actor: text_at(row, first + 4)?,
        agent: text_at(row, first + 5)?,
        client_session: text_at(row, first + 6)?,
        ended_at: time_at(row, first + 7)?,
    })
}

fn from_row(row: &Row<'_>) -> rusqlite::Result<AssignmentRow> {
    Ok(AssignmentRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        change_id: row.get(2)?,
        kind: row.get(3)?,
        commit_id: row.get(4)?,
        conflict_id: row.get(5)?,
        first_conflict_id: row.get(6)?,
        introduced: u32_at(row, 7)?,
        files_total: u32_at(row, 8)?,
        state: row.get(9)?,
        reason: row.get(10)?,
        owner: owner_at(row, 11)?,
        priority: i32::try_from(integer_at(row, 19)?).unwrap_or(0),
        queued_at: time_at(row, 20)?,
        claims: u32_at(row, 21)?,
        events: u32_at(row, 22)?,
        opened_at: Timestamp(integer_at(row, 23)?),
        updated_at: Timestamp(integer_at(row, 24)?),
        conflict_changed_at: Timestamp(integer_at(row, 25)?),
        closed_at: time_at(row, 26)?,
        closed_commit_id: text_at(row, 27)?,
        resolved_session_id: text_at(row, 28)?,
    })
}

fn event_from_row(row: &Row<'_>) -> rusqlite::Result<AssignmentEventRow> {
    Ok(AssignmentEventRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        seq: integer_at(row, 2)?,
        assignment_id: row.get(3)?,
        change_id: row.get(4)?,
        conflict_id: row.get(5)?,
        from_state: text_at(row, 6)?,
        to_state: row.get(7)?,
        reason: row.get(8)?,
        owner: owner_at(row, 9)?,
        from_workspace_id: text_at(row, 17)?,
        priority: i32::try_from(integer_at(row, 18)?).unwrap_or(0),
        note: text_at(row, 19)?,
        by_actor: row.get(20)?,
        by_agent: text_at(row, 21)?,
        by_identity_id: text_at(row, 22)?,
        by_session_id: text_at(row, 23)?,
        by_remote_token: text_at(row, 24)?,
        at: Timestamp(integer_at(row, 25)?),
    })
}

/// Writes `row`, inserting it or updating the row with its id. A second
/// open row for the same change and kind fails on the unique index rather
/// than replacing it.
fn upsert_in(tx: &Transaction<'_>, row: &AssignmentRow) -> rusqlite::Result<()> {
    let o = &row.owner;
    tx.execute(
        &format!(
            "INSERT INTO conflict_assignments({COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26, ?27, ?28, ?29)
             ON CONFLICT(repo_id, id) DO UPDATE SET
               commit_id = excluded.commit_id, conflict_id = excluded.conflict_id,
               introduced = excluded.introduced, files_total = excluded.files_total,
               state = excluded.state, reason = excluded.reason,
               owner_session_id = excluded.owner_session_id,
               owner_workspace_id = excluded.owner_workspace_id,
               owner_workspace_owner = excluded.owner_workspace_owner,
               owner_identity_id = excluded.owner_identity_id,
               owner_actor = excluded.owner_actor, owner_agent = excluded.owner_agent,
               owner_client_session = excluded.owner_client_session,
               owner_ended_at = excluded.owner_ended_at, priority = excluded.priority,
               queued_at = excluded.queued_at, claims = excluded.claims, events = excluded.events,
               updated_at = excluded.updated_at, conflict_changed_at = excluded.conflict_changed_at,
               closed_at = excluded.closed_at, closed_commit_id = excluded.closed_commit_id,
               resolved_session_id = excluded.resolved_session_id"
        ),
        params![
            row.repo_id,
            row.id,
            row.change_id,
            row.kind,
            row.commit_id,
            row.conflict_id,
            row.first_conflict_id,
            row.introduced,
            row.files_total,
            row.state,
            row.reason,
            o.session_id,
            o.workspace_id,
            o.workspace_owner,
            o.identity_id,
            o.actor,
            o.agent,
            o.client_session,
            o.ended_at,
            row.priority,
            row.queued_at,
            row.claims,
            row.events,
            row.opened_at,
            row.updated_at,
            row.conflict_changed_at,
            row.closed_at,
            row.closed_commit_id,
            row.resolved_session_id,
        ],
    )?;
    Ok(())
}

fn insert_event_in(tx: &Transaction<'_>, event: &AssignmentEventRow) -> rusqlite::Result<()> {
    let o = &event.owner;
    tx.execute(
        &format!(
            "INSERT INTO conflict_assignment_events({EVENT_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17,
                     ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)"
        ),
        params![
            event.repo_id,
            event.id,
            event.seq,
            event.assignment_id,
            event.change_id,
            event.conflict_id,
            event.from_state,
            event.to_state,
            event.reason,
            o.session_id,
            o.workspace_id,
            o.workspace_owner,
            o.identity_id,
            o.actor,
            o.agent,
            o.client_session,
            o.ended_at,
            event.from_workspace_id,
            event.priority,
            event.note,
            event.by_actor,
            event.by_agent,
            event.by_identity_id,
            event.by_session_id,
            event.by_remote_token,
            event.at,
        ],
    )?;
    Ok(())
}

/// The `WHERE` clause and parameters of `filter` for `repo`.
fn filter_sql(repo: RepoId, filter: &AssignmentFilter) -> (String, Vec<rusqlite::types::Value>) {
    use rusqlite::types::Value;
    let mut sql = String::from("repo_id = ?");
    let mut values = vec![Value::Text(repo.as_hex())];
    let states: Vec<String> = if filter.states.is_empty() {
        OPEN_ASSIGNMENT_STATES
            .iter()
            .map(|s| (*s).to_owned())
            .collect()
    } else {
        filter.states.clone()
    };
    let _ = write!(
        sql,
        " AND state IN ({})",
        vec!["?"; states.len()].join(", ")
    );
    values.extend(states.into_iter().map(Value::Text));
    if let Some(workspace) = &filter.workspace_id {
        sql.push_str(" AND owner_workspace_id = ? AND state = 'assigned'");
        values.push(Value::Text(workspace.clone()));
    }
    if !filter.change_prefixes.is_empty() {
        let any: Vec<&str> = filter
            .change_prefixes
            .iter()
            .map(|_| "substr(change_id, 1, ?) = ?")
            .collect();
        let _ = write!(sql, " AND ({})", any.join(" OR "));
        for prefix in &filter.change_prefixes {
            values.push(Value::Integer(i64::try_from(prefix.len()).unwrap_or(0)));
            values.push(Value::Text(prefix.clone()));
        }
    }
    if let Some(agent) = &filter.agent {
        sql.push_str(" AND owner_agent = ?");
        values.push(Value::Text(agent.clone()));
    }
    if let Some(identity) = &filter.identity_id {
        sql.push_str(" AND owner_identity_id = ?");
        values.push(Value::Text(identity.clone()));
    }
    (sql, values)
}

/// The `meta` key of `repo`'s last event seq.
pub(crate) fn seq_key(repo: RepoId) -> String {
    format!("assignment_seq:{}", repo.as_hex())
}

/// The order listings use: the queue first, in its order, then the rest,
/// newest update first.
const LIST_ORDER: &str = "ORDER BY CASE WHEN state = 'queued' THEN 0 ELSE 1 END,
     CASE WHEN state = 'queued' THEN -priority ELSE 0 END,
     CASE WHEN state = 'queued' THEN queued_at ELSE -updated_at END, id";

impl Db {
    /// Writes each `(row, event)` in one transaction, each event taking the
    /// repository's next `seq` (set in the returned events).
    pub fn record_assignments(
        &self,
        repo: RepoId,
        changes: Vec<(AssignmentRow, AssignmentEventRow)>,
    ) -> Result<Vec<AssignmentEventRow>, DbError> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        self.in_transaction(|tx| {
            // A repository forgotten while a look ran: nothing to keep.
            let exists = tx
                .query_row("SELECT 1 FROM repositories WHERE id = ?1", [repo], |_| {
                    Ok(())
                })
                .optional()?
                .is_some();
            if !exists {
                return Ok(Vec::new());
            }
            // The repository's last seq, kept in `meta` so that retention,
            // which deletes old events, never lets it go back.
            let key = seq_key(repo);
            let kept: Option<String> = tx
                .query_row("SELECT value FROM meta WHERE key = ?1", [&key], |row| {
                    row.get(0)
                })
                .optional()?;
            let stored: i64 = tx.query_row(
                "SELECT coalesce(max(seq), 0) FROM conflict_assignment_events WHERE repo_id = ?1",
                [repo],
                |row| row.get(0),
            )?;
            let mut seq = kept
                .and_then(|value| value.parse::<i64>().ok())
                .unwrap_or(0)
                .max(stored);
            let mut recorded = Vec::with_capacity(changes.len());
            for (row, mut event) in changes {
                upsert_in(tx, &row)?;
                seq += 1;
                event.seq = seq;
                insert_event_in(tx, &event)?;
                recorded.push(event);
            }
            tx.execute(
                "INSERT INTO meta(key, value) VALUES (?1, ?2)
                 ON CONFLICT(key) DO UPDATE SET value = excluded.value",
                params![key, seq.to_string()],
            )?;
            Ok(recorded)
        })
    }

    /// Writes `row` with no event: what the follower updates quietly (the
    /// commit a conflict is in, an owner's restarted session, when an
    /// owner's session ended).
    pub fn save_assignment(&self, row: &AssignmentRow) -> Result<(), DbError> {
        self.in_transaction(|tx| Ok(upsert_in(tx, row)?))
    }

    /// The open assignments of `repo`, oldest first.
    pub fn open_assignments(&self, repo: RepoId) -> Result<Vec<AssignmentRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM conflict_assignments
                 WHERE repo_id = ?1 AND closed_at IS NULL ORDER BY opened_at, id"
            ))?;
            stmt.query_map([repo], from_row)?.collect()
        })
    }

    /// How many assignments of `repo` are open.
    pub fn open_assignment_count(&self, repo: RepoId) -> Result<u64, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT count(*) FROM conflict_assignments WHERE repo_id = ?1 AND closed_at IS NULL",
                [repo],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(|n| u64::try_from(n).unwrap_or(0))
    }

    /// The assignment `id` of `repo`.
    pub fn assignment(&self, repo: RepoId, id: &str) -> Result<Option<AssignmentRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM conflict_assignments WHERE repo_id = ?1 AND id = ?2"
                ),
                params![repo, id],
                from_row,
            )
            .optional()
        })
    }

    /// `change`'s open assignment, else its most recently closed one.
    pub fn assignment_of_change(
        &self,
        repo: RepoId,
        change: &str,
    ) -> Result<Option<AssignmentRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM conflict_assignments WHERE repo_id = ?1 AND change_id = ?2
                     ORDER BY closed_at IS NOT NULL, closed_at DESC, opened_at DESC LIMIT 1"
                ),
                params![repo, change],
                from_row,
            )
            .optional()
        })
    }

    /// The open assignments of these changes, any kind, by change.
    pub fn open_assignments_of_changes(
        &self,
        repo: RepoId,
        changes: &[String],
    ) -> Result<Vec<AssignmentRow>, DbError> {
        if changes.is_empty() {
            return Ok(Vec::new());
        }
        let sql = format!(
            "SELECT {COLUMNS} FROM conflict_assignments
             WHERE repo_id = ? AND closed_at IS NULL AND change_id IN ({})",
            vec!["?"; changes.len()].join(", ")
        );
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            let mut values = vec![rusqlite::types::Value::Text(repo.as_hex())];
            values.extend(changes.iter().cloned().map(rusqlite::types::Value::Text));
            stmt.query_map(params_from_iter(values), from_row)?
                .collect()
        })
    }

    /// The newest assignment of `change` and `kind` that closed at or after
    /// `since` with conflict `conflict_id`: one to reopen.
    pub fn reopenable_assignment(
        &self,
        repo: RepoId,
        change: &str,
        kind: &str,
        conflict_id: &str,
        since: Timestamp,
    ) -> Result<Option<AssignmentRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM conflict_assignments
                     WHERE repo_id = ?1 AND change_id = ?2 AND kind = ?3 AND conflict_id = ?4
                       AND closed_at IS NOT NULL AND closed_at >= ?5
                     ORDER BY closed_at DESC LIMIT 1"
                ),
                params![repo, change, kind, conflict_id, since],
                from_row,
            )
            .optional()
        })
    }

    /// The head of `repo`'s resolver queue.
    pub fn queue_head(&self, repo: RepoId) -> Result<Option<AssignmentRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {COLUMNS} FROM conflict_assignments
                     WHERE repo_id = ?1 AND state = 'queued'
                     ORDER BY priority DESC, queued_at, id LIMIT 1"
                ),
                [repo],
                from_row,
            )
            .optional()
        })
    }

    /// Up to `limit` assignments of `repo` that `filter` keeps, after the
    /// first `offset`, queued first in queue order, then by update, newest
    /// first; and how many queued assignments come before the first one
    /// returned (for queue positions).
    pub fn list_assignments(
        &self,
        repo: RepoId,
        filter: &AssignmentFilter,
        offset: usize,
        limit: usize,
    ) -> Result<Vec<AssignmentRow>, DbError> {
        let (condition, mut values) = filter_sql(repo, filter);
        values.push(rusqlite::types::Value::Integer(
            i64::try_from(limit).unwrap_or(i64::MAX),
        ));
        values.push(rusqlite::types::Value::Integer(
            i64::try_from(offset).unwrap_or(i64::MAX),
        ));
        let sql = format!(
            "SELECT {COLUMNS} FROM conflict_assignments WHERE {condition} {LIST_ORDER} LIMIT ? OFFSET ?"
        );
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(values), from_row)?
                .collect()
        })
    }

    /// The place of the queued assignment `id` in `repo`'s queue, from 1.
    pub fn queue_position(&self, repo: RepoId, row: &AssignmentRow) -> Result<u32, DbError> {
        let queued_at = row.queued_at.unwrap_or(Timestamp(0));
        self.with(|conn| {
            conn.query_row(
                "SELECT count(*) FROM conflict_assignments
                 WHERE repo_id = ?1 AND state = 'queued' AND (priority > ?2
                   OR (priority = ?2 AND (queued_at < ?3 OR (queued_at = ?3 AND id < ?4))))",
                params![repo, row.priority, queued_at, row.id],
                |r| r.get::<_, i64>(0),
            )
        })
        .map(|before| u32::try_from(before + 1).unwrap_or(u32::MAX))
    }

    /// The open assignments `workspace_id`'s session owns, newest update
    /// first, at most `limit`, and how many there are.
    pub fn assignments_owned_by(
        &self,
        workspace_id: &str,
        limit: usize,
    ) -> Result<(Vec<AssignmentRow>, u64), DbError> {
        self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT count(*) FROM conflict_assignments
                 WHERE owner_workspace_id = ?1 AND closed_at IS NULL AND state = 'assigned'",
                [workspace_id],
                |row| row.get(0),
            )?;
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM conflict_assignments
                 WHERE owner_workspace_id = ?1 AND closed_at IS NULL AND state = 'assigned'
                 ORDER BY updated_at DESC, id LIMIT ?2"
            ))?;
            let rows = stmt
                .query_map(
                    params![workspace_id, i64::try_from(limit).unwrap_or(i64::MAX)],
                    from_row,
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok((rows, u64::try_from(count).unwrap_or(0)))
        })
    }

    /// Up to `limit` events of assignment `id` after `after_seq`, oldest
    /// first.
    pub fn assignment_events(
        &self,
        repo: RepoId,
        id: &str,
        after_seq: i64,
        limit: usize,
    ) -> Result<Vec<AssignmentEventRow>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {EVENT_COLUMNS} FROM conflict_assignment_events
                 WHERE repo_id = ?1 AND assignment_id = ?2 AND seq > ?3 ORDER BY seq LIMIT ?4"
            ))?;
            stmt.query_map(
                params![
                    repo,
                    id,
                    after_seq,
                    i64::try_from(limit).unwrap_or(i64::MAX)
                ],
                event_from_row,
            )?
            .collect()
        })
    }

    /// Deletes `repo`'s assignments closed before `before`, with their
    /// events; how many assignments.
    pub fn delete_closed_assignments(
        &self,
        repo: RepoId,
        before: Timestamp,
    ) -> Result<u64, DbError> {
        self.in_transaction(|tx| {
            tx.execute(
                "DELETE FROM conflict_assignment_events WHERE repo_id = ?1 AND assignment_id IN
                   (SELECT id FROM conflict_assignments
                    WHERE repo_id = ?1 AND closed_at IS NOT NULL AND closed_at < ?2)",
                params![repo, before],
            )?;
            let deleted = tx.execute(
                "DELETE FROM conflict_assignments
                 WHERE repo_id = ?1 AND closed_at IS NOT NULL AND closed_at < ?2",
                params![repo, before],
            )?;
            Ok(u64::try_from(deleted).unwrap_or(0))
        })
    }

    /// Moves `id`'s close time to `at` (tests of retention).
    #[doc(hidden)]
    pub fn age_assignment(&self, repo: RepoId, id: &str, at: Timestamp) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE conflict_assignments SET closed_at = ?3 WHERE repo_id = ?1 AND id = ?2",
                params![repo, id, at],
            )
        })?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::tests::repo;

    fn row(
        repo: RepoId,
        id: &str,
        change: &str,
        state: &str,
        priority: i32,
        queued: i64,
    ) -> AssignmentRow {
        AssignmentRow {
            repo_id: repo,
            id: id.into(),
            change_id: change.into(),
            kind: "tree".into(),
            commit_id: "c".into(),
            conflict_id: "cfl_1".into(),
            first_conflict_id: "cfl_1".into(),
            introduced: 1,
            files_total: 1,
            state: state.into(),
            reason: "default".into(),
            owner: OwnerColumns::default(),
            priority,
            queued_at: (state == "queued").then_some(Timestamp(queued)),
            claims: 0,
            events: 1,
            opened_at: Timestamp(1),
            updated_at: Timestamp(queued),
            conflict_changed_at: Timestamp(1),
            closed_at: None,
            closed_commit_id: None,
            resolved_session_id: None,
        }
    }

    fn event(row: &AssignmentRow) -> AssignmentEventRow {
        AssignmentEventRow {
            repo_id: row.repo_id,
            id: format!("cae_{}_{}", row.id, row.state),
            seq: 0,
            assignment_id: row.id.clone(),
            change_id: row.change_id.clone(),
            conflict_id: row.conflict_id.clone(),
            from_state: None,
            to_state: row.state.clone(),
            reason: row.reason.clone(),
            owner: row.owner.clone(),
            from_workspace_id: None,
            priority: row.priority,
            note: None,
            by_actor: "daemon".into(),
            by_agent: None,
            by_identity_id: None,
            by_session_id: None,
            by_remote_token: None,
            at: row.updated_at,
        }
    }

    #[test]
    fn queue_order_and_positions() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("a3dda866c7e8f343", "/r");
        db.insert_repository(&record).unwrap();
        let repo = record.id;
        let rows = [
            row(repo, "asg_a", "aa", "queued", 0, 10),
            row(repo, "asg_b", "bb", "queued", 5, 20),
            row(repo, "asg_c", "cc", "assigned", 0, 30),
            row(repo, "asg_d", "dd", "queued", 0, 5),
        ];
        let events = db
            .record_assignments(repo, rows.iter().map(|r| (r.clone(), event(r))).collect())
            .unwrap();
        assert_eq!(
            events.iter().map(|e| e.seq).collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        let listed = db
            .list_assignments(repo, &AssignmentFilter::default(), 0, 10)
            .unwrap();
        let ids: Vec<&str> = listed.iter().map(|r| r.id.as_str()).collect();
        assert_eq!(ids, ["asg_b", "asg_d", "asg_a", "asg_c"]);
        assert_eq!(db.queue_head(repo).unwrap().unwrap().id, "asg_b");
        assert_eq!(db.queue_position(repo, &rows[0]).unwrap(), 3);
        // A second open row for one change fails, never replaces.
        let twin = row(repo, "asg_e", "aa", "queued", 0, 1);
        assert!(
            db.record_assignments(repo, vec![(twin.clone(), event(&twin))])
                .is_err()
        );
        assert_eq!(db.open_assignment_count(repo).unwrap(), 4);
        // Closing and retention.
        let mut closed = rows[0].clone();
        closed.state = "closed".into();
        closed.closed_at = Some(Timestamp(100));
        db.record_assignments(repo, vec![(closed.clone(), event(&closed))])
            .unwrap();
        assert_eq!(
            db.reopenable_assignment(repo, "aa", "tree", "cfl_1", Timestamp(50))
                .unwrap()
                .map(|r| r.id),
            Some("asg_a".to_owned())
        );
        assert_eq!(
            db.delete_closed_assignments(repo, Timestamp(101)).unwrap(),
            1
        );
        assert!(db.assignment(repo, "asg_a").unwrap().is_none());
        assert_eq!(db.assignment_events(repo, "asg_a", 0, 10).unwrap(), []);
        // Retention never lets the seq go back.
        let later = row(repo, "asg_f", "ff", "queued", 0, 1);
        let next = db
            .record_assignments(repo, vec![(later.clone(), event(&later))])
            .unwrap();
        assert_eq!(next[0].seq, 6);
        assert_eq!(db.assignment_events(repo, "asg_b", 0, 10).unwrap().len(), 1);
    }
}
