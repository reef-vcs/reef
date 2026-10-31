//! The follower: reads each open repository's operation log after its
//! frontier, as subscriptions do (`subscribe::feed`), and writes what each
//! operation did to its changes; and the backfill, which reads older
//! operations newest first when an index is new, recreated, or fell too far
//! behind.
//!
//! An operation writes a change directly when the change is what it set out
//! to write: a created change; a rewrite whose new commit is a working-copy
//! commit the operation moved (a snapshot, a move); a rewrite or abandon of
//! the commit Reef's describe, edit or abandon names in its description; and
//! every effect of another jj client's operation, whose intent Reef cannot
//! tell. Direct writes add activity. Other rewrites are rebases: they move
//! the change's commit and nothing else. Outcomes are read from the view at
//! the repository's tip, never accumulated, so the order operations are
//! applied in does not matter.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};
use std::ops::ControlFlow;
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use reef_api::v1::{self, ChangeEffectKind, OperationKind};
use reef_lib::jj::effects;
use reef_lib::jj::history::{self as jj_history, ChangeState};
use reef_lib::jj::oplog::{self, Frontier};
use reef_lib::jj::revset;
use reef_lib::jj::{ChangeIdHex, CommitIdHex, OperationIdHex, ReadonlyRepo};
use rusqlite::Transaction;

use super::store::{
    self, ChangeUpdate, META_BACKFILL, META_CATCHUP, META_FRONTIER, META_TRUNK, Outcome, Store,
};
use super::{History, HistoryError, RepoHistory};
use crate::daemon::AppState;
use crate::model::RepoId;
use crate::service::op_filter::OpMatcher;
use crate::service::reading::Records;
use crate::subscribe::feed::{self, Budget, ReadFailure};

/// Operations one live read examines at most.
const READ_OPERATIONS: usize = 200;
/// Operations one backfill chunk reads at most.
const BACKFILL_CHUNK: usize = 500;
/// Changes a move of `trunk()` lands or unlands at most; past it, the rest
/// are found when they are next written.
const MAX_TRUNK_CHANGES: usize = 10_000;
/// Operations a live read may be behind before the follower catches up by
/// walking back from the heads instead.
const MAX_BEHIND: usize = 10_000;

/// Milliseconds since the epoch of `at`.
pub(crate) fn ms(at: SystemTime) -> i64 {
    at.duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

fn ts_ms(at: Option<&prost_types::Timestamp>) -> i64 {
    at.map_or(0, |t| {
        t.seconds.saturating_mul(1000) + i64::from(t.nanos / 1_000_000)
    })
}

/// Where an unfinished backfill stands: `meta.backfill`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Backfill {
    /// Operations read so far.
    pub examined: u64,
    /// Where the walk continues: operations not read whose children all
    /// were.
    pub next: Vec<OperationIdHex>,
    /// The walk stops at the first of these (a catch-up's old frontier).
    pub stop: Vec<OperationIdHex>,
    /// The walk stops at an operation that ended at or before this, in
    /// milliseconds since the epoch; 0 for none. A catch-up whose old
    /// frontier GC rewrote stops by its time.
    pub stop_at: i64,
}

impl Backfill {
    fn encode(&self) -> String {
        let join = |ids: &[OperationIdHex]| {
            ids.iter()
                .map(ToString::to_string)
                .collect::<Vec<_>>()
                .join(",")
        };
        format!(
            "{}|{}|{}|{}",
            self.examined,
            join(&self.next),
            join(&self.stop),
            self.stop_at
        )
    }

    fn decode(text: &str) -> Option<Backfill> {
        let mut parts = text.split('|');
        let examined = parts.next()?.parse().ok()?;
        let ids = |part: Option<&str>| -> Option<Vec<OperationIdHex>> {
            part?
                .split(',')
                .filter(|id| !id.is_empty())
                .map(|id| id.parse().ok())
                .collect()
        };
        Some(Backfill {
            examined,
            next: ids(parts.next())?,
            stop: ids(parts.next())?,
            stop_at: parts.next().map_or(Some(0), |at| at.parse().ok())?,
        })
    }
}

fn encode_frontier(frontier: &Frontier) -> String {
    frontier
        .ids()
        .iter()
        .map(ToString::to_string)
        .collect::<Vec<_>>()
        .join(",")
}

fn decode_frontier(text: &str) -> Option<Frontier> {
    let ids: Option<Vec<OperationIdHex>> = text
        .split(',')
        .filter(|id| !id.is_empty())
        .map(|id| id.parse().ok())
        .collect();
    ids.map(Frontier::new).filter(|f| !f.ids().is_empty())
}

/// What the follower reads of the repository's state for one batch.
#[derive(Debug, Default)]
struct TipRead {
    states: HashMap<String, ChangeState>,
    trunk: Option<String>,
    landed: Vec<ChangeIdHex>,
    unlanded: Vec<ChangeIdHex>,
}

/// The latest commit each change in `operations` was given, in order.
fn latest_commits(operations: &[v1::Operation]) -> BTreeMap<String, String> {
    let mut latest: BTreeMap<String, (i64, String)> = BTreeMap::new();
    for op in operations {
        let at = ts_ms(op.ended_at.as_ref());
        for effect in op.effects.iter().flat_map(|e| &e.changes) {
            let commit = if effect.commit_id.is_empty() {
                &effect.previous_commit_id
            } else {
                &effect.commit_id
            };
            let entry = latest
                .entry(effect.change_id.clone())
                .or_insert((at, commit.clone()));
            if at >= entry.0 && !effect.commit_id.is_empty() {
                *entry = (at, commit.clone());
            }
        }
    }
    latest.into_iter().map(|(k, (_, c))| (k, c)).collect()
}

/// Effects past [`effects::MAX_EFFECTS`] the history reads, so a change an
/// operation wrote is not lost behind its rebased descendants.
const MAX_HISTORY_EFFECTS: usize = 10_000;

/// Reads, at `tip`, the state of every change `operations` touch, and what
/// a move of `trunk()` from `old_trunk` landed and unlanded. An operation
/// whose effects the API capped is read again with a larger cap.
async fn read_tip(
    tip: &Arc<ReadonlyRepo>,
    mut operations: Vec<v1::Operation>,
    old_trunk: Option<&str>,
    live: bool,
) -> Result<(Vec<v1::Operation>, TipRead), oplog::OpLogError> {
    for op in &mut operations {
        let capped = op.effects.as_ref().is_some_and(|e| {
            usize::try_from(e.change_count).unwrap_or(usize::MAX) > e.changes.len()
        });
        if !capped {
            continue;
        }
        let Ok(operation) = oplog::resolve_operation(tip, &op.id).await else {
            continue;
        };
        let wider = effects::operation_effects(tip, &operation, MAX_HISTORY_EFFECTS).await?;
        op.effects = Some(crate::service::convert::effects(&wider));
    }
    let read = read_states(tip, &operations, old_trunk, live).await?;
    Ok((operations, read))
}

async fn read_states(
    tip: &ReadonlyRepo,
    operations: &[v1::Operation],
    old_trunk: Option<&str>,
    live: bool,
) -> Result<TipRead, oplog::OpLogError> {
    let latest = latest_commits(operations);
    let pairs: Vec<(ChangeIdHex, CommitIdHex)> = latest
        .iter()
        .filter_map(|(change, commit)| Some((change.parse().ok()?, commit.parse().ok()?)))
        .collect();
    let trunk = revset::trunk(tip).map(|t| t.commit_id);
    let states = jj_history::change_states(tip, &pairs, trunk.as_ref()).await?;
    let mut read = TipRead {
        states: pairs
            .iter()
            .map(|(change, _)| change.to_string())
            .zip(states)
            .collect(),
        trunk: trunk.as_ref().map(ToString::to_string),
        ..TipRead::default()
    };
    let old: Option<CommitIdHex> = old_trunk.and_then(|t| t.parse().ok());
    if live
        && let (Some(old), Some(new)) = (&old, &trunk)
        && old != new
    {
        // An old trunk commit GC pruned lands nothing; later writes catch up.
        if let Ok((landed, _)) =
            jj_history::change_ids_between(tip, Some(old), new, MAX_TRUNK_CHANGES).await
        {
            read.landed = landed;
        }
        if let Ok((unlanded, _)) =
            jj_history::change_ids_between(tip, Some(new), old, MAX_TRUNK_CHANGES).await
        {
            read.unlanded = unlanded;
        }
    }
    Ok(read)
}

/// The commit a Reef describe, edit or abandon names in its description.
fn named_commit(description: &str) -> Option<&str> {
    [
        oplog::DESCRIBE_PREFIX,
        oplog::EDIT_PREFIX,
        oplog::ABANDON_PREFIX,
    ]
    .iter()
    .find_map(|prefix| description.strip_prefix(prefix))
    .and_then(|rest| rest.split(' ').next())
}

/// Writes `operations`' effects in one transaction, with `read` from the
/// tip. `live`: read after the frontier, so a change landing now has a
/// landing time; a backfill's changes found landed have none.
#[expect(
    clippy::too_many_lines,
    reason = "the direct-write rule, then outcomes, then trunk"
)]
fn apply(
    tx: &Transaction<'_>,
    operations: &[v1::Operation],
    read: &TipRead,
    live: bool,
    retention_cutoff: i64,
) -> rusqlite::Result<()> {
    let mut touched: BTreeMap<String, (i64, Option<store::ChangeRow>)> = BTreeMap::new();
    let mut batch_end = 0;
    for op in operations {
        let at = ts_ms(op.ended_at.as_ref());
        batch_end = batch_end.max(at);
        let kind = op.kind();
        let external = matches!(kind, OperationKind::External | OperationKind::Other);
        let import = kind == OperationKind::ImportRefs;
        let Some(effects) = &op.effects else {
            continue;
        };
        let wcs: HashSet<&str> = effects
            .working_copies
            .iter()
            .map(|w| w.commit_id.as_str())
            .collect();
        let named = named_commit(&op.description);
        for effect in &effects.changes {
            let effect_kind = effect.kind();
            let names_it = named == Some(effect.previous_commit_id.as_str());
            let direct = match effect_kind {
                ChangeEffectKind::Created => true,
                ChangeEffectKind::Rewritten => {
                    external || wcs.contains(effect.commit_id.as_str()) || names_it
                }
                ChangeEffectKind::Abandoned => external || names_it,
                ChangeEffectKind::Hidden | ChangeEffectKind::Unspecified => false,
            };
            if effect_kind == ChangeEffectKind::Abandoned {
                // By change id, whether or not a row exists yet: a backfill
                // reads this before the operation that created the change.
                store::record_end(tx, &effect.change_id, kind == OperationKind::Undo, at)?;
            }
            let mut written_at = at;
            if import && effect_kind == ChangeEffectKind::Created {
                // A fetch of old commits is dated by them, and those older
                // than the retention are left out.
                let committed = read
                    .states
                    .get(&effect.change_id)
                    .map_or(0, |s| s.committed_at);
                if committed < retention_cutoff {
                    continue;
                }
                written_at = committed;
            }
            let row = match store::change_row(tx, &effect.change_id)? {
                Some(row) => row,
                None if direct => {
                    let commit = if effect.commit_id.is_empty() {
                        &effect.previous_commit_id
                    } else {
                        &effect.commit_id
                    };
                    let id = store::insert_change(tx, &effect.change_id, commit, written_at)?;
                    touched
                        .entry(effect.change_id.clone())
                        .or_insert((id, None));
                    store::change_row(tx, &effect.change_id)?.unwrap_or(store::ChangeRow {
                        id,
                        outcome: Outcome::Open,
                        landed_at: None,
                        ended_at: None,
                    })
                }
                // A rebase of a change the index has never seen written.
                None => continue,
            };
            touched
                .entry(effect.change_id.clone())
                .or_insert_with(|| (row.id, Some(row.clone())));
            if !effect.commit_id.is_empty() {
                store::set_commit(tx, row.id, &effect.commit_id, at)?;
            }
            if direct {
                store::add_activity(tx, row.id, &op.agent, &op.workspace_id, written_at, &op.id)?;
            }
        }
    }
    for (change_id, (id, before)) in &touched {
        let Some(state) = read.states.get(change_id) else {
            continue;
        };
        let update = if state.visible.is_empty() {
            let (outcome, ended_at) = match (store::end_of(tx, change_id)?, before) {
                (Some((undo, at)), _) => (
                    if undo {
                        Outcome::Undone
                    } else {
                        Outcome::Abandoned
                    },
                    Some(at),
                ),
                (None, Some(before)) if !before.outcome.visible() => {
                    (before.outcome, before.ended_at.or(Some(batch_end)))
                }
                _ => (Outcome::Abandoned, Some(batch_end)),
            };
            ChangeUpdate {
                outcome,
                divergent: false,
                subject: state.subject.clone(),
                author_email: state.author_email.clone(),
                landed_at: None,
                ended_at,
            }
        } else {
            // The commit the repository shows, whatever order the effects
            // that led to it were read in.
            if let Some(commit) = state.visible.first() {
                store::settle_commit(tx, *id, commit.as_str(), batch_end)?;
            }
            let landed_at = match before {
                Some(before) if state.landed && before.outcome == Outcome::Landed => {
                    before.landed_at
                }
                Some(before) if state.landed && live && before.outcome == Outcome::Open => {
                    Some(batch_end)
                }
                _ => None,
            };
            ChangeUpdate {
                outcome: if state.landed {
                    Outcome::Landed
                } else {
                    Outcome::Open
                },
                divergent: state.visible.len() > 1,
                subject: state.subject.clone(),
                author_email: state.author_email.clone(),
                landed_at,
                ended_at: None,
            }
        };
        store::update_change(tx, *id, &update)?;
    }
    for change in &read.landed {
        tx.prepare_cached(
            "UPDATE changes SET outcome = ?2, landed_at = ?3 WHERE change_id = ?1 AND outcome = ?4",
        )?
        .execute(rusqlite::params![
            change.as_str(),
            Outcome::Landed as i64,
            batch_end,
            Outcome::Open as i64
        ])?;
    }
    // A rewrite of a commit on trunk puts its change in both lists: it
    // stays landed.
    let landed: HashSet<&str> = read.landed.iter().map(ChangeIdHex::as_str).collect();
    for change in read
        .unlanded
        .iter()
        .filter(|c| !landed.contains(c.as_str()))
    {
        tx.prepare_cached(
            "UPDATE changes SET outcome = ?2, landed_at = NULL WHERE change_id = ?1 AND outcome = ?3",
        )?
        .execute(rusqlite::params![
            change.as_str(),
            Outcome::Open as i64,
            Outcome::Landed as i64
        ])?;
    }
    if let Some(trunk) = &read.trunk
        && live
    {
        store::set_meta(tx, META_TRUNK, Some(trunk))?;
    }
    Ok(())
}

/// A matcher that keeps every operation.
fn keep_all() -> Arc<OpMatcher> {
    Arc::new(OpMatcher::default())
}

impl History {
    /// Reads and writes what `repo`'s log holds after the frontier, and a
    /// chunk of catch-up or backfill; returns whether more is left to read
    /// now.
    #[expect(clippy::too_many_lines, reason = "start, read, catch up, backfill")]
    pub(crate) async fn step(
        &self,
        state: &Arc<AppState>,
        repo: RepoId,
        held: &RepoHistory,
    ) -> Result<bool, HistoryError> {
        let _lock = held.lock.lock().await;
        let store = &held.store;
        let Some(tip) = state.events.tip(repo) else {
            return Ok(false);
        };
        let handle = state.repos.get(repo).ok_or(HistoryError::NotOpen(repo))?;
        let frontier = store.read(|conn| store::meta(conn, META_FRONTIER))?;
        let Some(frontier) = frontier.as_deref().and_then(decode_frontier) else {
            // New: start at the heads, and backfill what came before.
            let heads = {
                let tip = Arc::clone(&tip);
                state
                    .reads
                    .run(&handle, move || async move { oplog::op_heads(&tip).await })
                    .await??
            };
            let trunk = revset::trunk(&tip).map(|t| t.commit_id.to_string());
            let backfill = (self.config.backfill_operations > 0).then(|| Backfill {
                examined: 0,
                next: heads.clone(),
                stop: Vec::new(),
                stop_at: 0,
            });
            store.write(|tx| {
                store::set_meta(
                    tx,
                    META_FRONTIER,
                    Some(&encode_frontier(&Frontier::new(heads))),
                )?;
                store::set_meta(tx, META_TRUNK, trunk.as_deref())?;
                store::set_meta(
                    tx,
                    META_BACKFILL,
                    backfill.as_ref().map(Backfill::encode).as_deref(),
                )
            })?;
            return Ok(true);
        };
        let budget = Budget {
            operations: READ_OPERATIONS,
            bytes: usize::MAX,
            first_free: true,
            effects: READ_OPERATIONS,
            behind: MAX_BEHIND,
        };
        // Under the read pool's permit, never a subscriber's.
        let permit = state.reads.permit().await?;
        let read = feed::read_holding(
            state,
            repo,
            Arc::clone(&tip),
            &frontier,
            None,
            &keep_all(),
            budget,
            permit,
        )
        .await;
        let more;
        match read {
            Ok(batch) => {
                more = batch.more;
                if !batch.operations.is_empty() || batch.frontier != frontier {
                    let old_trunk = store.read(|conn| store::meta(conn, META_TRUNK))?;
                    // At the batch's last operation, loaded on the read
                    // pool: the tip the log was read through can trail the
                    // operations read, and reloading on the repository's
                    // thread would hold up the writes it serves.
                    let last = batch.operations.last().map(|op| op.id.clone());
                    let operations = batch.operations;
                    let tip = Arc::clone(&tip);
                    let (operations, tip_read) = state
                        .reads
                        .run(&handle, move || async move {
                            let at = at_operation(&tip, last.as_deref()).await?;
                            read_tip(&at, operations, old_trunk.as_deref(), true).await
                        })
                        .await??;
                    let cutoff = ms(SystemTime::now()) - ms_of(self.config.retention);
                    let next = encode_frontier(&batch.frontier);
                    let store = Arc::clone(store);
                    on_pool(move || {
                        store.write(|tx| {
                            apply(tx, &operations, &tip_read, true, cutoff)?;
                            store::set_meta(tx, META_FRONTIER, Some(&next))
                        })?;
                        Ok(())
                    })
                    .await?;
                    self.wake_worker();
                }
            }
            Err(ReadFailure::Behind | ReadFailure::Rewritten) => {
                // Too far behind, or the log was rewritten under the
                // frontier by a GC this daemon did not map: start at the
                // heads and walk back to where the index stood, by its
                // operations or, when they are gone, by their end time.
                let (heads, stop_at) = {
                    let tip = Arc::clone(&tip);
                    let ids = frontier.ids().to_vec();
                    state
                        .reads
                        .run(&handle, move || async move {
                            let heads = oplog::op_heads(&tip).await?;
                            let mut stop_at = None::<i64>;
                            for id in &ids {
                                if let Ok(entry) = oplog::lookup(&tip, id.as_str()).await {
                                    let end = ms(entry.end);
                                    stop_at = Some(stop_at.map_or(end, |s| s.min(end)));
                                }
                            }
                            Ok::<_, HistoryError>((heads, stop_at))
                        })
                        .await??
                };
                let stop_at = match stop_at {
                    Some(at) => at,
                    None => store.read(|conn| {
                        conn.query_row(
                            "SELECT coalesce(max(last_at), 0) FROM activity",
                            [],
                            |row| row.get(0),
                        )
                    })?,
                };
                tracing::info!(repo = %repo, "the history index catches up from the operation heads");
                let pending = store.read(|conn| store::meta(conn, META_CATCHUP))?;
                let mut catch_up = Backfill {
                    examined: 0,
                    next: heads.clone(),
                    stop: frontier.ids().to_vec(),
                    stop_at,
                };
                // An unfinished catch-up goes on beside this one.
                if let Some(pending) = pending.as_deref().and_then(Backfill::decode) {
                    catch_up.next.extend(pending.next);
                    catch_up.stop.extend(pending.stop);
                    catch_up.stop_at = catch_up.stop_at.min(pending.stop_at);
                    catch_up.next.sort();
                    catch_up.next.dedup();
                }
                store.write(|tx| {
                    store::set_meta(
                        tx,
                        META_FRONTIER,
                        Some(&encode_frontier(&Frontier::new(heads))),
                    )?;
                    store::set_meta(tx, META_CATCHUP, Some(&catch_up.encode()))
                })?;
                more = true;
            }
            Err(ReadFailure::Failed(status)) => {
                return Err(HistoryError::Read(status.message().to_owned()));
            }
        }
        // A catch-up first: it fills a gap in recent history.
        let catching_up = self
            .backfill_chunk(state, repo, store, &tip, META_CATCHUP, None)
            .await?;
        let backfilling = if catching_up {
            true
        } else {
            self.backfill_chunk(
                state,
                repo,
                store,
                &tip,
                META_BACKFILL,
                Some(self.config.backfill_operations),
            )
            .await?
        };
        held.backfilling.store(backfilling, Ordering::Relaxed);
        Ok(more || backfilling)
    }

    /// Reads one chunk of the unfinished walk stored at `key`, stopping at
    /// `cap` operations in all when it has one; returns whether the walk is
    /// still unfinished.
    async fn backfill_chunk(
        &self,
        state: &Arc<AppState>,
        repo: RepoId,
        store: &Arc<Store>,
        tip: &Arc<ReadonlyRepo>,
        key: &'static str,
        cap: Option<u64>,
    ) -> Result<bool, HistoryError> {
        let text = store.read(|conn| store::meta(conn, key))?;
        let Some(mut backfill) = text.as_deref().and_then(Backfill::decode) else {
            return Ok(false);
        };
        #[cfg(any(test, feature = "testing"))]
        if self.backfill_paused.load(Ordering::Relaxed) {
            return Ok(true);
        }
        let handle = state.repos.get(repo).ok_or(HistoryError::NotOpen(repo))?;
        let cutoff = ms(SystemTime::now()) - ms_of(self.config.retention);
        let left = cap
            .map_or(usize::MAX, |cap| {
                usize::try_from(cap.saturating_sub(backfill.examined)).unwrap_or(usize::MAX)
            })
            .min(BACKFILL_CHUNK);
        let (walked, stopped, next) = {
            let tip = Arc::clone(tip);
            let start = backfill.next.clone();
            let stop: HashSet<OperationIdHex> = backfill.stop.iter().cloned().collect();
            let stop_at = backfill.stop_at;
            state
                .reads
                .run(&handle, move || async move {
                    walk_chunk(&tip, &start, &stop, stop_at, left, cutoff).await
                })
                .await??
        };
        let walked_count = walked.len();
        let (operations, tip_read) = {
            let (state2, tip) = (Arc::clone(state), Arc::clone(tip));
            state
                .reads
                .run(&handle, move || async move {
                    let entries: Vec<oplog::OpEntry> = walked.iter().map(oplog::entry).collect();
                    let records = Records::of(&state2, &entries)
                        .map_err(|status| HistoryError::Read(status.message().to_owned()))?;
                    let mut operations = Vec::with_capacity(walked.len());
                    for (operation, entry) in walked.iter().zip(entries) {
                        let delivered =
                            feed::deliver(&state2, repo, &tip, &records, operation, entry).await;
                        operations.push(delivered.message);
                    }
                    // Outcomes as of the current head, loaded on this pool.
                    let heads = oplog::op_heads(&tip).await?;
                    let head = heads.last().map(ToString::to_string);
                    let at = at_operation(&tip, head.as_deref()).await?;
                    Ok::<_, HistoryError>(read_tip(&at, operations, None, false).await?)
                })
                .await??
        };
        backfill.examined += u64::try_from(walked_count).unwrap_or(u64::MAX);
        backfill.next = next;
        let done =
            stopped || backfill.next.is_empty() || cap.is_some_and(|cap| backfill.examined >= cap);
        let wrote = !operations.is_empty();
        let examined = backfill.examined;
        let store = Arc::clone(store);
        on_pool(move || {
            store.write(|tx| {
                apply(tx, &operations, &tip_read, false, cutoff)?;
                store::set_meta(tx, key, (!done).then(|| backfill.encode()).as_deref())
            })?;
            Ok(())
        })
        .await?;
        if wrote {
            self.wake_worker();
        }
        if done {
            tracing::info!(repo = %repo, walk = key, examined_count = examined, "history walk finished");
        }
        Ok(!done)
    }

    /// Maps the operation ids `repo`'s index names through GC's rewrite, so
    /// the follower continues without meeting a rewritten log. Call it with
    /// the follower paused ([`History::pause`]).
    pub(crate) fn rewrite(
        &self,
        repo: RepoId,
        rewritten: &[(OperationIdHex, OperationIdHex)],
    ) -> Result<(), HistoryError> {
        if rewritten.is_empty() {
            return Ok(());
        }
        let Some(held) = self.held(repo, false)? else {
            return Ok(());
        };
        let map: HashMap<&str, &str> = rewritten
            .iter()
            .map(|(old, new)| (old.as_str(), new.as_str()))
            .collect();
        let map_ids = |text: &str| -> String {
            text.split(',')
                .filter_map(|id| map.get(id).map(|new| (*new).to_owned()))
                .collect::<BTreeSet<_>>()
                .into_iter()
                .collect::<Vec<_>>()
                .join(",")
        };
        held.store.write(|tx| {
            if let Some(frontier) = store::meta(tx, META_FRONTIER)? {
                let mapped = map_ids(&frontier);
                store::set_meta(
                    tx,
                    META_FRONTIER,
                    (!mapped.is_empty()).then_some(mapped.as_str()),
                )?;
            }
            for key in [META_BACKFILL, META_CATCHUP] {
                let Some(walk) = store::meta(tx, key)?.as_deref().and_then(Backfill::decode) else {
                    continue;
                };
                // The walk's operations were rewritten too: it continues
                // from their rewrites, or stops when they were abandoned.
                let remap = |ids: &[OperationIdHex]| -> Vec<OperationIdHex> {
                    ids.iter()
                        .filter_map(|id| map.get(id.as_str()).and_then(|new| new.parse().ok()))
                        .collect()
                };
                let mapped = Backfill {
                    next: remap(&walk.next),
                    stop: remap(&walk.stop),
                    ..walk
                };
                let done = mapped.next.is_empty();
                store::set_meta(tx, key, (!done).then(|| mapped.encode()).as_deref())?;
            }
            // One statement through a map table: activity can hold many
            // rows, and `last_operation_id` has no index.
            tx.execute_batch(
                "CREATE TEMP TABLE IF NOT EXISTS op_map (old TEXT PRIMARY KEY, new TEXT NOT NULL);
                 DELETE FROM temp.op_map;",
            )?;
            {
                let mut insert = tx.prepare_cached(
                    "INSERT OR REPLACE INTO temp.op_map (old, new) VALUES (?1, ?2)",
                )?;
                for (old, new) in rewritten {
                    insert.execute([old.as_str(), new.as_str()])?;
                }
            }
            tx.execute(
                "UPDATE activity SET last_operation_id =
                   (SELECT new FROM temp.op_map WHERE old = activity.last_operation_id)
                 WHERE last_operation_id IS NOT NULL",
                [],
            )?;
            tx.execute_batch("DROP TABLE temp.op_map;")
        })?;
        Ok(())
    }
}

/// `tip`'s repository loaded at operation `id`, or `tip` itself when it
/// is `None`, is `tip`'s own operation, or cannot be found.
async fn at_operation(
    tip: &Arc<ReadonlyRepo>,
    id: Option<&str>,
) -> Result<Arc<ReadonlyRepo>, oplog::OpLogError> {
    let Some(id) = id.filter(|id| *id != oplog::op_id_of(tip).as_str()) else {
        return Ok(Arc::clone(tip));
    };
    match oplog::resolve_operation(tip, id).await {
        Ok(operation) => oplog::load_at(tip, &operation).await,
        Err(_) => Ok(Arc::clone(tip)),
    }
}

/// Runs `work`, which uses SQLite, on the blocking pool.
pub(crate) async fn on_pool<T: Send + 'static>(
    work: impl FnOnce() -> Result<T, HistoryError> + Send + 'static,
) -> Result<T, HistoryError> {
    tokio::task::spawn_blocking(work)
        .await
        .map_err(|error| HistoryError::Read(error.to_string()))?
}

/// Milliseconds in `d`.
pub(crate) fn ms_of(d: Duration) -> i64 {
    i64::try_from(d.as_millis()).unwrap_or(i64::MAX)
}

/// Walks from `start` newest first for at most `left` operations, stopping
/// at a member of `stop` or an operation that ended before `cutoff`.
/// Returns the operations, whether it stopped, and where to continue.
async fn walk_chunk(
    tip: &ReadonlyRepo,
    start: &[OperationIdHex],
    stop: &HashSet<OperationIdHex>,
    stop_at: i64,
    left: usize,
    cutoff: i64,
) -> Result<(Vec<reef_lib::jj::Operation>, bool, Vec<OperationIdHex>), oplog::OpLookupError> {
    let mut walked = Vec::new();
    let mut stopped = false;
    if left == 0 || start.is_empty() {
        return Ok((walked, true, Vec::new()));
    }
    let walk = oplog::walk_from(tip, start, 0, async |op| {
        let entry = oplog::entry(op);
        let end = ms(entry.end);
        if stop.contains(&entry.id)
            || end < cutoff
            || (stop_at > 0 && end <= stop_at)
            || entry.parents.is_empty()
        {
            stopped = true;
            return ControlFlow::Break(());
        }
        walked.push(op.clone());
        if walked.len() >= left {
            ControlFlow::Break(())
        } else {
            ControlFlow::Continue(())
        }
    })
    .await;
    match walk {
        Ok(walk) if walk.ended => stopped = true,
        Ok(_) => {}
        // The walk's start was collected (another client's GC): stop.
        Err(oplog::OpLookupError::NotFound(_)) => return Ok((Vec::new(), true, Vec::new())),
        Err(error) => return Err(error),
    }
    let seen: HashSet<OperationIdHex> = walked.iter().map(|op| oplog::entry(op).id).collect();
    let mut next: BTreeSet<OperationIdHex> = BTreeSet::new();
    if !stopped {
        for op in &walked {
            for parent in oplog::entry(op).parents {
                if !seen.contains(&parent) {
                    next.insert(parent);
                }
            }
        }
    }
    Ok((walked, stopped, next.into_iter().collect()))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, reason = "tests")]

    use super::*;

    #[test]
    fn backfill_round_trips() {
        let id = |c: char| -> OperationIdHex { c.to_string().repeat(128).parse().unwrap() };
        let backfill = Backfill {
            examined: 42,
            next: vec![id('a'), id('b')],
            stop: vec![id('c')],
            stop_at: 1234,
        };
        assert_eq!(Backfill::decode(&backfill.encode()), Some(backfill));
        let empty = Backfill {
            examined: 0,
            next: Vec::new(),
            stop: Vec::new(),
            stop_at: 0,
        };
        assert_eq!(Backfill::decode(&empty.encode()), Some(empty));
    }

    /// Review finding: a rewrite of the commit `trunk()` points at puts its
    /// change in both the landed and the unlanded lists; it stays landed.
    #[test]
    fn trunk_rewrite_keeps_landed() {
        let dir = tempfile::tempdir().unwrap();
        let (store, _) = Store::open(&dir.path().join("h.sqlite3"), 1).unwrap();
        let change: ChangeIdHex = "ab".repeat(16).parse().unwrap();
        store
            .write(|tx| {
                let id = store::insert_change(tx, change.as_str(), "c1", 10)?;
                tx.execute(
                    "UPDATE changes SET outcome = ?2, landed_at = 5 WHERE id = ?1",
                    rusqlite::params![id, Outcome::Landed as i64],
                )?;
                let read = TipRead {
                    landed: vec![change.clone()],
                    unlanded: vec![change.clone()],
                    ..TipRead::default()
                };
                apply(tx, &[], &read, true, 0)
            })
            .unwrap();
        let (outcome, landed_at): (i64, Option<i64>) = store
            .read(|conn| {
                conn.query_row("SELECT outcome, landed_at FROM changes", [], |row| {
                    Ok((row.get(0)?, row.get(1)?))
                })
            })
            .unwrap();
        assert_eq!(outcome, Outcome::Landed as i64);
        assert_eq!(landed_at, Some(5));
    }

    #[test]
    fn named_commit_reads_reef_descriptions() {
        assert_eq!(named_commit("reef: describe commit abc"), Some("abc"));
        assert_eq!(named_commit("reef: edit commit abc for x"), Some("abc"));
        assert_eq!(named_commit("snapshot working copy"), None);
    }
}
