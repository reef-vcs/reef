//! Queries on `landing_batches`, `landing_batch_members` and
//! `landing_batch_trials` (R3.14, ADR 0067).
//!
//! A batch row is written as the batch moves (formed, each trial, each
//! result, the end) and never pending: the lifecycle rows and the landing
//! rows stay the record of what landed. All three tables are per repository
//! and deleted with it; GC maps operation ids
//! ([`Db::map_batch_operations`]) and deletes nothing.

use std::collections::HashMap;
use std::fmt::Write as _;

use rusqlite::types::Value as SqlValue;
use rusqlite::{OptionalExtension as _, Row, Transaction, params, params_from_iter};

use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// One batch, as `landing_batches` holds it.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchRow {
    /// The repository.
    pub repo_id: RepoId,
    /// `bat_` and 16 hex digits.
    pub id: String,
    /// The destination bookmark.
    pub destination: String,
    /// Its head when the batch formed.
    pub base_commit: String,
    /// `testing`, `bisecting` or `finished`.
    pub state: String,
    /// For a finished batch: `landed`, `split`, `unavailable`, `reformed`,
    /// `interrupted` or `busy`.
    pub outcome: Option<String>,
    /// How many members.
    pub size: u32,
    /// For `split`, the culprit's position.
    pub culprit_position: Option<u32>,
    /// How many members landed.
    pub landed_count: u32,
    /// The policy's `[landing] checks`: `batch` or `proposal`.
    pub checks: String,
    /// The commit whose policy formed the batch.
    pub policy_commit: Option<String>,
    /// The SHA-256 of that policy's bytes.
    pub policy_sha256: Option<String>,
    /// The land operation.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// More about how it ended.
    pub message: Option<String>,
    /// When it formed.
    pub created_at: Timestamp,
    /// When it finished.
    pub finished_at: Option<Timestamp>,
    /// The `[history]` table it formed under, as JSON (#297).
    pub history: Option<String>,
}

/// One member of a batch.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct BatchMemberRow {
    /// Its position in the chain, from 1.
    pub position: u32,
    /// Its landing.
    pub landing_id: String,
    /// The change, in hex.
    pub change_id: String,
    /// The commit it was queued at.
    pub queued_commit: String,
    /// The chain's tree with it applied.
    pub planned_tree: String,
    /// `landed`, `culprit`, `requeued`, `dequeued`, `left` or
    /// `unavailable`; `None` while undecided.
    pub result: Option<String>,
}

/// One tested prefix of a batch.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct BatchTrialRow {
    /// The order it was tested in, from 0.
    pub seq: u32,
    /// How many members its tree has.
    pub prefix: u32,
    /// The tested tree.
    pub tree: String,
    /// The trial commit carrying it.
    pub commit_id: Option<String>,
    /// The operation that wrote the trial commit.
    pub operation_id: Option<String>,
    /// GC dropped `operation_id`.
    pub operation_collected: bool,
    /// `passed`, `failed`, `unavailable` or `cancelled`; `None` while open.
    pub result: Option<String>,
    /// The executor jobs asked for its missing evidence.
    pub job_ids: Vec<String>,
    /// `{"needs": [...], "total": n}`: the failed or missing needs.
    pub needs: Option<String>,
    /// How many times its jobs were asked for.
    pub attempts: u32,
    /// When it started.
    pub created_at: Timestamp,
    /// When it ended.
    pub finished_at: Option<Timestamp>,
}

/// A batch with its members and trials.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoredBatch {
    /// The batch.
    pub row: BatchRow,
    /// Its members, in chain order.
    pub members: Vec<BatchMemberRow>,
    /// Its trials, in the order tested.
    pub trials: Vec<BatchTrialRow>,
}

/// What [`Db::batch_page`] selects.
#[derive(Clone, Debug, Default)]
pub struct BatchFilter {
    /// Destination bookmarks; empty for all.
    pub destinations: Vec<String>,
    /// Only batches not finished.
    pub open_only: bool,
    /// Only batches that finished after this, or are open.
    pub finished_after: Option<Timestamp>,
}

const COLUMNS: &str = "repo_id, id, destination, base_commit, state, outcome, size,
     culprit_position, landed_count, checks, policy_commit, policy_sha256, operation_id,
     operation_collected, message, created_at, finished_at, history";

const MEMBER_COLUMNS: &str = "position, landing_id, change_id, queued_commit, planned_tree, result";

const TRIAL_COLUMNS: &str = "seq, prefix, tree, commit_id, operation_id, operation_collected,
     result, job_ids, needs, attempts, created_at, finished_at";

fn count(value: i64) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn batch_from_row(row: &Row<'_>) -> rusqlite::Result<BatchRow> {
    Ok(BatchRow {
        repo_id: row.get(0)?,
        id: row.get(1)?,
        destination: row.get(2)?,
        base_commit: row.get(3)?,
        state: row.get(4)?,
        outcome: row.get(5)?,
        size: count(row.get(6)?),
        culprit_position: row.get::<_, Option<i64>>(7)?.map(count),
        landed_count: count(row.get(8)?),
        checks: row.get(9)?,
        policy_commit: row.get(10)?,
        policy_sha256: row.get(11)?,
        operation_id: row.get(12)?,
        operation_collected: row.get::<_, i64>(13)? != 0,
        message: row.get(14)?,
        created_at: row.get(15)?,
        finished_at: row.get(16)?,
        history: row.get(17)?,
    })
}

fn member_from_row(row: &Row<'_>) -> rusqlite::Result<BatchMemberRow> {
    Ok(BatchMemberRow {
        position: count(row.get(0)?),
        landing_id: row.get(1)?,
        change_id: row.get(2)?,
        queued_commit: row.get(3)?,
        planned_tree: row.get(4)?,
        result: row.get(5)?,
    })
}

fn trial_from_row(row: &Row<'_>) -> rusqlite::Result<BatchTrialRow> {
    let job_ids: String = row.get(7)?;
    Ok(BatchTrialRow {
        seq: count(row.get(0)?),
        prefix: count(row.get(1)?),
        tree: row.get(2)?,
        commit_id: row.get(3)?,
        operation_id: row.get(4)?,
        operation_collected: row.get::<_, i64>(5)? != 0,
        result: row.get(6)?,
        job_ids: serde_json::from_str(&job_ids).unwrap_or_default(),
        needs: row.get(8)?,
        attempts: count(row.get(9)?),
        created_at: row.get(10)?,
        finished_at: row.get(11)?,
    })
}

/// `ours`, unless GC mapped (or collected) the operation id stored meanwhile:
/// the batch writes an id once, so a different stored one is GC's.
fn merged(ours: &mut Option<String>, collected: &mut bool, stored: Option<(Option<String>, bool)>) {
    if let Some((theirs, theirs_collected)) = stored {
        if ours.is_some() && theirs.is_some() && *ours != theirs {
            *ours = theirs;
        }
        *collected |= theirs_collected;
    }
}

#[expect(clippy::too_many_lines, reason = "the merge, then three tables")]
fn write_batch_in(tx: &Transaction<'_>, batch: &StoredBatch) -> rusqlite::Result<()> {
    let mut batch = batch.clone();
    {
        let stored: Option<(Option<String>, bool)> = tx
            .query_row(
                "SELECT operation_id, operation_collected FROM landing_batches
                 WHERE repo_id = ?1 AND id = ?2",
                params![batch.row.repo_id, batch.row.id],
                |row| Ok((row.get(0)?, row.get::<_, i64>(1)? != 0)),
            )
            .optional()?;
        merged(
            &mut batch.row.operation_id,
            &mut batch.row.operation_collected,
            stored,
        );
        let mut statement = tx.prepare(
            "SELECT seq, operation_id, operation_collected FROM landing_batch_trials
             WHERE repo_id = ?1 AND batch_id = ?2",
        )?;
        let trials: HashMap<u32, (Option<String>, bool)> = statement
            .query_map(params![batch.row.repo_id, batch.row.id], |row| {
                Ok((
                    count(row.get(0)?),
                    (row.get(1)?, row.get::<_, i64>(2)? != 0),
                ))
            })?
            .collect::<Result<_, _>>()?;
        for trial in &mut batch.trials {
            merged(
                &mut trial.operation_id,
                &mut trial.operation_collected,
                trials.get(&trial.seq).cloned(),
            );
        }
    }
    let batch = &batch;
    let row = &batch.row;
    tx.execute(
        &format!(
            "INSERT OR REPLACE INTO landing_batches({COLUMNS})
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17, ?18)"
        ),
        params![
            row.repo_id,
            row.id,
            row.destination,
            row.base_commit,
            row.state,
            row.outcome,
            i64::from(row.size),
            row.culprit_position.map(i64::from),
            i64::from(row.landed_count),
            row.checks,
            row.policy_commit,
            row.policy_sha256,
            row.operation_id,
            i64::from(row.operation_collected),
            row.message,
            row.created_at,
            row.finished_at,
            row.history,
        ],
    )?;
    tx.execute(
        "DELETE FROM landing_batch_members WHERE repo_id = ?1 AND batch_id = ?2",
        params![row.repo_id, row.id],
    )?;
    for member in &batch.members {
        tx.execute(
            &format!(
                "INSERT INTO landing_batch_members(repo_id, batch_id, {MEMBER_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)"
            ),
            params![
                row.repo_id,
                row.id,
                i64::from(member.position),
                member.landing_id,
                member.change_id,
                member.queued_commit,
                member.planned_tree,
                member.result,
            ],
        )?;
    }
    tx.execute(
        "DELETE FROM landing_batch_trials WHERE repo_id = ?1 AND batch_id = ?2",
        params![row.repo_id, row.id],
    )?;
    for trial in &batch.trials {
        tx.execute(
            &format!(
                "INSERT INTO landing_batch_trials(repo_id, batch_id, {TRIAL_COLUMNS})
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)"
            ),
            params![
                row.repo_id,
                row.id,
                i64::from(trial.seq),
                i64::from(trial.prefix),
                trial.tree,
                trial.commit_id,
                trial.operation_id,
                i64::from(trial.operation_collected),
                trial.result,
                serde_json::to_string(&trial.job_ids).unwrap_or_else(|_| "[]".to_owned()),
                trial.needs,
                i64::from(trial.attempts),
                trial.created_at,
                trial.finished_at,
            ],
        )?;
    }
    Ok(())
}

impl Db {
    /// Writes `batch` whole, replacing its members and trials, and sets
    /// `landings.batch_id` of each member's landing to it.
    /// Nothing is written for a repository that was forgotten meanwhile.
    pub fn write_batch(&self, batch: &StoredBatch) -> Result<(), DbError> {
        self.in_transaction(|tx| {
            let exists: bool = tx.query_row(
                "SELECT EXISTS (SELECT 1 FROM repositories WHERE id = ?1)",
                [batch.row.repo_id],
                |row| row.get(0),
            )?;
            if !exists {
                return Ok(());
            }
            write_batch_in(tx, batch)?;
            for member in &batch.members {
                tx.execute(
                    "UPDATE landings SET batch_id = ?3 WHERE repo_id = ?1 AND id = ?2",
                    params![batch.row.repo_id, member.landing_id, batch.row.id],
                )?;
            }
            Ok(())
        })
    }

    fn batches_where(
        &self,
        tail: &str,
        args: &[&dyn rusqlite::ToSql],
    ) -> Result<Vec<StoredBatch>, DbError> {
        self.with(|conn| {
            let rows: Vec<BatchRow> = {
                let mut statement =
                    conn.prepare(&format!("SELECT {COLUMNS} FROM landing_batches b {tail}"))?;
                statement
                    .query_map(args, batch_from_row)?
                    .collect::<Result<_, _>>()?
            };
            let mut members = conn.prepare(&format!(
                "SELECT {MEMBER_COLUMNS} FROM landing_batch_members
                 WHERE repo_id = ?1 AND batch_id = ?2 ORDER BY position"
            ))?;
            let mut trials = conn.prepare(&format!(
                "SELECT {TRIAL_COLUMNS} FROM landing_batch_trials
                 WHERE repo_id = ?1 AND batch_id = ?2 ORDER BY seq"
            ))?;
            let mut out = Vec::with_capacity(rows.len());
            for row in rows {
                let batch_members = members
                    .query_map(params![row.repo_id, row.id], member_from_row)?
                    .collect::<Result<_, _>>()?;
                let batch_trials = trials
                    .query_map(params![row.repo_id, row.id], trial_from_row)?
                    .collect::<Result<_, _>>()?;
                out.push(StoredBatch {
                    row,
                    members: batch_members,
                    trials: batch_trials,
                });
            }
            Ok(out)
        })
    }

    /// Batch `id`.
    pub fn batch(&self, repo: RepoId, id: &str) -> Result<Option<StoredBatch>, DbError> {
        Ok(self
            .batches_where("WHERE repo_id = ?1 AND id = ?2", &[&repo, &id])?
            .into_iter()
            .next())
    }

    /// The latest batch the change whose id starts with `change` was in.
    pub fn latest_batch_of(
        &self,
        repo: RepoId,
        change: &str,
    ) -> Result<Option<StoredBatch>, DbError> {
        let prefix = format!("{change}%");
        Ok(self
            .batches_where(
                "WHERE repo_id = ?1 AND id IN (SELECT batch_id FROM landing_batch_members
                   WHERE repo_id = ?1 AND change_id LIKE ?2)
                 ORDER BY created_at DESC, id DESC LIMIT 1",
                &[&repo, &prefix],
            )?
            .into_iter()
            .next())
    }

    /// Every batch not finished, in every repository: `(repo, id)`.
    pub fn open_batches(&self) -> Result<Vec<(RepoId, String)>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT repo_id, id FROM landing_batches WHERE state != 'finished'
                 ORDER BY created_at, id",
            )?;
            statement
                .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?
                .collect::<Result<_, _>>()
        })
    }

    /// A trial commit already written onto `base` with `tree`, if any batch
    /// of `repo` has one: `(commit, operation)`.
    pub fn trial_commit_for(
        &self,
        repo: RepoId,
        base: &str,
        tree: &str,
    ) -> Result<Option<(String, Option<String>)>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT t.commit_id, t.operation_id FROM landing_batch_trials t
                 JOIN landing_batches b ON b.repo_id = t.repo_id AND b.id = t.batch_id
                 WHERE t.repo_id = ?1 AND t.tree = ?3 AND b.base_commit = ?2
                   AND t.commit_id IS NOT NULL
                 ORDER BY t.created_at DESC LIMIT 1",
            )?;
            let mut rows = statement.query_map(params![repo, base, tree], |row| {
                Ok((row.get(0)?, row.get(1)?))
            })?;
            rows.next().transpose()
        })
    }

    /// Whether an executor claimed a job of `repo` at or after `since`.
    pub fn job_claimed_since(&self, repo: RepoId, since: Timestamp) -> Result<bool, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT EXISTS (SELECT 1 FROM jobs WHERE repo_id = ?1 AND claimed_at >= ?2)",
                params![repo, since],
                |row| row.get(0),
            )
        })
    }

    /// The trial commits of `repo`'s batches interrupted in the last week
    /// that no open batch uses, for abandoning those still visible.
    pub fn interrupted_trial_commits(&self, repo: RepoId) -> Result<Vec<String>, DbError> {
        let since = Timestamp(Timestamp::now().0 - 7 * 24 * 60 * 60 * 1000);
        self.with(|conn| {
            let mut statement = conn.prepare(
                "SELECT DISTINCT t.commit_id FROM landing_batch_trials t
                 JOIN landing_batches b ON b.repo_id = t.repo_id AND b.id = t.batch_id
                 WHERE t.repo_id = ?1 AND t.commit_id IS NOT NULL
                   AND b.outcome = 'interrupted'
                   AND b.finished_at > ?2
                   AND t.commit_id NOT IN (
                     SELECT u.commit_id FROM landing_batch_trials u
                     JOIN landing_batches o ON o.repo_id = u.repo_id AND o.id = u.batch_id
                     WHERE u.repo_id = ?1 AND u.commit_id IS NOT NULL AND o.state != 'finished')
                 ORDER BY t.commit_id",
            )?;
            statement
                .query_map(params![repo, since], |row| row.get(0))?
                .collect::<Result<_, _>>()
        })
    }

    /// A page of `repo`'s batches matching `filter`, newest first, after
    /// `cursor` (`created_at`, id).
    pub fn batch_page(
        &self,
        repo: RepoId,
        filter: &BatchFilter,
        cursor: Option<(Timestamp, &str)>,
        limit: usize,
    ) -> Result<Vec<StoredBatch>, DbError> {
        let mut sql = String::from("WHERE repo_id = ?");
        let mut args: Vec<SqlValue> = vec![SqlValue::Text(repo.to_string())];
        if !filter.destinations.is_empty() {
            let marks = vec!["?"; filter.destinations.len()].join(", ");
            let _ = write!(sql, " AND destination IN ({marks})");
            args.extend(filter.destinations.iter().cloned().map(SqlValue::Text));
        }
        if filter.open_only {
            sql.push_str(" AND state != 'finished'");
        }
        if let Some(after) = filter.finished_after {
            sql.push_str(" AND (finished_at IS NULL OR finished_at > ?)");
            args.push(SqlValue::Integer(after.0));
        }
        if let Some((at, id)) = cursor {
            sql.push_str(" AND (created_at < ? OR (created_at = ? AND id < ?))");
            args.push(SqlValue::Integer(at.0));
            args.push(SqlValue::Integer(at.0));
            args.push(SqlValue::Text(id.to_owned()));
        }
        let _ = write!(sql, " ORDER BY created_at DESC, id DESC LIMIT {limit}");
        let ids: Vec<String> = self.with(|conn| {
            let mut statement = conn.prepare(&format!("SELECT id FROM landing_batches b {sql}"))?;
            statement
                .query_map(params_from_iter(args.iter()), |row| row.get(0))?
                .collect::<Result<_, _>>()
        })?;
        let mut out = Vec::with_capacity(ids.len());
        for id in ids {
            if let Some(batch) = self.batch(repo, &id)? {
                out.push(batch);
            }
        }
        Ok(out)
    }

    /// GC's step: maps each batch's and trial's operation id through
    /// `rewritten`; one not in it was dropped (`operation_collected`).
    /// Returns (mapped, collected).
    pub fn map_batch_operations(
        &self,
        repo: RepoId,
        rewritten: &HashMap<String, String>,
    ) -> Result<(usize, usize), DbError> {
        self.in_transaction(|tx| {
            let (mut mapped, mut collected) = (0, 0);
            for (table, key) in [
                ("landing_batches", "id"),
                ("landing_batch_trials", "batch_id || ':' || seq"),
            ] {
                let rows: Vec<(String, String)> = {
                    let mut statement = tx.prepare(&format!(
                        "SELECT {key}, operation_id FROM {table}
                         WHERE repo_id = ?1 AND operation_id IS NOT NULL
                           AND operation_collected = 0"
                    ))?;
                    statement
                        .query_map([repo], |row| Ok((row.get(0)?, row.get(1)?)))?
                        .collect::<Result<_, _>>()?
                };
                for (id, old) in rows {
                    let (new, gone) = match rewritten.get(&old) {
                        Some(new) => (new.clone(), false),
                        None => (old, true),
                    };
                    tx.execute(
                        &format!(
                            "UPDATE {table} SET operation_id = ?3,
                               operation_collected = MAX(operation_collected, ?4)
                             WHERE repo_id = ?1 AND {key} = ?2"
                        ),
                        params![repo, id, new, i64::from(gone)],
                    )?;
                    if gone {
                        collected += 1;
                    } else {
                        mapped += 1;
                    }
                }
            }
            Ok((mapped, collected))
        })
    }
}
