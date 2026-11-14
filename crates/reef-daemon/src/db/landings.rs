//! Queries on `landings`, `landing_changes`, `landing_evidence` and
//! `landing_policies` (R3.13, ADR 0064).
//!
//! A landing row is written when the gate decides: `blocked`, or `queued`
//! once the enqueue transition is committed. The worker finishes it
//! `landed`, `dequeued` or `cancelled` ([`Db::finish_landing`]), which for a
//! landing also pins the evidence its changes' satisfied rules used, links
//! that evidence to the change, and keeps the policy bytes. All four tables
//! are per repository and deleted with it ([`Db::delete_repository`]); GC
//! maps operation ids ([`Db::map_landing_operations`]) and deletes nothing.

use std::collections::HashMap;
use std::fmt::Write as _;

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// One landing, as `landings` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandingRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `lnd_` and 16 hex digits.
    pub id: String,
    /// The destination bookmark.
    pub destination: String,
    /// Its head when read.
    pub destination_before: Option<String>,
    /// Its head after a landing.
    pub destination_after: Option<String>,
    /// `blocked`, `queued`, `landed`, `dequeued` or `cancelled`.
    pub outcome: String,
    /// For blocked and dequeued: `policy`, `conflicted`, `unlanded_parents`,
    /// `amended`, `destination`, `busy` or `failed`.
    pub reason: Option<String>,
    /// More about the reason.
    pub message: Option<String>,
    /// The commit the policy was read from.
    pub policy_commit: Option<String>,
    /// The policy file's blob id there.
    pub policy_blob: Option<String>,
    /// The SHA-256 of the bytes evaluated.
    pub policy_sha256: Option<String>,
    /// How many identical blocked requests it stands for.
    pub attempts: u32,
    /// The enqueue transition's operation.
    pub enqueue_operation_id: Option<String>,
    /// The land or dequeue transition's operation.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// The `reef-agent` that asked.
    pub agent: Option<String>,
    /// `human`, `agent` or `daemon`.
    pub actor: Option<String>,
    /// The identity that asked, by name.
    pub identity_name: Option<String>,
    /// The remote token that asked: id.
    pub remote_token: Option<String>,
    /// Its name then.
    pub remote_token_name: Option<String>,
    /// When it was asked for.
    pub created_at: Timestamp,
    /// When it finished.
    pub finished_at: Option<Timestamp>,
    /// The batch it is, or was last, in (R3.14).
    pub batch_id: Option<String>,
    /// The `[history]` table it landed under, as JSON (R3.16).
    pub history: Option<String>,
    /// When a push first carried its landed commit to the destination's git
    /// remote (R3.17).
    pub pushed_at: Option<Timestamp>,
    /// That push's operation.
    pub push_operation_id: Option<String>,
}

/// One change of a landing, as `landing_changes` holds it.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct LandingChangeRow {
    /// Its place in the landing, from 0.
    pub position: u32,
    /// The change, in hex.
    pub change_id: String,
    /// The commit it was queued at.
    pub queued_commit: String,
    /// That commit's tree.
    pub queued_tree: Option<String>,
    /// The commit that landed.
    pub landed_commit: Option<String>,
    /// Its tree.
    pub landed_tree: Option<String>,
    /// Whether it was rebased.
    pub rebased: bool,
    /// `{"paths": [...], "total": n}`.
    pub overlap: Option<String>,
    /// `{"paths": [...], "total": n}`.
    pub conflicts: Option<String>,
    /// `allow`, `block`, `no_policy` or `invalid_policy`.
    pub decision: Option<String>,
    /// `reef_policy::Evaluation` as JSON.
    pub evaluation: Option<String>,
    /// The SHA-256 of `evaluation`, for deduplicating blocked attempts.
    pub evaluation_sha256: Option<String>,
    /// The tree its `[[require]]` rules were judged on in its batch
    /// (R3.14).
    pub tested_tree: Option<String>,
    /// The trial commit carrying it.
    pub tested_commit: Option<String>,
    /// The evaluation over the tested tree's evidence, as JSON.
    pub tested_evaluation: Option<String>,
    /// Its position in its batch, from 1.
    pub batch_position: Option<u32>,
    /// Whether it landed with its description rewritten as a summary
    /// (R3.16).
    pub summarized: bool,
}

/// A landing and its changes.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredLanding {
    /// The landing.
    pub row: LandingRow,
    /// Its changes, in order.
    pub changes: Vec<LandingChangeRow>,
    /// The evidence it pinned: `(change, evidence id)`.
    pub pinned: Vec<(String, String)>,
}

/// What [`Db::landing_page`] selects.
#[derive(Clone, Debug, Default)]
pub struct LandingFilter {
    /// Destination bookmarks; empty for all.
    pub destinations: Vec<String>,
    /// Outcomes; empty for all.
    pub outcomes: Vec<String>,
    /// Change id prefixes; empty for all.
    pub changes: Vec<String>,
    /// Only landings that finished after this.
    pub finished_after: Option<Timestamp>,
}

const COLUMNS: &str = "repo_id, id, destination, destination_before, destination_after, outcome,
     reason, message, policy_commit, policy_blob, policy_sha256, attempts, enqueue_operation_id,
     operation_id, operation_collected, agent, actor, identity_name, remote_token,
     remote_token_name, created_at, finished_at, batch_id, history, pushed_at, push_operation_id";

const CHANGE_COLUMNS: &str = "position, change_id, queued_commit, queued_tree, landed_commit,
     landed_tree, rebased, overlap, conflicts, decision, evaluation, evaluation_sha256,
     tested_tree, tested_commit, tested_evaluation, batch_position, summarized";

fn landing_from_row(row: &Row<'_>) -> rusqlite::Result<LandingRow> {
    Ok(LandingRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        destination: row.get(2)?,
        destination_before: row.get(3)?,
        destination_after: row.get(4)?,
        outcome: row.get(5)?,
        reason: row.get(6)?,
        message: row.get(7)?,
        policy_commit: row.get(8)?,
        policy_blob: row.get(9)?,
        policy_sha256: row.get(10)?,
        attempts: u32::try_from(row.get::<_, i64>(11)?).unwrap_or(u32::MAX),
        enqueue_operation_id: row.get(12)?,
        operation_id: row.get(13)?,
        operation_collected: row.get::<_, i64>(14)? != 0,
        agent: row.get(15)?,
        actor: row.get(16)?,
        identity_name: row.get(17)?,
        remote_token: row.get(18)?,
        remote_token_name: row.get(19)?,
        created_at: row.get(20)?,
        finished_at: row.get(21)?,
        batch_id: row.get(22)?,
        history: row.get(23)?,
        pushed_at: row.get(24)?,
        push_operation_id: row.get(25)?,
    })
}

fn change_from_row(row: &Row<'_>) -> rusqlite::Result<LandingChangeRow> {
    Ok(LandingChangeRow {
        position: u32::try_from(row.get::<_, i64>(0)?).unwrap_or(u32::MAX),
        change_id: row.get(1)?,
        queued_commit: row.get(2)?,
        queued_tree: row.get(3)?,
        landed_commit: row.get(4)?,
        landed_tree: row.get(5)?,
        rebased: row.get::<_, i64>(6)? != 0,
        overlap: row.get(7)?,
        conflicts: row.get(8)?,
        decision: row.get(9)?,
        evaluation: row.get(10)?,
        evaluation_sha256: row.get(11)?,
        tested_tree: row.get(12)?,
        tested_commit: row.get(13)?,
        tested_evaluation: row.get(14)?,
        batch_position: row
            .get::<_, Option<i64>>(15)?
            .and_then(|position| u32::try_from(position).ok()),
        summarized: row.get::<_, i64>(16)? != 0,
    })
}

fn write_landing_in(tx: &Transaction<'_>, row: &LandingRow) -> rusqlite::Result<()> {
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO landings({COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                     ?17, ?18, ?19, ?20, ?21, ?22, ?23, ?24, ?25, ?26)"
        ),
        params![
            row.repo_id,
            row.id,
            row.destination,
            row.destination_before,
            row.destination_after,
            row.outcome,
            row.reason,
            row.message,
            row.policy_commit,
            row.policy_blob,
            row.policy_sha256,
            i64::from(row.attempts),
            row.enqueue_operation_id,
            row.operation_id,
            i64::from(row.operation_collected),
            row.agent,
            row.actor,
            row.identity_name,
            row.remote_token,
            row.remote_token_name,
            row.created_at,
            row.finished_at,
            row.batch_id,
            row.history,
            row.pushed_at,
            row.push_operation_id,
        ],
    )?;
    Ok(())
}

fn write_changes_in(
    tx: &Transaction<'_>,
    repo: RepoId,
    landing: &str,
    changes: &[LandingChangeRow],
) -> rusqlite::Result<()> {
    tx.execute(
        "DELETE FROM landing_changes WHERE repo_id = ?1 AND landing_id = ?2",
        params![repo, landing],
    )?;
    for change in changes {
        tx.execute(
            &format!(
                "INSERT INTO landing_changes(repo_id, landing_id, {CHANGE_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16,
                         ?17, ?18, ?19)"
            ),
            params![
                repo,
                landing,
                i64::from(change.position),
                change.change_id,
                change.queued_commit,
                change.queued_tree,
                change.landed_commit,
                change.landed_tree,
                i64::from(change.rebased),
                change.overlap,
                change.conflicts,
                change.decision,
                change.evaluation,
                change.evaluation_sha256,
                change.tested_tree,
                change.tested_commit,
                change.tested_evaluation,
                change.batch_position.map(i64::from),
                i64::from(change.summarized),
            ],
        )?;
    }
    Ok(())
}

/// Appends ` AND <expr> IN (?, ...)` for `values`.
fn push_in(sql: &mut String, args: &mut Vec<SqlValue>, expr: &str, values: &[String]) {
    if values.is_empty() {
        return;
    }
    let marks = vec!["?"; values.len()].join(", ");
    let _ = write!(sql, " AND {expr} IN ({marks})");
    args.extend(values.iter().cloned().map(SqlValue::Text));
}

impl Db {
    /// Writes `row` and its `changes`, replacing any row with its id.
    pub fn write_landing(
        &self,
        row: &LandingRow,
        changes: &[LandingChangeRow],
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            write_landing_in(tx, row)?;
            write_changes_in(tx, row.repo_id, &row.id, changes)?;
            Ok(())
        })
    }

    /// Finishes a landing: writes `row` and its `changes`, and for a landed
    /// one pins `pins` (`(change, evidence id)`, linked to the change at
    /// `commit`), and keeps `policy` (`(sha256, bytes)`), in one
    /// transaction.
    pub fn finish_landing(
        &self,
        row: &LandingRow,
        changes: &[LandingChangeRow],
        pins: &[(String, String)],
        policy: Option<(&str, &[u8])>,
    ) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            write_landing_in(tx, row)?;
            write_changes_in(tx, row.repo_id, &row.id, changes)?;
            let at = row.finished_at.unwrap_or(row.created_at);
            for (change, evidence) in pins {
                tx.execute(
                    "INSERT INTO landing_evidence(repo_id, landing_id, change_id, evidence_id)
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                    params![row.repo_id, row.id, change, evidence],
                )?;
                let commit = changes
                    .iter()
                    .find(|c| c.change_id == *change)
                    .and_then(|c| c.landed_commit.clone());
                tx.execute(
                    "INSERT INTO evidence_links(repo_id, change_id, evidence_id, commit_id, linked_at)
                     SELECT ?1, ?2, ?3, ?4, ?5
                     WHERE EXISTS (SELECT 1 FROM evidence WHERE repo_id = ?1 AND id = ?3)
                     ON CONFLICT DO NOTHING",
                    params![row.repo_id, change, evidence, commit, at],
                )?;
            }
            if let Some((sha256, bytes)) = policy {
                tx.execute(
                    "INSERT INTO landing_policies(repo_id, sha256, bytes, first_used_at)
                     VALUES (?1, ?2, ?3, ?4) ON CONFLICT DO NOTHING",
                    params![row.repo_id, sha256, bytes, at],
                )?;
            }
            Ok(())
        })
    }

    /// Counts one more identical blocked request on landing `id`.
    pub fn bump_landing_attempts(
        &self,
        repo: RepoId,
        id: &str,
        at: Timestamp,
    ) -> Result<(), DbError> {
        self.with(|conn| {
            conn.execute(
                "UPDATE landings SET attempts = attempts + 1, finished_at = ?3
                 WHERE repo_id = ?1 AND id = ?2",
                params![repo, id, at],
            )?;
            Ok(())
        })
    }

    fn landings_where(
        &self,
        tail: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<StoredLanding>, DbError> {
        self.with(|conn| {
            let rows: Vec<LandingRow> = {
                let mut statement =
                    conn.prepare(&format!("SELECT {COLUMNS} FROM landings l {tail}"))?;
                statement
                    .query_map(args, landing_from_row)?
                    .collect::<Result<_, _>>()?
            };
            let mut out = Vec::with_capacity(rows.len());
            let mut changes = conn.prepare(&format!(
                "SELECT {CHANGE_COLUMNS} FROM landing_changes
                 WHERE repo_id = ?1 AND landing_id = ?2 ORDER BY position"
            ))?;
            let mut pins = conn.prepare(
                "SELECT change_id, evidence_id FROM landing_evidence
                 WHERE repo_id = ?1 AND landing_id = ?2 ORDER BY change_id, evidence_id",
            )?;
            for row in rows {
                let landing_changes = changes
                    .query_map(params![row.repo_id, row.id], change_from_row)?
                    .collect::<Result<_, _>>()?;
                let pinned = pins
                    .query_map(params![row.repo_id, row.id], |r| Ok((r.get(0)?, r.get(1)?)))?
                    .collect::<Result<_, _>>()?;
                out.push(StoredLanding {
                    row,
                    changes: landing_changes,
                    pinned,
                });
            }
            Ok(out)
        })
    }

    /// Landing `id`.
    pub fn landing(&self, repo: RepoId, id: &str) -> Result<Option<StoredLanding>, DbError> {
        Ok(self
            .landings_where("WHERE repo_id = ?1 AND id = ?2", &[&repo, &id])?
            .into_iter()
            .next())
    }

    /// The latest landing of the change whose id starts with `change`.
    pub fn latest_landing_of(
        &self,
        repo: RepoId,
        change: &str,
    ) -> Result<Option<StoredLanding>, DbError> {
        let prefix = format!("{change}%");
        Ok(self
            .landings_where(
                "WHERE repo_id = ?1 AND id IN (SELECT landing_id FROM landing_changes
                   WHERE repo_id = ?1 AND change_id LIKE ?2)
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                &[&repo, &prefix],
            )?
            .into_iter()
            .next())
    }

    /// The queued landing of `change`, if any.
    pub fn open_landing_of(
        &self,
        repo: RepoId,
        change: &str,
    ) -> Result<Option<StoredLanding>, DbError> {
        Ok(self
            .landings_where(
                "WHERE repo_id = ?1 AND outcome = 'queued' AND id IN (
                   SELECT landing_id FROM landing_changes WHERE repo_id = ?1 AND change_id = ?2)
                 ORDER BY created_at DESC LIMIT 1",
                &[&repo, &change],
            )?
            .into_iter()
            .next())
    }

    /// Every queued landing of `repo`, oldest first.
    pub fn open_landings(&self, repo: RepoId) -> Result<Vec<StoredLanding>, DbError> {
        self.landings_where(
            "WHERE repo_id = ?1 AND outcome = 'queued' ORDER BY created_at, id",
            &[&repo],
        )
    }

    /// How many landings of `repo` are queued.
    pub fn queued_landing_count(&self, repo: RepoId) -> Result<u64, DbError> {
        self.with(|conn| {
            let n: i64 = conn.query_row(
                "SELECT count(*) FROM landings WHERE repo_id = ?1 AND outcome = 'queued'",
                [repo],
                |row| row.get(0),
            )?;
            Ok(u64::try_from(n).unwrap_or(0))
        })
    }

    /// A page of `repo`'s landings matching `filter`, newest first, after
    /// `cursor` (`created_at`, id).
    pub fn landing_page(
        &self,
        repo: RepoId,
        filter: &LandingFilter,
        cursor: Option<(Timestamp, &str)>,
        limit: usize,
    ) -> Result<Vec<StoredLanding>, DbError> {
        let mut sql = String::from("WHERE repo_id = ?");
        let mut args: Vec<SqlValue> = vec![SqlValue::Text(repo.to_string())];
        push_in(&mut sql, &mut args, "destination", &filter.destinations);
        push_in(&mut sql, &mut args, "outcome", &filter.outcomes);
        if !filter.changes.is_empty() {
            let likes = vec!["change_id LIKE ?"; filter.changes.len()].join(" OR ");
            let _ = write!(
                sql,
                " AND id IN (SELECT landing_id FROM landing_changes c
                   WHERE c.repo_id = l.repo_id AND ({likes}))"
            );
            args.extend(
                filter
                    .changes
                    .iter()
                    .map(|c| SqlValue::Text(format!("{c}%"))),
            );
        }
        if let Some(after) = filter.finished_after {
            sql.push_str(" AND finished_at > ?");
            args.push(SqlValue::Integer(after.0));
        }
        if let Some((at, id)) = cursor {
            sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
            args.push(SqlValue::Integer(at.0));
            args.push(SqlValue::Integer(at.0));
            args.push(SqlValue::Text(id.to_owned()));
        }
        let _ = write!(sql, " ORDER BY created_at DESC, id DESC LIMIT {limit}");
        self.with(|conn| {
            let ids: Vec<String> = {
                let mut statement = conn.prepare(&format!("SELECT id FROM landings l {sql}"))?;
                statement
                    .query_map(params_from_iter(args.iter()), |row| row.get(0))?
                    .collect::<Result<_, _>>()?
            };
            Ok(ids)
        })
        .and_then(|ids| {
            let mut out = Vec::with_capacity(ids.len());
            for id in ids {
                if let Some(landing) = self.landing(repo, &id)? {
                    out.push(landing);
                }
            }
            Ok(out)
        })
    }

    /// The policy bytes `repo`'s landings kept under `sha256`.
    pub fn landing_policy(&self, repo: RepoId, sha256: &str) -> Result<Option<Vec<u8>>, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT bytes FROM landing_policies WHERE repo_id = ?1 AND sha256 = ?2",
                params![repo, sha256],
                |row| row.get(0),
            )
            .optional()
        })
    }

    /// GC's step: maps each landing's operation ids through `rewritten`; an
    /// operation not in it was dropped (`operation_collected`). Returns
    /// (mapped, collected).
    pub fn map_landing_operations(
        &self,
        repo: RepoId,
        rewritten: &HashMap<String, String>,
    ) -> Result<(usize, usize), DbError> {
        self.in_transaction(|tx| {
            let rows: Vec<(String, Option<String>, Option<String>)> = {
                let mut statement = tx.prepare(
                    "SELECT id, enqueue_operation_id,
                            CASE WHEN operation_collected = 0 THEN operation_id END
                     FROM landings
                     WHERE repo_id = ?1
                       AND (enqueue_operation_id IS NOT NULL
                            OR (operation_id IS NOT NULL AND operation_collected = 0))",
                )?;
                statement
                    .query_map([repo], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))?
                    .collect::<Result<_, _>>()?
            };
            let (mut mapped, mut collected) = (0, 0);
            for (id, enqueue, operation) in rows {
                let enqueue = enqueue.map(|old| rewritten.get(&old).cloned().unwrap_or(old));
                let (operation, gone) = match operation {
                    Some(old) => match rewritten.get(&old) {
                        Some(new) => (Some(new.clone()), false),
                        None => (Some(old), true),
                    },
                    None => (None, false),
                };
                // An enqueue operation GC dropped keeps its old id;
                // `operation_collected` is about `operation_id`, and a row
                // already collected keeps its old id and flag.
                tx.execute(
                    "UPDATE landings SET enqueue_operation_id = ?3,
                       operation_id = COALESCE(?4, operation_id),
                       operation_collected = MAX(operation_collected, ?5)
                     WHERE repo_id = ?1 AND id = ?2",
                    params![repo, id, enqueue, operation, i64::from(gone)],
                )?;
                if gone {
                    collected += 1;
                } else {
                    mapped += 1;
                }
            }
            Ok((mapped, collected))
        })
    }

    /// Every repository with a queued landing or a queued change.
    pub fn repos_with_queued(&self) -> Result<Vec<RepoId>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT repo_id FROM landings WHERE outcome = 'queued'
                 UNION SELECT repo_id FROM lifecycle WHERE state = 'queued'",
            )?;
            statement
                .query_map([], |row| row.get(0))?
                .collect::<Result<_, _>>()
        })
    }

    /// `repo`'s queued changes with no queued landing: a crash cut the
    /// landing row after the enqueue.
    pub fn queued_without_landing(
        &self,
        repo: RepoId,
    ) -> Result<Vec<super::LifecycleRow>, DbError> {
        let rows = self.lifecycle_page(
            repo,
            &super::LifecycleFilter {
                states: vec!["queued".to_owned()],
                ..super::LifecycleFilter::default()
            },
            None,
            usize::try_from(crate::landing::MAX_QUEUED).unwrap_or(1000),
        )?;
        let mut out = Vec::new();
        for row in rows {
            if self.open_landing_of(repo, &row.change_id)?.is_none() {
                out.push(row);
            }
        }
        Ok(out)
    }
}
