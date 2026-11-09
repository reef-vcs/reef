//! Queries on `lifecycle_transitions` and `lifecycle` (R3.09, ADR 0063).
//!
//! A transition row is written `pending` before its operation is written,
//! and committed with the operation's id, its end time and the next `seq`
//! after it, in the same transaction that updates the change's `lifecycle`
//! row ([`Db::commit_transition`]). Reads see committed rows only. Both
//! tables are per repository and deleted with it
//! ([`Db::delete_repository`]); GC maps operation ids
//! ([`Db::map_transition_operations`]) and deletes nothing.

use std::collections::HashMap;
use std::fmt::Write as _;

use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::evidence::text_at;
use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// One transition, as `lifecycle_transitions` holds it. States, causes and
/// actors are their column texts (`reef_lib::jj::lifecycle`'s names).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct TransitionRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `trn_` and 16 hex digits.
    pub id: String,
    /// Its place among the repository's committed transitions; `None`
    /// while pending.
    pub seq: Option<i64>,
    /// Whether its operation is known to exist.
    pub committed: bool,
    /// The change, in hex.
    pub change_id: String,
    /// Its commit when recorded.
    pub commit_id: String,
    /// The state before.
    pub from_state: String,
    /// The state after.
    pub to_state: String,
    /// `request`, `undo` or `observed`.
    pub cause: String,
    /// The proposal round.
    pub round: u32,
    /// The reason given.
    pub reason: Option<String>,
    /// For a supersede, the superseding change.
    pub superseded_by: Option<String>,
    /// The bookmark it is proposed for.
    pub destination: Option<String>,
    /// For an undo, the operation it reverts.
    pub undoes_operation_id: Option<String>,
    /// For an undo, the transition it reverts.
    pub undoes_transition_id: Option<String>,
    /// The operation that recorded it; `None` while pending.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// The call's `reef-agent`.
    pub agent: Option<String>,
    /// `human`, `agent` or `daemon`.
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
    /// The operation's end; `None` while pending.
    pub recorded_at: Option<Timestamp>,
}

/// A change's current lifecycle, as `lifecycle` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LifecycleRow {
    /// The repository.
    pub repo_id: RepoId,
    /// The change, in hex.
    pub change_id: String,
    /// Its state's column text.
    pub state: String,
    /// Its proposal round.
    pub round: u32,
    /// The commit its latest transition recorded.
    pub commit_id: String,
    /// The bookmark it is proposed for.
    pub destination: Option<String>,
    /// For a superseded change, the superseding one.
    pub superseded_by: Option<String>,
    /// Its latest transition.
    pub transition_id: String,
    /// The agent of its latest proposal.
    pub proposed_by_agent: Option<String>,
    /// The remote token of its latest proposal.
    pub proposed_by_token: Option<String>,
    /// When its latest transition was recorded.
    pub updated_at: Timestamp,
}

/// What committing a transition does to its change's `lifecycle` row.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum LifecycleUpdate {
    /// Write this row.
    Set(Box<LifecycleRow>),
    /// The change is a draft again: delete its row.
    Delete,
    /// Leave the row as it is: a transition committed late, at start,
    /// whose `from` is no longer the change's state.
    Keep,
}

/// Which `lifecycle` rows a listing returns: fields AND, an empty list
/// matches everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LifecycleFilter {
    /// Only these states' column texts.
    pub states: Vec<String>,
    /// Only changes starting with one of these hex prefixes.
    pub changes: Vec<String>,
    /// Only proposals made by these agents.
    pub agents: Vec<String>,
}

/// Which transitions a listing returns: fields AND, an empty list matches
/// everything.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TransitionFilter {
    /// Only changes starting with one of these hex prefixes.
    pub changes: Vec<String>,
    /// Only these target states' column texts.
    pub to: Vec<String>,
    /// Only these causes.
    pub causes: Vec<String>,
    /// Only those recorded after this time.
    pub recorded_after: Option<Timestamp>,
}

const TRANSITION_COLUMNS: &str = "repo_id, id, seq, status, change_id, commit_id, from_state,
     to_state, cause, round, reason, superseded_by, destination, undoes_operation_id,
     undoes_transition_id, operation_id, operation_collected, agent, actor, identity_id,
     identity_name, session_id, remote_token, remote_token_name, created_at, recorded_at";

const LIFECYCLE_COLUMNS: &str = "repo_id, change_id, state, round, commit_id, destination,
     superseded_by, transition_id, proposed_by_agent, proposed_by_token, updated_at";

fn small(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(0)
}

fn transition_from_row(row: &Row<'_>) -> rusqlite::Result<TransitionRow> {
    Ok(TransitionRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        seq: row.get(2)?,
        committed: row.get::<_, String>(3)? == "committed",
        change_id: row.get(4)?,
        commit_id: row.get(5)?,
        from_state: row.get(6)?,
        to_state: row.get(7)?,
        cause: row.get(8)?,
        round: small(row.get(9)?),
        reason: text_at(row, 10)?,
        superseded_by: text_at(row, 11)?,
        destination: text_at(row, 12)?,
        undoes_operation_id: text_at(row, 13)?,
        undoes_transition_id: text_at(row, 14)?,
        operation_id: text_at(row, 15)?,
        operation_collected: row.get::<_, i64>(16)? != 0,
        agent: text_at(row, 17)?,
        actor: row.get(18)?,
        identity_id: text_at(row, 19)?,
        identity_name: text_at(row, 20)?,
        session_id: text_at(row, 21)?,
        remote_token: text_at(row, 22)?,
        remote_token_name: text_at(row, 23)?,
        created_at: Timestamp(row.get(24)?),
        recorded_at: row.get::<_, Option<i64>>(25)?.map(Timestamp),
    })
}

fn lifecycle_from_row(row: &Row<'_>) -> rusqlite::Result<LifecycleRow> {
    Ok(LifecycleRow {
        repo_id: row.get(0)?,
        change_id: row.get(1)?,
        state: row.get(2)?,
        round: small(row.get(3)?),
        commit_id: row.get(4)?,
        destination: text_at(row, 5)?,
        superseded_by: text_at(row, 6)?,
        transition_id: row.get(7)?,
        proposed_by_agent: text_at(row, 8)?,
        proposed_by_token: text_at(row, 9)?,
        updated_at: Timestamp(row.get(10)?),
    })
}

/// `column LIKE ?` clauses for hex `prefixes`, joined by `OR`, appended to `sql` with
/// their values, or nothing for none.
fn prefix_clause(
    sql: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    column: &str,
    prefixes: &[String],
) {
    if prefixes.is_empty() {
        return;
    }
    let ors: Vec<String> = prefixes
        .iter()
        .map(|prefix| {
            values.push(rusqlite::types::Value::Text(format!("{prefix}%")));
            format!("{column} LIKE ?")
        })
        .collect();
    let _ = write!(sql, " AND ({})", ors.join(" OR "));
}

/// `column IN (…)` for `list`, appended with its values, or nothing for an
/// empty list.
fn in_clause(
    sql: &mut String,
    values: &mut Vec<rusqlite::types::Value>,
    column: &str,
    list: &[String],
) {
    if list.is_empty() {
        return;
    }
    let marks = vec!["?"; list.len()].join(", ");
    values.extend(
        list.iter()
            .map(|item| rusqlite::types::Value::Text(item.clone())),
    );
    let _ = write!(sql, " AND {column} IN ({marks})");
}

impl Db {
    /// Writes `row`, pending.
    pub fn insert_pending_transition(&self, row: &TransitionRow) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                &format!(
                    "INSERT INTO lifecycle_transitions({TRANSITION_COLUMNS})
                     VALUES (?1, ?2, NULL, 'pending', ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11,
                             ?12, ?13, NULL, 0, ?14, ?15, ?16, ?17, ?18, ?19, ?20, ?21, NULL)"
                ),
                params![
                    row.repo_id,
                    row.id,
                    row.change_id,
                    row.commit_id,
                    row.from_state,
                    row.to_state,
                    row.cause,
                    i64::from(row.round),
                    row.reason,
                    row.superseded_by,
                    row.destination,
                    row.undoes_operation_id,
                    row.undoes_transition_id,
                    row.agent,
                    row.actor,
                    row.identity_id,
                    row.identity_name,
                    row.session_id,
                    row.remote_token,
                    row.remote_token_name,
                    row.created_at.0,
                ],
            )?;
            Ok(())
        })
    }

    /// Sets the commit of the pending transition `id`: a landing knows the
    /// commit it landed only after its operation (R3.13).
    pub fn set_pending_transition_commit(
        &self,
        repo: RepoId,
        id: &str,
        commit: &str,
    ) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE lifecycle_transitions SET commit_id = ?3
                 WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
                params![repo, id, commit],
            )?;
            Ok(())
        })
    }

    /// Deletes the pending transition `id`: its operation was never written.
    pub fn delete_pending_transition(&self, repo: RepoId, id: &str) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "DELETE FROM lifecycle_transitions
                 WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
                params![repo, id],
            )?;
            Ok(())
        })
    }

    /// Commits the pending transition `id` with its operation and end time,
    /// giving it the repository's next `seq`, and applies `update` to its
    /// change's `lifecycle` row, in one transaction. Returns the committed
    /// row; `None` when no such pending row exists.
    pub fn commit_transition(
        &self,
        repo: RepoId,
        id: &str,
        operation_id: &str,
        recorded_at: Timestamp,
        update: &LifecycleUpdate,
    ) -> Result<Option<TransitionRow>, DbError> {
        self.in_transaction(|tx| {
            commit_transition_in(tx, repo, id, operation_id, recorded_at, update)
        })
    }

    /// Every pending transition, of every repository, oldest first.
    pub fn pending_transitions(&self) -> Result<Vec<TransitionRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {TRANSITION_COLUMNS} FROM lifecycle_transitions
                 WHERE status = 'pending' ORDER BY created_at, id"
            ))?;
            statement
                .query_map([], transition_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// The `lifecycle` row of `change`, if it has one.
    pub fn lifecycle_row(
        &self,
        repo: RepoId,
        change: &str,
    ) -> Result<Option<LifecycleRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {LIFECYCLE_COLUMNS} FROM lifecycle
                     WHERE repo_id = ?1 AND change_id = ?2"
                ),
                params![repo, change],
                lifecycle_from_row,
            )
            .optional()
        })
    }

    /// The `lifecycle` rows of `changes`, by change id.
    pub fn lifecycle_rows(
        &self,
        repo: RepoId,
        changes: &[String],
    ) -> Result<HashMap<String, LifecycleRow>, DbError> {
        let mut found = HashMap::new();
        if changes.is_empty() {
            return Ok(found);
        }
        self.with(|conn| {
            for chunk in changes.chunks(500) {
                let marks = vec!["?"; chunk.len()].join(", ");
                let mut statement = conn.prepare(&format!(
                    "SELECT {LIFECYCLE_COLUMNS} FROM lifecycle
                     WHERE repo_id = ? AND change_id IN ({marks})"
                ))?;
                let mut values: Vec<rusqlite::types::Value> =
                    vec![rusqlite::types::Value::Text(repo.as_hex())];
                values.extend(
                    chunk
                        .iter()
                        .map(|change| rusqlite::types::Value::Text(change.clone())),
                );
                for row in statement.query_map(params_from_iter(values), lifecycle_from_row)? {
                    let row = row?;
                    found.insert(row.change_id.clone(), row);
                }
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// The latest committed transition of each of `changes` recorded by the
    /// operation `op` or before it, by change id: their lifecycle as of that
    /// operation, which ended at `at`. "Before" is an earlier end time, so a
    /// transition whose operation ended in the same millisecond as `op` (and
    /// is not `op`) counts as after it.
    pub fn transitions_as_of(
        &self,
        repo: RepoId,
        changes: &[String],
        at: Timestamp,
        op: &str,
    ) -> Result<HashMap<String, TransitionRow>, DbError> {
        let mut found = HashMap::new();
        self.with(|conn| {
            for chunk in changes.chunks(500) {
                let marks = vec!["?"; chunk.len()].join(", ");
                let mut statement = conn.prepare(&format!(
                    "SELECT {TRANSITION_COLUMNS} FROM (
                       SELECT *, row_number() OVER (PARTITION BY change_id ORDER BY seq DESC) AS n
                       FROM lifecycle_transitions
                       WHERE repo_id = ? AND status = 'committed'
                         AND (recorded_at < ? OR operation_id = ?)
                         AND change_id IN ({marks}))
                     WHERE n = 1"
                ))?;
                let mut values: Vec<rusqlite::types::Value> = vec![
                    rusqlite::types::Value::Text(repo.as_hex()),
                    rusqlite::types::Value::Integer(at.0),
                    rusqlite::types::Value::Text(op.to_owned()),
                ];
                values.extend(
                    chunk
                        .iter()
                        .map(|change| rusqlite::types::Value::Text(change.clone())),
                );
                for row in statement.query_map(params_from_iter(values), transition_from_row)? {
                    let row = row?;
                    found.insert(row.change_id.clone(), row);
                }
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// The committed transitions `ids`, by id.
    pub fn transitions_by_id(
        &self,
        repo: RepoId,
        ids: &[String],
    ) -> Result<HashMap<String, TransitionRow>, DbError> {
        let mut found = HashMap::new();
        self.with(|conn| {
            for chunk in ids.chunks(500) {
                let marks = vec!["?"; chunk.len()].join(", ");
                let mut statement = conn.prepare(&format!(
                    "SELECT {TRANSITION_COLUMNS} FROM lifecycle_transitions
                     WHERE repo_id = ? AND status = 'committed' AND id IN ({marks})"
                ))?;
                let mut values = vec![rusqlite::types::Value::Text(repo.as_hex())];
                values.extend(
                    chunk
                        .iter()
                        .map(|id| rusqlite::types::Value::Text(id.clone())),
                );
                for row in statement.query_map(params_from_iter(values), transition_from_row)? {
                    let row = row?;
                    found.insert(row.id.clone(), row);
                }
            }
            Ok(())
        })?;
        Ok(found)
    }

    /// The highest round any transition of `change` recorded; 0 for none.
    pub fn max_round(&self, repo: RepoId, change: &str) -> Result<u32, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT COALESCE(MAX(round), 0) FROM lifecycle_transitions
                 WHERE repo_id = ?1 AND change_id = ?2 AND status = 'committed'",
                params![repo, change],
                |row| row.get::<_, i64>(0),
            )
        })
        .map(small)
    }

    /// The committed transition `id`.
    pub fn transition(&self, repo: RepoId, id: &str) -> Result<Option<TransitionRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {TRANSITION_COLUMNS} FROM lifecycle_transitions
                     WHERE repo_id = ?1 AND id = ?2 AND status = 'committed'"
                ),
                params![repo, id],
                transition_from_row,
            )
            .optional()
        })
    }

    /// The committed transitions an operation recorded, in `seq` order.
    pub fn transitions_of_operation(
        &self,
        repo: RepoId,
        operation_id: &str,
    ) -> Result<Vec<TransitionRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(&format!(
                "SELECT {TRANSITION_COLUMNS} FROM lifecycle_transitions
                 WHERE repo_id = ?1 AND operation_id = ?2 AND status = 'committed'
                 ORDER BY seq"
            ))?;
            statement
                .query_map(params![repo, operation_id], transition_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// `lifecycle` rows matching `filter`, newest update first, after the
    /// cursor `(updated_at, change_id)`, at most `limit`.
    pub fn lifecycle_page(
        &self,
        repo: RepoId,
        filter: &LifecycleFilter,
        after: Option<(Timestamp, String)>,
        limit: usize,
    ) -> Result<Vec<LifecycleRow>, DbError> {
        let mut sql = format!("SELECT {LIFECYCLE_COLUMNS} FROM lifecycle WHERE repo_id = ?");
        let mut values = vec![rusqlite::types::Value::Text(repo.as_hex())];
        in_clause(&mut sql, &mut values, "state", &filter.states);
        in_clause(&mut sql, &mut values, "proposed_by_agent", &filter.agents);
        prefix_clause(&mut sql, &mut values, "change_id", &filter.changes);
        if let Some((at, change)) = after {
            sql.push_str(" AND (updated_at < ? OR (updated_at = ? AND change_id > ?))");
            values.push(rusqlite::types::Value::Integer(at.0));
            values.push(rusqlite::types::Value::Integer(at.0));
            values.push(rusqlite::types::Value::Text(change));
        }
        sql.push_str(" ORDER BY updated_at DESC, change_id LIMIT ?");
        values.push(rusqlite::types::Value::Integer(
            i64::try_from(limit).unwrap_or(i64::MAX),
        ));
        self.with(|conn| {
            let mut statement = conn.prepare(&sql)?;
            statement
                .query_map(params_from_iter(values), lifecycle_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// Committed transitions matching `filter`, newest first, before `seq`
    /// `before`, at most `limit`.
    pub fn transition_page(
        &self,
        repo: RepoId,
        filter: &TransitionFilter,
        before: Option<i64>,
        limit: usize,
    ) -> Result<Vec<TransitionRow>, DbError> {
        let mut sql = format!(
            "SELECT {TRANSITION_COLUMNS} FROM lifecycle_transitions
             WHERE repo_id = ? AND status = 'committed'"
        );
        let mut values = vec![rusqlite::types::Value::Text(repo.as_hex())];
        in_clause(&mut sql, &mut values, "to_state", &filter.to);
        in_clause(&mut sql, &mut values, "cause", &filter.causes);
        prefix_clause(&mut sql, &mut values, "change_id", &filter.changes);
        if let Some(after) = filter.recorded_after {
            sql.push_str(" AND recorded_at > ?");
            values.push(rusqlite::types::Value::Integer(after.0));
        }
        if let Some(before) = before {
            sql.push_str(" AND seq < ?");
            values.push(rusqlite::types::Value::Integer(before));
        }
        sql.push_str(" ORDER BY seq DESC LIMIT ?");
        values.push(rusqlite::types::Value::Integer(
            i64::try_from(limit).unwrap_or(i64::MAX),
        ));
        self.with(|conn| {
            let mut statement = conn.prepare(&sql)?;
            statement
                .query_map(params_from_iter(values), transition_from_row)?
                .collect::<Result<_, _>>()
        })
    }

    /// The changes of `repo` that are proposed or queued: those an observed
    /// landing looks for.
    pub fn unlanded_changes(&self, repo: RepoId) -> Result<Vec<String>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT change_id FROM lifecycle
                 WHERE repo_id = ?1 AND state IN ('proposed', 'queued') ORDER BY change_id",
            )?;
            statement
                .query_map([repo], |row| row.get(0))?
                .collect::<Result<_, _>>()
        })
    }

    /// Maps the operation ids of `repo`'s committed transitions through
    /// GC's rewrite (old id to new), and marks the ones it dropped
    /// collected. Returns how many were mapped and how many marked.
    pub fn map_transition_operations(
        &self,
        repo: RepoId,
        rewritten: &HashMap<String, String>,
    ) -> Result<(usize, usize), DbError> {
        self.in_transaction(|tx| {
            let rows: Vec<(String, String)> = {
                let mut statement = tx.prepare(
                    "SELECT id, operation_id FROM lifecycle_transitions
                     WHERE repo_id = ?1 AND status = 'committed' AND operation_collected = 0",
                )?;
                statement
                    .query_map([repo], |row| Ok((row.get(0)?, row.get(1)?)))?
                    .collect::<Result<_, _>>()?
            };
            let (mut mapped, mut collected) = (0, 0);
            for (id, old) in rows {
                if let Some(new) = rewritten.get(&old) {
                    tx.execute(
                        "UPDATE lifecycle_transitions SET operation_id = ?3
                         WHERE repo_id = ?1 AND id = ?2",
                        params![repo, id, new],
                    )?;
                    mapped += 1;
                } else {
                    tx.execute(
                        "UPDATE lifecycle_transitions SET operation_collected = 1
                         WHERE repo_id = ?1 AND id = ?2",
                        params![repo, id],
                    )?;
                    collected += 1;
                }
            }
            Ok((mapped, collected))
        })
    }

    /// How many `lifecycle` and committed transition rows exist, for tests
    /// and GC's report.
    pub fn lifecycle_counts(&self) -> Result<(u64, u64), DbError> {
        self.with(|conn| {
            let states: i64 =
                conn.query_row("SELECT count(*) FROM lifecycle", [], |row| row.get(0))?;
            let transitions: i64 = conn.query_row(
                "SELECT count(*) FROM lifecycle_transitions WHERE status = 'committed'",
                [],
                |row| row.get(0),
            )?;
            Ok((
                u64::try_from(states).unwrap_or(0),
                u64::try_from(transitions).unwrap_or(0),
            ))
        })
    }
}

/// [`Db::commit_transition`] inside `tx`.
fn commit_transition_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    id: &str,
    operation_id: &str,
    recorded_at: Timestamp,
    update: &LifecycleUpdate,
) -> Result<Option<TransitionRow>, DbError> {
    let seq: i64 = tx.query_row(
        "SELECT COALESCE(MAX(seq), 0) + 1 FROM lifecycle_transitions WHERE repo_id = ?1",
        [repo],
        |row| row.get(0),
    )?;
    let changed = tx.execute(
        "UPDATE lifecycle_transitions
         SET status = 'committed', seq = ?3, operation_id = ?4, recorded_at = ?5
         WHERE repo_id = ?1 AND id = ?2 AND status = 'pending'",
        params![repo, id, seq, operation_id, recorded_at.0],
    )?;
    if changed == 0 {
        return Ok(None);
    }
    let row = tx.query_row(
        &format!(
            "SELECT {TRANSITION_COLUMNS} FROM lifecycle_transitions WHERE repo_id = ?1 AND id = ?2"
        ),
        params![repo, id],
        transition_from_row,
    )?;
    match update {
        LifecycleUpdate::Set(state) => {
            tx.execute(
                &format!(
                    "INSERT INTO lifecycle({LIFECYCLE_COLUMNS})
                     VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11)
                     ON CONFLICT(repo_id, change_id) DO UPDATE SET
                       state = excluded.state, round = excluded.round,
                       commit_id = excluded.commit_id, destination = excluded.destination,
                       superseded_by = excluded.superseded_by,
                       transition_id = excluded.transition_id,
                       proposed_by_agent = excluded.proposed_by_agent,
                       proposed_by_token = excluded.proposed_by_token,
                       updated_at = excluded.updated_at"
                ),
                params![
                    repo,
                    state.change_id,
                    state.state,
                    i64::from(state.round),
                    state.commit_id,
                    state.destination,
                    state.superseded_by,
                    state.transition_id,
                    state.proposed_by_agent,
                    state.proposed_by_token,
                    state.updated_at.0,
                ],
            )?;
        }
        LifecycleUpdate::Delete => {
            tx.execute(
                "DELETE FROM lifecycle WHERE repo_id = ?1 AND change_id = ?2",
                params![repo, row.change_id],
            )?;
        }
        LifecycleUpdate::Keep => {}
    }
    Ok(Some(row))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::repositories::tests::repo;

    fn pending(repo_id: RepoId, id: &str, change: &str, from: &str, to: &str) -> TransitionRow {
        TransitionRow {
            repo_id,
            id: id.to_owned(),
            seq: None,
            committed: false,
            change_id: change.to_owned(),
            commit_id: "c".repeat(40),
            from_state: from.to_owned(),
            to_state: to.to_owned(),
            cause: "request".to_owned(),
            round: 1,
            reason: Some("why".to_owned()),
            superseded_by: None,
            destination: Some("main".to_owned()),
            undoes_operation_id: None,
            undoes_transition_id: None,
            operation_id: None,
            operation_collected: false,
            agent: Some("bot".to_owned()),
            actor: "agent".to_owned(),
            identity_id: None,
            identity_name: None,
            session_id: None,
            remote_token: None,
            remote_token_name: None,
            created_at: Timestamp(1_000),
            recorded_at: None,
        }
    }

    fn state_of(row: &TransitionRow) -> LifecycleUpdate {
        LifecycleUpdate::Set(Box::new(LifecycleRow {
            repo_id: row.repo_id,
            change_id: row.change_id.clone(),
            state: row.to_state.clone(),
            round: row.round,
            commit_id: row.commit_id.clone(),
            destination: row.destination.clone(),
            superseded_by: None,
            transition_id: row.id.clone(),
            proposed_by_agent: row.agent.clone(),
            proposed_by_token: None,
            updated_at: Timestamp(2_000),
        }))
    }

    #[test]
    #[expect(clippy::too_many_lines, reason = "one change through every query")]
    fn pending_rows_commit_in_order_and_update_the_state() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let change = "ab".repeat(16);
        let first = pending(
            record.id,
            "trn_0000000000000001",
            &change,
            "draft",
            "proposed",
        );
        db.insert_pending_transition(&first).unwrap();
        assert_eq!(db.pending_transitions().unwrap().len(), 1);
        assert!(db.lifecycle_row(record.id, &change).unwrap().is_none());
        let committed = db
            .commit_transition(
                record.id,
                &first.id,
                "op1",
                Timestamp(2_000),
                &state_of(&first),
            )
            .unwrap()
            .unwrap();
        assert_eq!(committed.seq, Some(1));
        assert!(committed.committed);
        assert_eq!(
            db.lifecycle_row(record.id, &change).unwrap().unwrap().state,
            "proposed"
        );
        // Committing twice does nothing.
        assert!(
            db.commit_transition(
                record.id,
                &first.id,
                "op1",
                Timestamp(2_000),
                &state_of(&first)
            )
            .unwrap()
            .is_none()
        );
        let second = pending(
            record.id,
            "trn_0000000000000002",
            &change,
            "proposed",
            "withdrawn",
        );
        db.insert_pending_transition(&second).unwrap();
        let committed = db
            .commit_transition(
                record.id,
                &second.id,
                "op2",
                Timestamp(3_000),
                &state_of(&second),
            )
            .unwrap()
            .unwrap();
        assert_eq!(committed.seq, Some(2));
        let as_of = db
            .transitions_as_of(
                record.id,
                std::slice::from_ref(&change),
                Timestamp(2_500),
                "x",
            )
            .unwrap();
        assert_eq!(as_of[&change].to_state, "proposed");
        // The same millisecond counts as after, unless it is that operation.
        let tie = db
            .transitions_as_of(
                record.id,
                std::slice::from_ref(&change),
                Timestamp(3_000),
                "x",
            )
            .unwrap();
        assert_eq!(tie[&change].to_state, "proposed");
        let own = db
            .transitions_as_of(
                record.id,
                std::slice::from_ref(&change),
                Timestamp(3_000),
                "op2",
            )
            .unwrap();
        assert_eq!(own[&change].to_state, "withdrawn");
        assert_eq!(db.max_round(record.id, &change).unwrap(), 1);
        assert_eq!(as_of[&change].to_state, "proposed");
        let page = db
            .transition_page(record.id, &TransitionFilter::default(), None, 10)
            .unwrap();
        assert_eq!(
            page.iter().map(|row| row.id.as_str()).collect::<Vec<_>>(),
            [second.id.as_str(), first.id.as_str()]
        );
        let lifecycle = db
            .lifecycle_page(
                record.id,
                &LifecycleFilter {
                    states: vec!["withdrawn".to_owned()],
                    changes: vec!["abab".to_owned()],
                    agents: Vec::new(),
                },
                None,
                10,
            )
            .unwrap();
        assert_eq!(lifecycle.len(), 1);
        // Withdrawn: not one an observed landing looks for.
        assert_eq!(
            db.unlanded_changes(record.id).unwrap(),
            Vec::<String>::new()
        );

        let mapped = db
            .map_transition_operations(
                record.id,
                &HashMap::from([("op2".to_owned(), "op2b".to_owned())]),
            )
            .unwrap();
        assert_eq!(mapped, (1, 1));
        assert_eq!(
            db.transitions_of_operation(record.id, "op2b")
                .unwrap()
                .len(),
            1
        );
        assert!(
            db.transition(record.id, &first.id)
                .unwrap()
                .unwrap()
                .operation_collected
        );
    }

    #[test]
    fn a_pending_row_can_be_dropped_and_delete_clears_the_state() {
        let db = Db::open_in_memory().unwrap();
        let record = repo("aaaa000000000001", "/src/app");
        db.insert_repository(&record).unwrap();
        let change = "cd".repeat(16);
        let row = pending(
            record.id,
            "trn_0000000000000003",
            &change,
            "draft",
            "proposed",
        );
        db.insert_pending_transition(&row).unwrap();
        db.delete_pending_transition(record.id, &row.id).unwrap();
        assert_eq!(db.pending_transitions().unwrap(), Vec::new());
        db.insert_pending_transition(&row).unwrap();
        db.commit_transition(record.id, &row.id, "op", Timestamp(5), &state_of(&row))
            .unwrap();
        let undo = pending(
            record.id,
            "trn_0000000000000004",
            &change,
            "proposed",
            "draft",
        );
        db.insert_pending_transition(&undo).unwrap();
        db.commit_transition(
            record.id,
            &undo.id,
            "op2",
            Timestamp(6),
            &LifecycleUpdate::Delete,
        )
        .unwrap();
        assert!(db.lifecycle_row(record.id, &change).unwrap().is_none());
        assert_eq!(db.lifecycle_counts().unwrap(), (0, 2));
    }
}
