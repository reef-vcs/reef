//! The worker that computes each change's touches: the paths and symbols
//! its latest commit's net diff changes against its parents, with R2.15's
//! matching (`symbols::diff::compute`). One change at a time per daemon, on
//! the read pool. Its queue is the table: changes whose touches are not for
//! their latest commit, newest activity first, each taken once it has been
//! quiet for [`Throttle::quiet`] or its touches are [`Throttle::stale`]
//! behind, whichever comes first, so a workspace snapshotting every few
//! seconds is computed about once a minute and once more when it settles.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime};

use reef_lib::jj::CommitIdHex;
use reef_lib::jj::diff::{self, ChangeKind, DiffBase, DiffFormat, DiffOptions, RENAME_LIMIT};
use reef_lib::jj::files;
use rusqlite::OptionalExtension as _;

use super::follow::{ms, ms_of, on_pool};
use super::store::{self, NewPath, NewTouch, Store, SymbolsCoverage, Touches};
use super::{History, HistoryError};
use crate::daemon::AppState;
use crate::model::RepoId;
use crate::symbols::diff::{self as symbol_diff, Coverage, SymbolRequest};

/// Paths stored per change at most.
pub(crate) const MAX_PATHS: usize = 10_000;
/// Symbol touches stored per change at most.
pub(crate) const MAX_TOUCHES: usize = 10_000;

/// When a change's touches are computed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Throttle {
    /// Since its last commit.
    pub quiet: Duration,
    /// Since its touches were computed, or it was first written.
    pub stale: Duration,
}

impl Default for Throttle {
    fn default() -> Self {
        Throttle {
            quiet: Duration::from_secs(5),
            stale: Duration::from_secs(60),
        }
    }
}

/// A change waiting for its touches.
#[derive(Debug, Clone)]
pub(crate) struct Pending {
    pub id: i64,
    pub change_id: String,
    pub commit_id: String,
}

/// The next change of `store` due now, or when the next one falls due.
pub(crate) fn next_due(
    store: &Store,
    throttle: Throttle,
    now: i64,
) -> Result<Result<Pending, Option<i64>>, HistoryError> {
    let (quiet, stale) = (ms_of(throttle.quiet), ms_of(throttle.stale));
    store
        .read(|conn| {
            // Due ones in SQL, newest first, so busy changes on top never
            // hide quiet ones behind them.
            let due = conn
                .prepare_cached(
                    "SELECT id, change_id, commit_id FROM changes
                     WHERE touched_commit IS NOT commit_id
                       AND (commit_at <= ?1 OR coalesce(touched_at, first_at) <= ?2)
                     ORDER BY last_at DESC LIMIT 1",
                )?
                .query_row([now - quiet, now - stale], |row| {
                    Ok(Pending {
                        id: row.get(0)?,
                        change_id: row.get(1)?,
                        commit_id: row.get(2)?,
                    })
                })
                .optional()?;
            if let Some(pending) = due {
                return Ok(Ok(pending));
            }
            let soonest: Option<i64> = conn.query_row(
                "SELECT min(min(commit_at + ?1, coalesce(touched_at, first_at) + ?2))
                 FROM changes WHERE touched_commit IS NOT commit_id",
                [quiet, stale],
                |row| row.get(0),
            )?;
            Ok(Err(soonest))
        })
        .map_err(HistoryError::from)
}

/// The `SymbolChangeKind` value of a change's kind.
fn change_value(kind: reef_symbols::diff::ChangeKind) -> i64 {
    use reef_symbols::diff::ChangeKind;
    match kind {
        ChangeKind::Added => 1,
        ChangeKind::Removed => 2,
        ChangeKind::Changed => 3,
        ChangeKind::Moved => 4,
    }
}

impl History {
    /// Computes `pending`'s touches in `repo` and stores them.
    #[expect(clippy::too_many_lines, reason = "one read job: paths, then symbols")]
    pub(crate) async fn touch(
        &self,
        state: &Arc<AppState>,
        repo: RepoId,
        store: &Arc<Store>,
        pending: Pending,
    ) -> Result<(), HistoryError> {
        self.computed.fetch_add(1, Ordering::Relaxed);
        let handle = state.repos.get(repo).ok_or(HistoryError::NotOpen(repo))?;
        let head = state.reads.loaded(&handle).await?;
        let symbols = Arc::clone(&state.symbols);
        let symbol_store = if symbols.enabled() {
            symbols.store(repo).ok()
        } else {
            None
        };
        let commit: CommitIdHex = pending
            .commit_id
            .parse()
            .map_err(|_| HistoryError::Read(format!("bad commit id {}", pending.commit_id)))?;
        let computed = state
            .reads
            .run(&handle, move || async move {
                let options = DiffOptions {
                    format: DiffFormat::Summary,
                    context: 0,
                    max_lines: usize::MAX,
                    max_hunk_bytes: usize::MAX,
                    max_file_bytes: usize::MAX,
                    paths: Vec::new(),
                    after: None,
                    limit: MAX_PATHS,
                    pairs: symbol_store.is_some(),
                };
                let mut page =
                    diff::diff_page(&head, &DiffBase::Parents, &commit, &options, &|| false)
                        .await
                        .map_err(|error| HistoryError::Read(error.to_string()))?;
                let mut touches = Touches {
                    paths: page
                        .files
                        .iter()
                        .map(|file| NewPath {
                            path: file.path.clone(),
                            kind: match file.kind {
                                None => 0,
                                Some(ChangeKind::Added) => 1,
                                Some(ChangeKind::Modified) => 2,
                                Some(ChangeKind::Deleted) => 3,
                                Some(ChangeKind::Renamed) => 4,
                                Some(ChangeKind::Copied) => 5,
                            },
                        })
                        .collect(),
                    paths_changed: u64::try_from(page.total).unwrap_or(u64::MAX),
                    paths_truncated: page.total > MAX_PATHS,
                    ..Touches::default()
                };
                let Some(symbol_store) = symbol_store else {
                    touches.coverage = Some(SymbolsCoverage::Disabled);
                    return Ok(touches);
                };
                let pairs = page.pairs.take();
                if page.total > RENAME_LIMIT || pairs.is_none() {
                    touches.coverage = Some(if page.total == 0 {
                        SymbolsCoverage::Covered
                    } else {
                        SymbolsCoverage::TooLarge
                    });
                    return Ok(touches);
                }
                let tree = files::commit_tree(&head, &page.to)
                    .await
                    .map_err(|error| HistoryError::Read(error.to_string()))?;
                let all: Vec<String> = page.files.iter().map(|f| f.path.clone()).collect();
                let found = symbol_diff::compute(
                    &symbols,
                    &symbol_store,
                    &tree,
                    pairs.as_deref(),
                    &all,
                    &SymbolRequest::default(),
                    &options,
                )
                .map_err(|error| HistoryError::Read(error.to_string()))?;
                let mut partial = false;
                let mut files: Vec<_> = found.files.into_iter().collect();
                files.sort_by(|a, b| a.0.cmp(&b.0));
                for (path, file) in files {
                    partial |= matches!(file.coverage, Coverage::Skipped | Coverage::NoScip);
                    partial |= file.truncated;
                    for change in file.changes {
                        touches.symbols_changed += 1;
                        if touches.symbols.len() == MAX_TOUCHES {
                            continue;
                        }
                        let home = match change.kind {
                            reef_symbols::diff::ChangeKind::Removed => {
                                change.old_path.clone().unwrap_or_else(|| path.clone())
                            }
                            _ => path.clone(),
                        };
                        touches.symbols.push(NewTouch {
                            symbol: change.symbol,
                            old_symbol: change.old_symbol,
                            name: change.name,
                            kind: i64::from(change.symbol_kind as u8),
                            change_kind: change_value(change.kind),
                            path: home,
                        });
                    }
                }
                touches.coverage = Some(if partial {
                    SymbolsCoverage::Partial
                } else {
                    SymbolsCoverage::Covered
                });
                Ok::<_, HistoryError>(touches)
            })
            .await?;
        let now = ms(SystemTime::now());
        if let Err(error) = &computed {
            // A commit that cannot be read (collected, or a broken tree) is
            // not tried again until the change moves.
            tracing::warn!(repo = %repo, change = %pending.change_id, error = %error, "computing a change's touches failed");
        }
        let store = Arc::clone(store);
        on_pool(move || {
            store.write(|tx| match &computed {
                // Only for the commit it was computed for; a later commit
                // keeps the change pending.
                Ok(touches) => store::set_touches(tx, pending.id, &pending.commit_id, now, touches),
                Err(_) => tx
                    .execute(
                        "UPDATE changes SET touched_commit = ?2, touched_at = ?3 WHERE id = ?1",
                        rusqlite::params![pending.id, pending.commit_id, now],
                    )
                    .map(drop),
            })?;
            Ok(())
        })
        .await
    }
}
