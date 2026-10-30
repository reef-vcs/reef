//! GC's sixth step per repository (R2.16): changes no longer open whose
//! activity, landing and end are all older than `[history] retention`; then,
//! while the file's live pages pass `[history] max_size`, the oldest of the
//! rest, never one with activity in the last day; then unused strings.

use std::time::SystemTime;

use super::follow::{ms, ms_of};
use super::store::{Outcome, Store};
use super::{History, HistoryError};
use crate::model::RepoId;

/// Changes deleted per transaction while shrinking.
const BATCH: i64 = 1000;
/// Activity this recent keeps a change whatever the size.
const KEEP_RECENT_MS: i64 = 86_400_000;

/// Deletes `ids`' rows and everything that hangs off them.
fn delete(
    tx: &rusqlite::Transaction<'_>,
    where_clause: &str,
    params: &[i64],
) -> rusqlite::Result<usize> {
    let ids = format!("SELECT id FROM changes {where_clause}");
    let p = rusqlite::params_from_iter(params.iter());
    tx.execute(&format!("DELETE FROM activity WHERE change IN ({ids})"), p)?;
    let p = rusqlite::params_from_iter(params.iter());
    tx.execute(
        &format!("DELETE FROM change_paths WHERE change IN ({ids})"),
        p,
    )?;
    let p = rusqlite::params_from_iter(params.iter());
    tx.execute(&format!("DELETE FROM touches WHERE change IN ({ids})"), p)?;
    let p = rusqlite::params_from_iter(params.iter());
    tx.execute(&format!("DELETE FROM changes WHERE id IN ({ids})"), p)
}

/// Runs the step on `store`, with `now` as the time.
pub(crate) fn run_on(
    store: &Store,
    retention_ms: i64,
    max_size: u64,
    now: i64,
) -> Result<usize, HistoryError> {
    let cutoff = now - retention_ms;
    let (open, landed) = (Outcome::Open as i64, Outcome::Landed as i64);
    let mut deleted = store.write(|tx| {
        delete(
            tx,
            "WHERE outcome != ?1 AND last_at < ?2 AND coalesce(landed_at, 0) < ?2
                   AND coalesce(ended_at, 0) < ?2",
            &[open, cutoff],
        )
    })?;
    let recent = now - KEEP_RECENT_MS;
    while store.live_bytes()? > max_size {
        // The oldest that are not open first, then open ones whose activity
        // is older than the retention.
        let removed = store.write(|tx| {
            let n = delete(
                tx,
                "WHERE id IN (SELECT id FROM changes WHERE outcome != ?1 AND last_at < ?2
                              ORDER BY last_at LIMIT ?3)",
                &[open, recent, BATCH],
            )?;
            if n > 0 {
                return Ok(n);
            }
            delete(
                tx,
                "WHERE id IN (SELECT id FROM changes WHERE outcome IN (?1, ?2) AND last_at < ?3
                              ORDER BY last_at LIMIT ?4)",
                &[open, landed, cutoff.min(recent), BATCH],
            )
        })?;
        if removed == 0 {
            break;
        }
        deleted += removed;
        store.vacuum_step(10_000)?;
    }
    store.write(|tx| {
        tx.execute_batch(
            "DELETE FROM names WHERE id NOT IN (SELECT name FROM touches);
             DELETE FROM symbols WHERE id NOT IN (SELECT symbol FROM touches)
               AND id NOT IN (SELECT old_symbol FROM touches WHERE old_symbol IS NOT NULL);
             DELETE FROM paths WHERE id NOT IN (SELECT path FROM change_paths)
               AND id NOT IN (SELECT path FROM touches);
             DELETE FROM agents WHERE id NOT IN (SELECT agent FROM activity);",
        )
    })?;
    store.vacuum_step(10_000)?;
    Ok(deleted)
}

impl History {
    /// GC's step for `repo`; nothing in a dry run or with the index off.
    pub(crate) fn gc(&self, repo: RepoId, dry_run: bool) -> Result<usize, HistoryError> {
        if dry_run || !self.config.enabled {
            return Ok(0);
        }
        let Some(held) = self.held(repo, false)? else {
            return Ok(0);
        };
        let now = self.now_ms().unwrap_or_else(|| ms(SystemTime::now()));
        let deleted = run_on(
            &held.store,
            ms_of(self.config.retention),
            self.config.max_size.0,
            now,
        )?;
        if deleted > 0 {
            tracing::info!(repo = %repo, deleted_count = deleted, "history index trimmed");
        }
        Ok(deleted)
    }
}
