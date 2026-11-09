//! Queries on `provenance` and `provenance_sessions` (R3.04, ADR 0058).
//!
//! A provenance row holds the canonical envelope of a record the daemon
//! signed about one commit it recorded for a workspace; every other column
//! is an index over its payload, written once at insert, and reads check
//! each against the verified payload. Both tables are per repository and
//! deleted with it ([`Db::delete_repository`]); GC's eighth step compacts
//! and expires records ([`Db::provenance_to_compact`],
//! [`Db::provenance_to_expire`], [`Db::delete_provenance`]).
//!
//! A `provenance_sessions` row is a workspace's provenance session: its
//! identity, actor and channel, fixed for the session, and the context a
//! client declared last. One per workspace is live (`ended_at` NULL).

use std::collections::{HashMap, HashSet};

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::evidence::{EvidenceKeyRow, bytes_at, integer_at, text_at};
use super::{Db, DbError, PREFIX_LOOKUP_LIMIT, unique_match};
use crate::model::{RepoId, Timestamp};

/// The record kind errors name.
const WHAT: &str = "provenance record";

/// Records GC deletes per transaction.
const SWEEP_BATCH: usize = 1000;

/// The declared fields of a context, as columns.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct DeclaredColumns {
    /// The model.
    pub model: Option<String>,
    /// The harness.
    pub harness: Option<String>,
    /// The task.
    pub task: Option<String>,
    /// The instruction's SHA-256.
    pub instruction_sha256: Option<String>,
    /// The client's session.
    pub client_session: Option<String>,
}

/// One row of `provenance`. Columns are read leniently, as evidence rows
/// are: reads take the verified payload's values and compare.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceRow {
    /// The repository.
    pub repo_id: RepoId,
    /// SHA-256 of `envelope`, lowercase hex.
    pub id: String,
    /// The canonical envelope.
    pub envelope: Vec<u8>,
    /// 1: `urn:reef-vcs:provenance:v1`.
    pub format: u32,
    /// The subject.
    pub commit_id: String,
    /// The change.
    pub change_id: String,
    /// The tree, unless conflicted.
    pub tree_id: Option<String>,
    /// The workspace.
    pub workspace_id: String,
    /// The provenance session.
    pub session_id: String,
    /// The record's number in its session.
    pub sequence: i64,
    /// What recorded it.
    pub kind: String,
    /// The signing key.
    pub key_id: String,
    /// The agent label.
    pub agent: Option<String>,
    /// The declared context.
    pub declared: DeclaredColumns,
    /// SHA-256 of the canonical context without `declaredOn`.
    pub context_digest: String,
    /// When it was recorded.
    pub recorded_at: Timestamp,
}

/// A stored record with its key's public half, when the key row exists.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredProvenance {
    /// The row.
    pub row: ProvenanceRow,
    /// `evidence_keys.public_key`.
    pub public_key: Option<Vec<u8>>,
}

/// One row of `provenance_sessions`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ProvenanceSessionRow {
    /// `ses_` and 16 hex digits.
    pub session_id: String,
    /// The repository.
    pub repo_id: RepoId,
    /// The workspace.
    pub workspace_id: String,
    /// The identity its keys are certified for; `None` when none can be.
    pub identity_id: Option<String>,
    /// `agent`, `executor` or `human`.
    pub actor: String,
    /// The agent label, fixed for the session.
    pub agent: Option<String>,
    /// `socket`, or the creating remote token's id.
    pub channel: String,
    /// The context declared last.
    pub declared: DeclaredColumns,
    /// When it was declared.
    pub declared_at: Option<Timestamp>,
    /// The declaring request's `reef-agent`.
    pub declared_by_agent: Option<String>,
    /// The next record's sequence.
    pub next_sequence: i64,
    /// Commits recorded with no record, by a failure.
    pub missing_records: i64,
    /// Why records are unbound, if they are.
    pub problem: Option<String>,
    /// When it started.
    pub started_at: Timestamp,
    /// When it ended.
    pub ended_at: Option<Timestamp>,
}

/// Which records of a repository a listing returns: the fields AND.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProvenanceFilter {
    /// Only records of these changes; empty for any.
    pub change_ids: Vec<String>,
    /// Only this workspace's.
    pub workspace_id: Option<String>,
    /// Only this session's.
    pub session_id: Option<String>,
    /// Only those recorded at or after.
    pub since: Option<Timestamp>,
}

const COLUMNS: &str = "p.repo_id, p.id, p.envelope, p.format, p.commit_id, p.change_id, \
     p.tree_id, p.workspace_id, p.session_id, p.sequence, p.kind, p.key_id, p.agent, p.model, \
     p.harness, p.task, p.instruction_sha256, p.client_session, p.context_digest, \
     p.recorded_at, k.public_key";

const FROM: &str = "provenance p LEFT JOIN evidence_keys k ON k.id = p.key_id";

fn from_row(row: &Row<'_>) -> rusqlite::Result<StoredProvenance> {
    Ok(StoredProvenance {
        row: ProvenanceRow {
            repo_id: row.get(0)?,
            id: text_at(row, 1)?.unwrap_or_default(),
            envelope: bytes_at(row, 2)?.unwrap_or_default(),
            format: u32::try_from(integer_at(row, 3)?).unwrap_or(0),
            commit_id: text_at(row, 4)?.unwrap_or_default(),
            change_id: text_at(row, 5)?.unwrap_or_default(),
            tree_id: text_at(row, 6)?,
            workspace_id: text_at(row, 7)?.unwrap_or_default(),
            session_id: text_at(row, 8)?.unwrap_or_default(),
            sequence: integer_at(row, 9)?,
            kind: text_at(row, 10)?.unwrap_or_default(),
            key_id: text_at(row, 11)?.unwrap_or_default(),
            agent: text_at(row, 12)?,
            declared: DeclaredColumns {
                model: text_at(row, 13)?,
                harness: text_at(row, 14)?,
                task: text_at(row, 15)?,
                instruction_sha256: text_at(row, 16)?,
                client_session: text_at(row, 17)?,
            },
            context_digest: text_at(row, 18)?.unwrap_or_default(),
            recorded_at: Timestamp(integer_at(row, 19)?),
        },
        public_key: bytes_at(row, 20)?,
    })
}

const SESSION_COLUMNS: &str = "session_id, repo_id, workspace_id, identity_id, actor, agent, \
     channel, model, harness, task, instruction_sha256, client_session, declared_at, \
     declared_by_agent, next_sequence, missing_records, problem, started_at, ended_at";

fn session_from_row(row: &Row<'_>) -> rusqlite::Result<ProvenanceSessionRow> {
    let optional_time = |idx| -> rusqlite::Result<Option<Timestamp>> {
        Ok(match row.get::<_, SqlValue>(idx)? {
            SqlValue::Integer(n) => Some(Timestamp(n)),
            _ => None,
        })
    };
    Ok(ProvenanceSessionRow {
        session_id: row.get(0)?,
        repo_id: row.get(1)?,
        workspace_id: row.get(2)?,
        identity_id: row.get(3)?,
        actor: row.get(4)?,
        agent: row.get(5)?,
        channel: row.get(6)?,
        declared: DeclaredColumns {
            model: row.get(7)?,
            harness: row.get(8)?,
            task: row.get(9)?,
            instruction_sha256: row.get(10)?,
            client_session: row.get(11)?,
        },
        declared_at: optional_time(12)?,
        declared_by_agent: row.get(13)?,
        next_sequence: row.get(14)?,
        missing_records: row.get(15)?,
        problem: row.get(16)?,
        started_at: row.get(17)?,
        ended_at: optional_time(18)?,
    })
}

fn insert_session_in(tx: &Transaction<'_>, row: &ProvenanceSessionRow) -> rusqlite::Result<()> {
    tx.execute(
        &format!(
            "INSERT INTO provenance_sessions({SESSION_COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19)"
        ),
        params![
            row.session_id,
            row.repo_id,
            row.workspace_id,
            row.identity_id,
            row.actor,
            row.agent,
            row.channel,
            row.declared.model,
            row.declared.harness,
            row.declared.task,
            row.declared.instruction_sha256,
            row.declared.client_session,
            row.declared_at,
            row.declared_by_agent,
            row.next_sequence,
            row.missing_records,
            row.problem,
            row.started_at,
            row.ended_at,
        ],
    )?;
    Ok(())
}

/// A temporary table `temp.<name>(value TEXT PRIMARY KEY)` holding
/// `values`, replacing any of that name.
fn temp_set<'a>(
    tx: &Transaction<'_>,
    name: &str,
    values: impl IntoIterator<Item = &'a String>,
) -> rusqlite::Result<()> {
    tx.execute_batch(&format!(
        "DROP TABLE IF EXISTS temp.{name}; CREATE TEMP TABLE {name}(value TEXT PRIMARY KEY);"
    ))?;
    let mut stmt = tx.prepare(&format!(
        "INSERT OR IGNORE INTO temp.{name}(value) VALUES (?1)"
    ))?;
    for value in values {
        stmt.execute([value])?;
    }
    Ok(())
}

impl Db {
    /// Stores a new provenance session.
    pub fn insert_provenance_session(&self, row: &ProvenanceSessionRow) -> Result<(), DbError> {
        self.in_transaction(|tx| Ok(insert_session_in(tx, row)?))
    }

    /// Ends `old` at `at` and stores `new` in its place, in one transaction:
    /// a workspace's session restarted after a revocation.
    pub fn restart_provenance_session(
        &self,
        old: &str,
        at: Timestamp,
        new: &ProvenanceSessionRow,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            tx.execute(
                "UPDATE provenance_sessions SET ended_at = ?2
                 WHERE session_id = ?1 AND ended_at IS NULL",
                params![old, at],
            )?;
            insert_session_in(tx, new)?;
            Ok(())
        })
    }

    /// The live provenance session of `workspace_id`, if it has one.
    pub fn live_provenance_session(
        &self,
        workspace_id: &str,
    ) -> Result<Option<ProvenanceSessionRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!(
                    "SELECT {SESSION_COLUMNS} FROM provenance_sessions
                     WHERE workspace_id = ?1 AND ended_at IS NULL"
                ),
                [workspace_id],
                session_from_row,
            )
            .optional()
        })
    }

    /// The provenance session `session_id`.
    pub fn provenance_session(
        &self,
        session_id: &str,
    ) -> Result<Option<ProvenanceSessionRow>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {SESSION_COLUMNS} FROM provenance_sessions WHERE session_id = ?1"),
                [session_id],
                session_from_row,
            )
            .optional()
        })
    }

    /// The live sessions of `workspace_ids`, with how many records each has,
    /// by workspace id.
    pub fn live_provenance_sessions(
        &self,
        workspace_ids: &[&str],
    ) -> Result<HashMap<String, (ProvenanceSessionRow, u64)>, DbError> {
        if workspace_ids.is_empty() {
            return Ok(HashMap::new());
        }
        let sql = format!(
            "SELECT {SESSION_COLUMNS},
                    (SELECT count(*) FROM provenance p WHERE p.session_id = s.session_id)
             FROM provenance_sessions s
             WHERE s.ended_at IS NULL AND s.workspace_id IN ({})",
            vec!["?"; workspace_ids.len()].join(", ")
        );
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(workspace_ids), |row| {
                let session = session_from_row(row)?;
                let count: i64 = row.get(19)?;
                Ok((
                    session.workspace_id.clone(),
                    (session, u64::try_from(count).unwrap_or(0)),
                ))
            })?
            .collect()
        })
    }

    /// Replaces the declared context of session `session_id`.
    pub fn declare_provenance(
        &self,
        session_id: &str,
        declared: &DeclaredColumns,
        at: Timestamp,
        by_agent: Option<&str>,
    ) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE provenance_sessions
                 SET model = ?2, harness = ?3, task = ?4, instruction_sha256 = ?5,
                     client_session = ?6, declared_at = ?7, declared_by_agent = ?8
                 WHERE session_id = ?1",
                params![
                    session_id,
                    declared.model,
                    declared.harness,
                    declared.task,
                    declared.instruction_sha256,
                    declared.client_session,
                    at,
                    by_agent
                ],
            )
            .map(drop)
        })
    }

    /// Records why session `session_id`'s records are unbound; `None`
    /// clears it.
    pub fn set_provenance_problem(
        &self,
        session_id: &str,
        problem: Option<&str>,
    ) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE provenance_sessions SET problem = ?2
                 WHERE session_id = ?1 AND problem IS NOT ?2",
                params![session_id, problem],
            )
            .map(drop)
        })
    }

    /// Counts one commit recorded with no record in session `session_id`.
    pub fn count_missing_record(&self, session_id: &str) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE provenance_sessions SET missing_records = missing_records + 1
                 WHERE session_id = ?1",
                [session_id],
            )
            .map(drop)
        })
    }

    /// Ends the live session of `workspace_id` at `at`, if it has one.
    pub fn end_provenance_session(&self, workspace_id: &str, at: Timestamp) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE provenance_sessions SET ended_at = ?2
                 WHERE workspace_id = ?1 AND ended_at IS NULL",
                params![workspace_id, at],
            )
            .map(drop)
        })
    }

    /// Stores `row`, signed by `key`, and moves its session's
    /// `next_sequence` past it, in one transaction. `false`, storing
    /// nothing, when its commit already has a record.
    pub fn insert_provenance(
        &self,
        key: &EvidenceKeyRow,
        row: &ProvenanceRow,
    ) -> Result<bool, DbError> {
        self.in_transaction(|tx| {
            tx.execute(
                "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
                 VALUES (?1, ?2, ?3, ?4) ON CONFLICT(id) DO NOTHING",
                params![key.id, key.algorithm, key.public_key, key.first_seen_at],
            )?;
            let inserted = tx.execute(
                "INSERT INTO provenance(repo_id, id, envelope, format, commit_id, change_id,
                                        tree_id, workspace_id, session_id, sequence, kind,
                                        key_id, agent, model, harness, task,
                                        instruction_sha256, client_session, context_digest,
                                        recorded_at)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19, ?20)
                 ON CONFLICT DO NOTHING",
                params![
                    row.repo_id,
                    row.id,
                    row.envelope,
                    row.format,
                    row.commit_id,
                    row.change_id,
                    row.tree_id,
                    row.workspace_id,
                    row.session_id,
                    row.sequence,
                    row.kind,
                    row.key_id,
                    row.agent,
                    row.declared.model,
                    row.declared.harness,
                    row.declared.task,
                    row.declared.instruction_sha256,
                    row.declared.client_session,
                    row.context_digest,
                    row.recorded_at,
                ],
            )?;
            if inserted == 0 {
                return Ok(false);
            }
            tx.execute(
                "UPDATE provenance_sessions SET next_sequence = max(next_sequence, ?2 + 1)
                 WHERE session_id = ?1",
                params![row.session_id, row.sequence],
            )?;
            Ok(true)
        })
    }

    /// The record of `repo`'s commit `commit_id`, if it has one.
    pub fn provenance_by_commit(
        &self,
        repo: RepoId,
        commit_id: &str,
    ) -> Result<Option<StoredProvenance>, DbError> {
        self.with(|conn| {
            conn.query_row(
                &format!("SELECT {COLUMNS} FROM {FROM} WHERE p.repo_id = ?1 AND p.commit_id = ?2"),
                params![repo, commit_id],
                from_row,
            )
            .optional()
        })
    }

    /// The record of `repo` whose id starts with `prefix` (8 to 64 hex
    /// digits, either case), as [`Db::evidence_by_prefix`] finds evidence.
    pub fn provenance_by_prefix(
        &self,
        repo: RepoId,
        prefix: &str,
    ) -> Result<StoredProvenance, DbError> {
        let pattern = format!("{}*", super::normalize_prefix_up_to(WHAT, prefix, 64)?);
        let matches = self.with(|conn| {
            let mut stmt = conn.prepare(&format!(
                "SELECT {COLUMNS} FROM {FROM}
                 WHERE p.repo_id = ?1 AND p.id GLOB ?2
                 ORDER BY p.id LIMIT {PREFIX_LOOKUP_LIMIT}"
            ))?;
            stmt.query_map(params![repo, pattern], from_row)?.collect()
        })?;
        unique_match(WHAT, prefix, matches, |found| found.row.id.clone())
    }

    /// At most `limit` of `repo`'s records matching `filter`, newest
    /// recorded first (then by id), after `cursor` (the `(recorded_at, id)`
    /// of the last one of the previous page).
    pub fn list_provenance(
        &self,
        repo: RepoId,
        filter: &ProvenanceFilter,
        cursor: Option<(Timestamp, &str)>,
        limit: usize,
    ) -> Result<Vec<StoredProvenance>, DbError> {
        let mut sql = format!("SELECT {COLUMNS} FROM {FROM} WHERE p.repo_id = ?");
        let mut values: Vec<SqlValue> = vec![SqlValue::Text(repo.as_hex())];
        let text = |value: &str| SqlValue::Text(value.to_owned());
        if !filter.change_ids.is_empty() {
            sql.push_str(" AND p.change_id IN (");
            sql.push_str(&vec!["?"; filter.change_ids.len()].join(", "));
            sql.push(')');
            values.extend(filter.change_ids.iter().map(|id| text(id)));
        }
        if let Some(workspace) = &filter.workspace_id {
            sql.push_str(" AND p.workspace_id = ?");
            values.push(text(workspace));
        }
        if let Some(session) = &filter.session_id {
            sql.push_str(" AND p.session_id = ?");
            values.push(text(session));
        }
        if let Some(since) = filter.since {
            sql.push_str(" AND p.recorded_at >= ?");
            values.push(SqlValue::Integer(since.0));
        }
        if let Some((at, id)) = cursor {
            sql.push_str(" AND (p.recorded_at < ? OR (p.recorded_at = ? AND p.id > ?))");
            values.extend([SqlValue::Integer(at.0), SqlValue::Integer(at.0), text(id)]);
        }
        sql.push_str(" ORDER BY p.recorded_at DESC, p.id LIMIT ?");
        values.push(SqlValue::Integer(i64::try_from(limit).unwrap_or(i64::MAX)));
        self.with(|conn| {
            let mut stmt = conn.prepare(&sql)?;
            stmt.query_map(params_from_iter(values), from_row)?
                .collect()
        })
    }

    /// How many records `repo` holds for the change `change_id` (R3.25).
    pub fn provenance_count_by_change(
        &self,
        repo: RepoId,
        change_id: &str,
    ) -> Result<u64, DbError> {
        self.with(|conn| {
            let count: i64 = conn.query_row(
                "SELECT COUNT(*) FROM provenance WHERE repo_id = ?1 AND change_id = ?2",
                params![repo, change_id],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(count).unwrap_or(0))
        })
    }

    /// The distinct change ids of `repo`'s records recorded before
    /// `before`: what GC's eighth step resolves in the head's view.
    pub fn provenance_changes_before(
        &self,
        repo: RepoId,
        before: Timestamp,
    ) -> Result<Vec<String>, DbError> {
        self.with(|conn| {
            let mut stmt = conn.prepare(
                "SELECT DISTINCT change_id FROM provenance
                 WHERE repo_id = ?1 AND recorded_at < ?2",
            )?;
            stmt.query_map(params![repo, before], |row| row.get(0))?
                .collect()
        })
    }

    /// GC's compaction (R3.04): the ids of `repo`'s records recorded before
    /// `before` that are neither the first nor the last of their run (the
    /// consecutive records of one session and change, by sequence, with one
    /// context digest), and whose commit is not in `visible`.
    pub fn provenance_to_compact(
        &self,
        repo: RepoId,
        before: Timestamp,
        visible: &HashSet<String>,
    ) -> Result<Vec<String>, DbError> {
        let ids: Vec<String> = self.in_transaction(|tx| {
            temp_set(tx, "gc_provenance_visible", visible)?;
            let mut stmt = tx.prepare(
                "WITH runs AS (
                   SELECT id, change_id, commit_id, recorded_at, context_digest,
                          LAG(context_digest) OVER w AS previous,
                          LEAD(context_digest) OVER w AS following
                   FROM provenance WHERE repo_id = ?1
                   WINDOW w AS (PARTITION BY session_id, change_id ORDER BY sequence))
                 SELECT id FROM runs
                 WHERE recorded_at < ?2
                   AND previous IS context_digest AND following IS context_digest
                   AND commit_id NOT IN (SELECT value FROM temp.gc_provenance_visible)
                   AND change_id NOT IN (SELECT change_id FROM landing_changes
                                         WHERE repo_id = ?1 AND landed_commit IS NOT NULL)",
            )?;
            let ids = stmt
                .query_map(params![repo, before], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            drop(stmt);
            tx.execute_batch("DROP TABLE IF EXISTS temp.gc_provenance_visible;")?;
            Ok(ids)
        })?;
        Ok(ids)
    }

    /// GC's expiry (R3.04): the ids of `repo`'s records recorded before
    /// `before` whose change is in `resolved` (looked up in the head's
    /// view) but not in `visible` (it has no visible commit).
    pub fn provenance_to_expire(
        &self,
        repo: RepoId,
        before: Timestamp,
        resolved: &HashSet<String>,
        visible: &HashSet<String>,
    ) -> Result<Vec<String>, DbError> {
        let gone: HashSet<&String> = resolved.difference(visible).collect();
        let ids: Vec<String> = self.in_transaction(|tx| {
            temp_set(tx, "gc_provenance_gone", gone)?;
            let mut stmt = tx.prepare(
                "SELECT id FROM provenance
                 WHERE repo_id = ?1 AND recorded_at < ?2
                   AND change_id IN (SELECT value FROM temp.gc_provenance_gone)
                   AND change_id NOT IN (SELECT change_id FROM landing_changes
                                         WHERE repo_id = ?1 AND landed_commit IS NOT NULL)",
            )?;
            let ids = stmt
                .query_map(params![repo, before], |row| row.get(0))?
                .collect::<rusqlite::Result<Vec<String>>>()?;
            drop(stmt);
            tx.execute_batch("DROP TABLE IF EXISTS temp.gc_provenance_gone;")?;
            Ok(ids)
        })?;
        Ok(ids)
    }

    /// Deletes `repo`'s records `ids`, 1,000 per transaction;
    /// how many there were.
    pub fn delete_provenance(&self, repo: RepoId, ids: &[String]) -> Result<u64, DbError> {
        let mut deleted = 0_u64;
        for batch in ids.chunks(SWEEP_BATCH) {
            deleted += self.in_transaction(|tx| {
                let mut stmt =
                    tx.prepare("DELETE FROM provenance WHERE repo_id = ?1 AND id = ?2")?;
                let mut n = 0_u64;
                for id in batch {
                    n += u64::try_from(stmt.execute(params![repo, id])?).unwrap_or(0);
                }
                Ok(n)
            })?;
        }
        Ok(deleted)
    }

    /// GC (R3.04): deletes `repo`'s provenance sessions that ended before
    /// `before` and have no record left. With `dry_run`, only counts.
    pub fn sweep_provenance_sessions(
        &self,
        repo: RepoId,
        before: Timestamp,
        dry_run: bool,
    ) -> Result<u64, DbError> {
        let condition = "repo_id = ?1 AND ended_at IS NOT NULL AND ended_at < ?2
             AND NOT EXISTS (SELECT 1 FROM provenance p
                             WHERE p.session_id = provenance_sessions.session_id)";
        self.with(|conn| {
            let n = if dry_run {
                conn.query_row(
                    &format!("SELECT count(*) FROM provenance_sessions WHERE {condition}"),
                    params![repo, before],
                    |row| row.get::<_, i64>(0),
                )?
            } else {
                i64::try_from(conn.execute(
                    &format!("DELETE FROM provenance_sessions WHERE {condition}"),
                    params![repo, before],
                )?)
                .unwrap_or(i64::MAX)
            };
            Ok(u64::try_from(n).unwrap_or(0))
        })
    }

    /// How many rows `provenance` and `provenance_sessions` have, all
    /// repositories together.
    pub fn provenance_counts(&self) -> Result<(u64, u64), DbError> {
        self.with(|conn| {
            let count = |table: &str| -> rusqlite::Result<u64> {
                conn.query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                    row.get::<_, i64>(0)
                })
                .map(|n| u64::try_from(n).unwrap_or(0))
            };
            Ok((count("provenance")?, count("provenance_sessions")?))
        })
    }

    /// Changes a record's column behind the daemon, for the tampering tests.
    #[doc(hidden)]
    pub fn tamper_provenance(
        &self,
        repo: RepoId,
        id: &str,
        column: &str,
        value: &SqlValue,
    ) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                &format!("UPDATE provenance SET {column} = ?3 WHERE repo_id = ?1 AND id = ?2"),
                params![repo, id, value],
            )
            .map(drop)
        })
    }

    /// Moves a record's time back, for the GC tests.
    #[doc(hidden)]
    pub fn age_provenance(&self, repo: RepoId, id: &str, at: Timestamp) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE provenance SET recorded_at = ?3 WHERE repo_id = ?1 AND id = ?2",
                params![repo, id, at],
            )
            .map(drop)
        })
    }
}
