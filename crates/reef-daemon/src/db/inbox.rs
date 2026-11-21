//! The triage inbox's record (R3.33, ADR 0088): each person's mark per
//! repository, and the landings since a base.
//!
//! A mark is the newest `lifecycle_transitions.seq` a person has seen. It
//! only moves forward, in one transaction, so two tabs marking at once
//! converge and a repeat changes nothing.

use std::collections::HashMap;

use rusqlite::{OptionalExtension as _, params};

use super::{Db, DbError};
use crate::model::{RepoId, Timestamp};

/// One person's mark on one repository.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct InboxMarkRow {
    /// The newest transition seen.
    pub seq: i64,
    /// When the mark last moved.
    pub seen_at: Timestamp,
    /// The web UI session that last moved it; `None` for the socket or the
    /// remote listener.
    pub web_session: Option<String>,
}

/// Where a mark stood before a move, and where it stands after.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MarkMove {
    /// The seq before; `None` for a person's first mark on the repository.
    pub from: Option<i64>,
    /// The seq after.
    pub to: i64,
}

impl MarkMove {
    /// Whether the mark moved (a first mark always does).
    pub fn moved(&self) -> bool {
        self.from.is_none_or(|from| from < self.to)
    }
}

/// Where a digest starts: after a transition seq, which is a person's mark,
/// a page's kept base, or with no mark the newest transition a day ago.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DigestBase {
    /// Landings recorded after this transition are counted.
    pub seq: i64,
    /// Whether a mark set it.
    pub from_mark: bool,
}

/// One change landed after a base and landed still.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LandedRow {
    /// The change, in hex.
    pub change_id: String,
    /// The landing transition's seq.
    pub seq: i64,
    /// When it was recorded.
    pub landed_at: Timestamp,
    /// The agent that proposed it.
    pub agent: Option<String>,
}

/// The landings after a base.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Landed {
    /// How many.
    pub count: u64,
    /// How many per proposing agent, most first.
    pub by_agent: Vec<(Option<String>, u64)>,
    /// The newest, newest first.
    pub newest: Vec<LandedRow>,
    /// The newest seq counted; `None` for none.
    pub through_seq: Option<i64>,
}

/// Agents a digest counts by name.
const BY_AGENT: usize = 10;

impl Db {
    /// `identity`'s marks on `repos`.
    pub fn inbox_marks(
        &self,
        identity: &str,
        repos: &[RepoId],
    ) -> Result<HashMap<RepoId, InboxMarkRow>, DbError> {
        self.with(|conn| {
            let mut statement = conn.prepare_cached(
                "SELECT seq, seen_at, web_session FROM inbox_marks
                 WHERE identity_id = ?1 AND repo_id = ?2",
            )?;
            let mut found = HashMap::new();
            for repo in repos {
                let row = statement
                    .query_row(params![identity, repo], |row| {
                        Ok(InboxMarkRow {
                            seq: row.get(0)?,
                            seen_at: Timestamp(row.get(1)?),
                            web_session: row.get(2)?,
                        })
                    })
                    .optional()?;
                if let Some(row) = row {
                    found.insert(*repo, row);
                }
            }
            Ok(found)
        })
    }

    /// Moves `identity`'s mark on `repo` to `seq`, never backward and never
    /// past the repository's newest committed transition, and returns where
    /// it stood and stands. `seen_at` and `web_session`, the web UI session
    /// the call came through, move only with the mark.
    pub fn advance_inbox_mark(
        &self,
        identity: &str,
        repo: RepoId,
        seq: i64,
        at: Timestamp,
        web_session: Option<&str>,
    ) -> Result<MarkMove, DbError> {
        self.in_transaction(|tx| {
            let from = tx
                .query_row(
                    "SELECT seq FROM inbox_marks WHERE identity_id = ?1 AND repo_id = ?2",
                    params![identity, repo],
                    |row| row.get(0),
                )
                .optional()?;
            let to = tx.query_row(
                "INSERT INTO inbox_marks(identity_id, repo_id, seq, seen_at, web_session)
                 SELECT ?1, ?2, max(0, min(?3, coalesce(
                     (SELECT max(seq) FROM lifecycle_transitions
                      WHERE repo_id = ?2 AND status = 'committed'), 0))), ?4, ?5
                 ON CONFLICT(identity_id, repo_id) DO UPDATE SET
                     seen_at = CASE WHEN excluded.seq > inbox_marks.seq
                                    THEN excluded.seen_at ELSE inbox_marks.seen_at END,
                     web_session = CASE WHEN excluded.seq > inbox_marks.seq
                                        THEN excluded.web_session
                                        ELSE inbox_marks.web_session END,
                     seq = max(inbox_marks.seq, excluded.seq)
                 RETURNING seq",
                params![identity, repo, seq, at.0, web_session],
                |row| row.get(0),
            )?;
            Ok(MarkMove { from, to })
        })
    }

    /// The changes of `repo` landed after `base` whose lifecycle is still
    /// that landing (an undone or reverted landing is not counted): their
    /// count, counts by proposing agent, the newest `limit`, and the newest
    /// seq counted.
    pub fn landed_since(
        &self,
        repo: RepoId,
        base: DigestBase,
        limit: usize,
    ) -> Result<Landed, DbError> {
        let seq = base.seq;
        let landed = "FROM lifecycle_transitions t
             JOIN lifecycle l ON l.repo_id = t.repo_id AND l.change_id = t.change_id
                 AND l.transition_id = t.id
             WHERE t.repo_id = ?1 AND t.status = 'committed' AND t.to_state = 'landed'
                 AND t.seq > ?2";
        self.with(|conn| {
            let (count, through): (i64, Option<i64>) = conn.query_row(
                &format!("SELECT count(*), max(t.seq) {landed}"),
                params![repo, seq],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )?;
            let mut statement = conn.prepare(&format!(
                "SELECT l.proposed_by_agent, count(*) {landed}
                 GROUP BY l.proposed_by_agent ORDER BY count(*) DESC, l.proposed_by_agent
                 LIMIT ?3"
            ))?;
            let by_agent = statement
                .query_map(
                    params![repo, seq, i64::try_from(BY_AGENT).unwrap_or(i64::MAX)],
                    |row| Ok((row.get(0)?, row.get::<_, i64>(1)?.unsigned_abs())),
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let mut statement = conn.prepare(&format!(
                "SELECT t.change_id, t.seq, t.recorded_at, l.proposed_by_agent {landed}
                 ORDER BY t.seq DESC LIMIT ?3"
            ))?;
            let newest = statement
                .query_map(
                    params![repo, seq, i64::try_from(limit).unwrap_or(i64::MAX)],
                    |row| {
                        Ok(LandedRow {
                            change_id: row.get(0)?,
                            seq: row.get(1)?,
                            landed_at: Timestamp(row.get(2)?),
                            agent: row.get(3)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            Ok(Landed {
                count: count.unsigned_abs(),
                by_agent,
                newest,
                through_seq: through,
            })
        })
    }

    /// The newest committed transition of `repo` recorded at or before
    /// `at`, or 0 for none: where a digest with no mark starts, as a seq.
    pub fn transition_seq_at(&self, repo: RepoId, at: Timestamp) -> Result<i64, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT coalesce(max(seq), 0) FROM lifecycle_transitions
                 WHERE repo_id = ?1 AND status = 'committed' AND recorded_at <= ?2",
                params![repo, at.0],
                |row| row.get(0),
            )
        })
    }

    /// When `repo`'s transition `seq` was recorded, if it exists.
    pub fn transition_time(&self, repo: RepoId, seq: i64) -> Result<Option<Timestamp>, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT recorded_at FROM lifecycle_transitions
                 WHERE repo_id = ?1 AND seq = ?2 AND status = 'committed'",
                params![repo, seq],
                |row| row.get::<_, Option<i64>>(0),
            )
            .optional()
            .map(|at| at.flatten().map(Timestamp))
        })
    }

    /// The newest committed transition of `repo`, if any.
    pub fn newest_transition_seq(&self, repo: RepoId) -> Result<Option<i64>, DbError> {
        self.with(|conn| {
            conn.query_row(
                "SELECT max(seq) FROM lifecycle_transitions
                 WHERE repo_id = ?1 AND status = 'committed'",
                params![repo],
                |row| row.get(0),
            )
        })
    }
}
