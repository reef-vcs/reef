//! Batches and bisect (R3.14, ADR 0067; `docs/landing.md`).
//!
//! The worker lands queued changes in batches: the queued changes for one
//! destination, oldest first, up to the destination policy's `[landing]
//! batch_max`, each judged as R3.13 judges it, then rebased one on another
//! onto the destination's head (the chain). Under `checks = "batch"`, each
//! member's `[[require]]` rules are judged again over the evidence on the
//! tested tree, the chain's head first: the tree with every member up to
//! the tested prefix. The executor is asked only for the pairs missing
//! there, one job per kind, on a trial commit carrying the tree. A passing
//! head lands every member in one operation; a failing one is bisected over
//! the prefixes, the passing prefix lands, the first failing member returns
//! to proposed with reason `checks`, and the rest stay queued. No lock is
//! held while checks run: the destination and the members are checked on
//! every wake-up, and a move re-forms the batch, which costs nothing for
//! trees already tested since evidence is by tree and jobs dedupe.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures::FutureExt as _;
use reef_lib::error::ErrorChain;
use reef_lib::jj::land::{self, LandError, LandPlan, LandTarget, PlannedMember};
use reef_lib::jj::lifecycle::{LifecycleState, TransitionCause, TransitionRecord};
use reef_lib::jj::oplog::{self, OpMeta};
use reef_lib::jj::{CommitIdHex, OpKind, files};
use reef_policy::{Checks, Evaluation, Need, RuleKind, Status};

use super::worker::{
    dequeue, dequeue_locked, fail, find_written, finish, operation_end, record_policy,
    refused_before_writing, settled, who_of,
};
use super::{
    JudgeContext, Judged, LandingError, MAX_ATTEMPTS, Outcome, Reason, Requester, Verdict,
    change_hex, change_row, commit_hex, judge, judge_with, pins,
};
use crate::daemon::AppState;
use crate::db::{
    BatchMemberRow, BatchRow, BatchTrialRow, StoredBatch, StoredLanding, TransitionRow,
};
use crate::executor::submit::NewJob;
use crate::lifecycle::Actor;
use crate::model::{RepoId, Timestamp};
use crate::policy::Loaded;
use crate::service::policy as policy_input;
use crate::workspace::{ManagerError, Write, WriteRequest};

/// The priority of a batch's jobs, above a proposal's (R3.21 replaces it).
pub const BATCH_PRIORITY: i32 = 1_000;

/// How long a batch's job may wait unclaimed before it expires.
pub const BATCH_QUEUE_TIMEOUT: Duration = Duration::from_mins(15);

/// How many times a trial's jobs are asked for before it is unavailable.
pub const MAX_TRIAL_ATTEMPTS: u32 = 2;

/// The `reef-agent` a batch's jobs and operations record.
pub const AGENT: &str = "reef-landing";

/// How often a waiting batch looks again with no executor news.
const TICK: Duration = Duration::from_secs(5);

/// The shortest time between two evaluations of a waiting trial.
const ROUND: Duration = Duration::from_secs(1);

/// The most needs a trial row keeps.
const MAX_NEEDS: usize = 20;

/// What a repository's worker keeps from one batch to the next.
#[derive(Debug, Default)]
pub(crate) struct Pace {
    /// Batches re-formed in a row because the destination moved.
    moves: usize,
    /// Formations in a row that found the destination moving.
    forming: usize,
    /// Landings that land in a batch of their own: a landing of several
    /// failed in a way that named none of them.
    solo: HashSet<String>,
}

/// What [`form`] found.
enum Formation {
    /// A batch.
    Formed(Box<Formed>),
    /// Nothing to batch now; `progress` when a candidate was finished or
    /// dequeued (look again), else none could be judged (wait).
    Nothing { progress: bool },
    /// The destination moved while the batch formed.
    Moved,
}

/// What the worker does after a batch.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Next {
    /// Look at the queue again now.
    Again,
    /// Nothing more to do now.
    Idle,
}

/// The destination policy's `[landing]` table, as a batch reads it.
struct Settings {
    checks: Checks,
    max: usize,
    wait: Duration,
    loaded: Option<Arc<Loaded>>,
    commit: Option<String>,
    sha256: Option<String>,
}

impl Settings {
    fn alone(&self, paths: &[String]) -> bool {
        let paths = paths.iter().map(String::as_str);
        match self.loaded.as_deref() {
            Some(Loaded::Policy(policy)) => policy.landing().alone(paths),
            _ => reef_policy::LandingSettings::default().alone(paths),
        }
    }
}

/// One member of a forming or formed batch.
struct Member {
    landing: StoredLanding,
    target: LandTarget,
    who: Requester,
    queued_tree: Option<String>,
    judged: Box<Judged>,
    /// The `landing` input from the chain.
    input: reef_policy::input::Landing,
    /// The chain's tree with it applied.
    planned: String,
}

/// A batch being driven.
struct Formed {
    stored: StoredBatch,
    members: Vec<Member>,
    checks: Checks,
}

impl Formed {
    fn id(&self) -> &str {
        &self.stored.row.id
    }

    fn base(&self) -> &str {
        &self.stored.row.base_commit
    }

    fn destination(&self) -> &str {
        &self.stored.row.destination
    }

    fn save(&self, state: &AppState) -> Result<(), LandingError> {
        Ok(state.db.write_batch(&self.stored)?)
    }

    fn member_result(&mut self, index: usize, result: &str) {
        if let Some(member) = self.stored.members.get_mut(index) {
            member.result = Some(result.to_owned());
        }
    }

    /// The trial commits this batch wrote or reused.
    fn trial_commits(&self) -> Vec<CommitIdHex> {
        let mut commits: Vec<CommitIdHex> = self
            .stored
            .trials
            .iter()
            .filter_map(|trial| trial.commit_id.as_deref().and_then(commit_hex))
            .collect();
        commits.sort();
        commits.dedup();
        commits
    }

    /// The `batch` input of member `index` (from 0) judged on `trial`.
    fn input(&self, index: usize, trial: Option<&BatchTrialRow>) -> reef_policy::Batch {
        reef_policy::Batch {
            id: self.id().to_owned(),
            size: i64::try_from(self.members.len()).unwrap_or(i64::MAX),
            position: i64::try_from(index + 1).unwrap_or(i64::MAX),
            tree: trial.map(|trial| trial.tree.clone()).unwrap_or_default(),
            commit: trial
                .and_then(|trial| trial.commit_id.clone())
                .unwrap_or_default(),
        }
    }
}

/// Why a batch re-forms.
#[derive(Debug)]
enum Reform {
    /// The destination moved.
    Moved,
    /// A member left the queue, or moved off its queued commit.
    Left,
}

/// How a trial ended.
enum Trial {
    /// Every member's required evidence passed on the tree: their tested
    /// evaluations.
    Passed(Vec<Evaluation>),
    /// Some failed: the tested evaluations.
    Failed(Vec<Evaluation>),
    /// No verdict could be had: the tested evaluations and why.
    Unavailable(Vec<Evaluation>, String),
    /// The batch must re-form.
    Reform(Reform),
    /// The daemon is stopping.
    Stopping,
}

/// What a member's tested evaluation says.
enum Checked {
    Passed,
    Failed(Vec<Need>),
    Missing(Vec<Need>),
}

/// The `[[require]]` results of `evaluation`.
fn checked(evaluation: &Evaluation) -> Checked {
    let mut failed = Vec::new();
    let mut missing = Vec::new();
    for rule in &evaluation.rules {
        if rule.kind != RuleKind::Require {
            continue;
        }
        match rule.status {
            Status::Satisfied | Status::NotApplicable => {}
            Status::Unsatisfied | Status::Error => {
                if !rule.failed.is_empty() {
                    failed.extend(rule.failed.iter().cloned());
                } else if !rule.missing.is_empty() {
                    missing.extend(rule.missing.iter().cloned());
                } else {
                    // An error with nothing missing: the rule cannot pass.
                    failed.push(Need {
                        kind: rule.name.clone(),
                        ..Need::default()
                    });
                }
            }
        }
    }
    if !failed.is_empty() {
        Checked::Failed(failed)
    } else if !missing.is_empty() {
        Checked::Missing(missing)
    } else {
        Checked::Passed
    }
}

/// The evidence ids the satisfied `[[require]]` rules of a tested
/// evaluation used.
fn tested_pins(evaluation: &Evaluation) -> Vec<String> {
    let mut ids: Vec<String> = evaluation
        .rules
        .iter()
        .filter(|rule| rule.kind == RuleKind::Require && rule.status == Status::Satisfied)
        .flat_map(|rule| rule.evidence_ids.iter().cloned())
        .collect();
    ids.sort();
    ids.dedup();
    ids
}

/// `{"needs": [...], "total": n}`.
fn needs_json(needs: &[Need]) -> String {
    let kept: Vec<&Need> = needs.iter().take(MAX_NEEDS).collect();
    serde_json::json!({ "needs": kept, "total": needs.len() }).to_string()
}

/// A reef-lib landing error as the service's.
fn land_error(error: LandError) -> LandingError {
    LandingError::Manager(ManagerError::Land(error))
}

/// Reads the `[landing]` table of the policy at `bookmark`'s head.
async fn settings(
    state: &AppState,
    repo: RepoId,
    bookmark: &str,
) -> Result<Settings, LandingError> {
    let handle = state.manager.handle(repo)?;
    let bookmark = bookmark.to_owned();
    let read = handle
        .call_async(move |head| {
            async move {
                let repo = head.refresh().await?;
                let Ok(destination) = land::destination_head(&repo, &bookmark) else {
                    return Ok::<_, LandingError>(None);
                };
                let tree = files::commit_tree(&repo, &destination)
                    .await
                    .map_err(|error| LandingError::Input(ErrorChain(&error).to_string()))?;
                let file = policy_input::policy_file(&tree)
                    .await
                    .map_err(|status| LandingError::Input(status.message().to_owned()))?;
                Ok(Some((destination.as_str().to_owned(), file)))
            }
            .boxed_local()
        })
        .await??;
    let mut settings = Settings {
        checks: Checks::Batch,
        max: usize::try_from(reef_policy::format::DEFAULT_BATCH_MAX).unwrap_or(8),
        wait: Duration::ZERO,
        loaded: None,
        commit: None,
        sha256: None,
    };
    let Some((commit, file)) = read else {
        return Ok(settings);
    };
    settings.commit = Some(commit);
    if let Some(Ok(bytes)) = file.map(|file| file.bytes) {
        settings.sha256 = Some(reef_policy::sha256_hex(&bytes));
        let loaded = state.policy.load(&bytes);
        if let Loaded::Policy(policy) = &*loaded {
            let table = policy.landing();
            settings.checks = table.checks;
            settings.max = usize::try_from(table.batch_max).unwrap_or(1).max(1);
            settings.wait = table.batch_wait;
        }
        settings.loaded = Some(loaded);
    }
    Ok(settings)
}

/// Drives `repo`'s next batch to its end; see the module docs. `pace`
/// is what the worker keeps between batches.
pub(crate) async fn drive(
    state: &Arc<AppState>,
    repo: RepoId,
    pace: &mut Pace,
) -> Result<Next, LandingError> {
    let open = state.db.open_landings(repo)?;
    let Some(first) = open.first() else {
        return Ok(Next::Idle);
    };
    pace.solo
        .retain(|id| open.iter().any(|landing| landing.row.id == *id));
    let destination = first.row.destination.clone();
    let candidates: Vec<StoredLanding> = open
        .into_iter()
        .filter(|landing| landing.row.destination == destination)
        .collect();
    let settings = settings(state, repo, &destination).await?;
    // A change that was in a batch before (requeued behind a culprit, or
    // re-formed) has waited already.
    if !settings.wait.is_zero()
        && candidates.len() < settings.max
        && candidates[0].row.batch_id.is_none()
    {
        let due = candidates[0]
            .row
            .created_at
            .0
            .saturating_add(i64::try_from(settings.wait.as_millis()).unwrap_or(i64::MAX));
        let now = Timestamp::now().0;
        if now < due {
            let pause = Duration::from_millis(u64::try_from(due - now).unwrap_or(0))
                .min(Duration::from_secs(1));
            tokio::select! {
                () = tokio::time::sleep(pause) => {}
                () = state.shutdown.cancelled() => return Ok(Next::Idle),
            }
            return Ok(Next::Again);
        }
    }
    let mut formed = match form(state, repo, &destination, candidates, &settings, pace).await? {
        Formation::Formed(formed) => formed,
        Formation::Nothing { progress } => {
            return Ok(if progress { Next::Again } else { Next::Idle });
        }
        Formation::Moved => {
            pace.forming += 1;
            if pace.forming > MAX_ATTEMPTS * 4 {
                // Formation keeps finding the destination moving: let the
                // next wake-up try.
                pace.forming = 0;
                return Ok(Next::Idle);
            }
            return Ok(Next::Again);
        }
    };
    pace.forming = 0;
    match run(state, repo, &mut formed, pace).await {
        Ok(next) => Ok(next),
        Err(error) => {
            let message = ErrorChain(&error).to_string();
            abort(state, repo, &mut formed, &message).await;
            Err(error)
        }
    }
}

/// Forms a batch from `candidates` (oldest first, all for `destination`):
/// judges each, dequeuing those the gate would refuse now, up to the
/// policy's size; then rebases them as a chain, leaving out a member that
/// conflicts with an earlier one.
#[expect(clippy::too_many_lines, reason = "formation's steps in order")]
async fn form(
    state: &Arc<AppState>,
    repo: RepoId,
    destination: &str,
    candidates: Vec<StoredLanding>,
    settings: &Settings,
    pace: &Pace,
) -> Result<Formation, LandingError> {
    let shared = state.manager.repo_lock(repo).read_owned().await;
    let mut members: Vec<Member> = Vec::new();
    let mut progress = false;
    for candidate in candidates {
        if members.len() >= settings.max || state.shutdown.is_cancelled() {
            break;
        }
        // Read again: an earlier step may have finished it.
        let Some(mut landing) = state
            .db
            .landing(repo, &candidate.row.id)?
            .filter(|fresh| fresh.row.outcome == Outcome::Queued.as_str())
        else {
            continue;
        };
        let Some(first) = landing.changes.first().cloned() else {
            finish(
                state,
                repo,
                &mut landing,
                Outcome::Cancelled,
                None,
                "no change",
            )?;
            progress = true;
            continue;
        };
        let (Some(change), Some(commit)) = (
            change_hex(&first.change_id),
            commit_hex(&first.queued_commit),
        ) else {
            finish(
                state,
                repo,
                &mut landing,
                Outcome::Cancelled,
                None,
                "an unreadable change",
            )?;
            progress = true;
            continue;
        };
        let target = LandTarget {
            change,
            commit,
            bookmark: destination.to_owned(),
        };
        if let Some(done) = settled(state, repo, &mut landing, &target)? {
            done?;
            progress = true;
            continue;
        }
        let who = who_of(&landing);
        // One landing that cannot be judged must not hold up the rest: it
        // stays queued with the error noted, and the next wake-up tries it
        // again.
        let verdict = match judge(state, repo, &target).await {
            Ok(verdict) => verdict,
            Err(error) => {
                tracing::warn!(repo = %repo, landing_id = %landing.row.id, error = %ErrorChain(&error), "cannot judge a queued change");
                fail(state, repo, &landing.row.id, &error);
                continue;
            }
        };
        match verdict {
            Verdict::Allow(judged) => {
                let alone = settings.alone(&judged.paths) || pace.solo.contains(&landing.row.id);
                if alone && !members.is_empty() {
                    break;
                }
                let input = policy_input::landing_input(destination, &judged.preview);
                let planned = judged.preview.landed_tree.clone().unwrap_or_default();
                members.push(Member {
                    landing,
                    target,
                    who,
                    queued_tree: first.queued_tree.clone(),
                    judged,
                    input,
                    planned,
                });
                if alone {
                    break;
                }
            }
            Verdict::Block {
                reason: Reason::Busy,
                ..
            } => return Ok(Formation::Moved),
            Verdict::Block {
                reason,
                message,
                preview,
                judged,
                destination: head,
                ..
            } => {
                landing.row.destination_before = head;
                landing.changes = vec![change_row(
                    &target,
                    first.queued_tree.clone(),
                    preview.as_ref(),
                    judged.as_deref(),
                )];
                if let Some(judged) = &judged {
                    record_policy(&mut landing, judged);
                }
                dequeue(state, repo, &mut landing, &target, &who, reason, &message).await?;
                progress = true;
            }
        }
    }
    let Some(base) = members
        .first()
        .map(|member| member.judged.preview.destination.clone())
    else {
        return Ok(Formation::Nothing { progress });
    };
    if members
        .iter()
        .any(|member| member.judged.preview.destination != base)
        || settings.commit.as_deref() != Some(base.as_str())
    {
        // The destination moved while the members were judged, or since
        // its `[landing]` table was read.
        return Ok(Formation::Moved);
    }
    if members.len() > 1 {
        let handle = state.manager.handle(repo)?;
        let targets: Vec<LandTarget> = members.iter().map(|m| m.target.clone()).collect();
        let bookmark = destination.to_owned();
        let chain = handle
            .call_async(move |head| {
                async move {
                    let repo = head.refresh().await?;
                    Ok::<_, LandingError>(land::preview_chain(&repo, &bookmark, &targets).await)
                }
                .boxed_local()
            })
            .await??;
        match chain {
            Ok(chain) if chain.destination == base => {
                let mut kept = Vec::with_capacity(members.len());
                for (mut member, found) in members.into_iter().zip(chain.members) {
                    if !found.included {
                        // It conflicts with an earlier member only: it stays
                        // queued for the next batch.
                        continue;
                    }
                    member.planned = found.tree.unwrap_or_default();
                    member.input = reef_policy::input::Landing {
                        destination: destination.to_owned(),
                        destination_commit: base.as_str().to_owned(),
                        rebased: found.rebased,
                        overlap: found.overlap,
                        overlap_total: i64::try_from(found.overlap_total).unwrap_or(i64::MAX),
                    };
                    kept.push(member);
                }
                members = kept;
            }
            Ok(_) => return Ok(Formation::Moved),
            Err(error) => {
                tracing::warn!(repo = %repo, error = %ErrorChain(&error), "cannot preview a batch's chain; landing its first change alone");
                members.truncate(1);
            }
        }
    }
    if members.is_empty() || members.iter().any(|member| member.planned.is_empty()) {
        return Ok(Formation::Moved);
    }
    let now = Timestamp::now();
    let row = BatchRow {
        repo_id: repo,
        id: crate::identity::new_id("bat_"),
        destination: destination.to_owned(),
        base_commit: base.as_str().to_owned(),
        state: "testing".to_owned(),
        outcome: None,
        size: u32::try_from(members.len()).unwrap_or(u32::MAX),
        culprit_position: None,
        landed_count: 0,
        checks: settings.checks.as_str().to_owned(),
        policy_commit: settings.commit.clone(),
        policy_sha256: settings.sha256.clone(),
        operation_id: None,
        operation_collected: false,
        message: None,
        created_at: now,
        finished_at: None,
    };
    let stored = StoredBatch {
        members: members
            .iter()
            .enumerate()
            .map(|(index, member)| BatchMemberRow {
                position: u32::try_from(index + 1).unwrap_or(u32::MAX),
                landing_id: member.landing.row.id.clone(),
                change_id: member.target.change.as_str().to_owned(),
                queued_commit: member.target.commit.as_str().to_owned(),
                planned_tree: member.planned.clone(),
                result: None,
            })
            .collect(),
        trials: Vec::new(),
        row,
    };
    let mut formed = Formed {
        stored,
        members,
        checks: settings.checks,
    };
    for (index, member) in formed.members.iter_mut().enumerate() {
        member.landing.row.destination_before = Some(base.as_str().to_owned());
        member.landing.row.batch_id = Some(formed.stored.row.id.clone());
        record_policy(&mut member.landing, &member.judged);
        let mut change = change_row(
            &member.target,
            member.queued_tree.clone(),
            None,
            Some(&*member.judged),
        );
        change.rebased = member.input.rebased;
        change.overlap = Some(
            serde_json::json!({ "paths": member.input.overlap, "total": member.input.overlap_total })
                .to_string(),
        );
        change.batch_position = Some(u32::try_from(index + 1).unwrap_or(u32::MAX));
        member.landing.changes = vec![change];
        state
            .db
            .write_landing(&member.landing.row, &member.landing.changes)?;
    }
    formed.save(state)?;
    drop(shared);
    tracing::info!(repo = %repo, batch_id = %formed.id(), size = formed.members.len(), destination, "batch formed");
    Ok(Formation::Formed(Box::new(formed)))
}

/// Tests and lands `batch`, bisecting on failure.
#[expect(
    clippy::too_many_lines,
    reason = "the head, the bisect, then the culprit"
)]
async fn run(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    pace: &mut Pace,
) -> Result<Next, LandingError> {
    let n = batch.members.len();
    if batch.checks == Checks::Proposal {
        return land_prefix(state, repo, batch, n, None, pace).await;
    }
    let head = match test(state, repo, batch, n).await? {
        Trial::Passed(evaluations) => {
            return land_prefix(state, repo, batch, n, Some(evaluations), pace).await;
        }
        Trial::Failed(evaluations) => evaluations,
        Trial::Unavailable(evaluations, why) => {
            return unavailable(state, repo, batch, (0, n), &evaluations, &why).await;
        }
        Trial::Reform(why) => return reform(state, repo, batch, &why, pace).await,
        Trial::Stopping => return Ok(Next::Idle),
    };
    // Bisect: the smallest failing prefix, `lo` passing and `hi` failing.
    "bisecting".clone_into(&mut batch.stored.row.state);
    batch.save(state)?;
    let (mut lo, mut hi) = (0usize, n);
    let mut lo_evaluations: Option<Vec<Evaluation>> = None;
    let mut hi_evaluations = head;
    while hi - lo > 1 {
        let mid = lo + (hi - lo) / 2;
        match test(state, repo, batch, mid).await? {
            Trial::Passed(evaluations) => {
                lo = mid;
                lo_evaluations = Some(evaluations);
            }
            Trial::Failed(evaluations) => {
                hi = mid;
                hi_evaluations = evaluations;
            }
            Trial::Unavailable(evaluations, why) => {
                // What passed lands; the members the probe added get no
                // verdict.
                if lo > 0
                    && let Some(next) =
                        land_prefix_part(state, repo, batch, lo, lo_evaluations, pace).await?
                {
                    return Ok(next);
                }
                return unavailable(state, repo, batch, (lo, mid), &evaluations, &why).await;
            }
            Trial::Reform(why) => return reform(state, repo, batch, &why, pace).await,
            Trial::Stopping => return Ok(Next::Idle),
        }
    }
    // `hi` is the culprit: prefix `lo` (= hi - 1) passed and adding it
    // failed.
    if lo > 0
        && let Some(next) = land_prefix_part(state, repo, batch, lo, lo_evaluations, pace).await?
    {
        return Ok(next);
    }
    let culprit = hi - 1;
    let tested = batch
        .stored
        .trials
        .iter()
        .rev()
        .find(|trial| trial.prefix as usize == hi)
        .cloned();
    let evaluation = hi_evaluations.get(culprit).cloned();
    let failed: Vec<String> = hi_evaluations
        .iter()
        .flat_map(|evaluation| match checked(evaluation) {
            Checked::Failed(needs) => needs,
            _ => Vec::new(),
        })
        .map(|need| {
            if need.target.is_empty() {
                need.kind
            } else {
                format!("{}:{}", need.kind, need.target)
            }
        })
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let message = format!(
        "required checks failed on the batch's tree {} with this change (prefix {hi} of {n}), and passed without it: {}",
        tested.as_ref().map_or("", |trial| trial.tree.as_str()),
        failed.join(", ")
    );
    {
        let _shared = state.manager.repo_lock(repo).read_owned().await;
        dequeue_member(
            state,
            repo,
            batch,
            culprit,
            Reason::Checks,
            &message,
            (evaluation.as_ref(), tested.as_ref()),
        )
        .await?;
    }
    batch.member_result(culprit, "culprit");
    for index in hi..n {
        batch.member_result(index, "requeued");
    }
    batch.stored.row.culprit_position = Some(u32::try_from(hi).unwrap_or(u32::MAX));
    end_trials(state, repo, batch).await;
    end(state, batch, "split", &message)?;
    pace.moves = 0;
    Ok(Next::Again)
}

/// Tests prefix `k` (from 1) of `batch`: evaluates its members over the
/// evidence on its tree, asks the executor for what is missing, and waits.
#[expect(clippy::too_many_lines, reason = "a trial's loop in order")]
async fn test(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    k: usize,
) -> Result<Trial, LandingError> {
    let tree = batch.members[k - 1].planned.clone();
    let seq = u32::try_from(batch.stored.trials.len()).unwrap_or(u32::MAX);
    batch.stored.trials.push(BatchTrialRow {
        seq,
        prefix: u32::try_from(k).unwrap_or(u32::MAX),
        tree: tree.clone(),
        commit_id: None,
        operation_id: None,
        operation_collected: false,
        result: None,
        job_ids: Vec::new(),
        needs: None,
        attempts: 1,
        created_at: Timestamp::now(),
        finished_at: None,
    });
    batch.save(state)?;
    let trial_index = batch.stored.trials.len() - 1;
    let mut round: Vec<String> = Vec::new();
    loop {
        if state.shutdown.is_cancelled() {
            return Ok(Trial::Stopping);
        }
        let changed = state.executor.changed().notified();
        tokio::pin!(changed);
        changed.as_mut().enable();
        if let Some(why) = check(state, repo, batch).await? {
            cancel_jobs(state, &round, "the batch re-formed");
            close_trial(batch, trial_index, "cancelled", None);
            return Ok(Trial::Reform(why));
        }
        // How the jobs ended, read before the evidence: a job that completes
        // after this is seen by the next round, never taken for one that
        // ended with nothing.
        let finished = if round.is_empty() {
            None
        } else {
            ended(state, &round)?
        };
        let trial = batch.stored.trials[trial_index].clone();
        let mut evaluations = Vec::with_capacity(k);
        for index in 0..k {
            let member = &batch.members[index];
            let context = JudgeContext {
                landing: Some(member.input.clone()),
                batch: batch.input(index, Some(&trial)),
                evidence_tree: Some(tree.clone()),
            };
            match judge_with(state, repo, &member.target, &context).await? {
                Verdict::Allow(judged)
                | Verdict::Block {
                    judged: Some(judged),
                    ..
                } => evaluations.push(judged.evaluation),
                Verdict::Block { .. } => {
                    cancel_jobs(state, &round, "the batch re-formed");
                    close_trial(batch, trial_index, "cancelled", None);
                    return Ok(Trial::Reform(Reform::Left));
                }
            }
        }
        let mut failed = Vec::new();
        let mut missing = Vec::new();
        for evaluation in &evaluations {
            match checked(evaluation) {
                Checked::Passed => {}
                Checked::Failed(needs) => failed.extend(needs),
                Checked::Missing(needs) => missing.extend(needs),
            }
        }
        if !failed.is_empty() {
            cancel_jobs(state, &round, "the batch's tree failed");
            close_trial(batch, trial_index, "failed", Some(&failed));
            batch.save(state)?;
            tracing::info!(repo = %repo, batch_id = %batch.id(), prefix = k, "trial failed");
            return Ok(Trial::Failed(evaluations));
        }
        if missing.is_empty() {
            cancel_jobs(state, &round, "the batch's tree passed");
            close_trial(batch, trial_index, "passed", None);
            batch.save(state)?;
            tracing::info!(repo = %repo, batch_id = %batch.id(), prefix = k, "trial passed");
            return Ok(Trial::Passed(evaluations));
        }
        if round.is_empty() {
            round = request(state, repo, batch, trial_index, k, &missing).await?;
            if round.is_empty() {
                let why = "the executor could not be asked for the missing evidence".to_owned();
                close_trial(batch, trial_index, "unavailable", Some(&missing));
                return Ok(Trial::Unavailable(evaluations, why));
            }
        } else if let Some(ended) = finished {
            let attempts = batch.stored.trials[trial_index].attempts;
            if ended.unclaimed || attempts >= MAX_TRIAL_ATTEMPTS {
                let why = unavailable_why(state, repo, &tree, &ended)?;
                close_trial(batch, trial_index, "unavailable", Some(&missing));
                batch.save(state)?;
                return Ok(Trial::Unavailable(evaluations, why));
            }
            batch.stored.trials[trial_index].attempts += 1;
            round = request(state, repo, batch, trial_index, k, &missing).await?;
            if round.is_empty() {
                let why = unavailable_why(state, repo, &tree, &ended)?;
                close_trial(batch, trial_index, "unavailable", Some(&missing));
                return Ok(Trial::Unavailable(evaluations, why));
            }
        }
        tokio::select! {
            () = &mut changed => {}
            () = tokio::time::sleep(TICK) => {}
            () = state.shutdown.cancelled() => return Ok(Trial::Stopping),
        }
        tokio::time::sleep(ROUND).await;
    }
}

/// Sets trial `index`'s result and needs.
fn close_trial(batch: &mut Formed, index: usize, result: &str, needs: Option<&[Need]>) {
    if let Some(trial) = batch.stored.trials.get_mut(index) {
        trial.result = Some(result.to_owned());
        trial.needs = needs.map(needs_json);
        trial.finished_at = Some(Timestamp::now());
    }
}

/// Whether the batch must re-form: the destination moved, or a member is
/// no longer queued at its queued commit.
async fn check(
    state: &AppState,
    repo: RepoId,
    batch: &Formed,
) -> Result<Option<Reform>, LandingError> {
    for member in &batch.members {
        let row = state
            .db
            .lifecycle_row(repo, member.target.change.as_str())?;
        let queued = row.is_some_and(|row| {
            row.state == LifecycleState::Queued.as_str()
                && row.commit_id == member.target.commit.as_str()
        });
        if !queued {
            return Ok(Some(Reform::Left));
        }
    }
    let handle = state.manager.handle(repo)?;
    let bookmark = batch.destination().to_owned();
    let head = handle
        .call_async(move |head| {
            async move {
                let repo = head.refresh().await?;
                Ok::<_, LandingError>(land::destination_head(&repo, &bookmark).ok())
            }
            .boxed_local()
        })
        .await??;
    if head.as_ref().map(CommitIdHex::as_str) != Some(batch.base()) {
        return Ok(Some(Reform::Moved));
    }
    Ok(None)
}

/// Asks the executor for `missing` on trial `index`'s tree (prefix `k`):
/// the trial commit first, then one job per kind. Returns the job ids.
async fn request(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    index: usize,
    k: usize,
    missing: &[Need],
) -> Result<Vec<String>, LandingError> {
    let tree = batch.stored.trials[index].tree.clone();
    let (commit, change) = trial_commit(state, repo, batch, index, k).await?;
    // One job per kind with its targets, and one per kind for its whole
    // check: an executor claims only a job whose kinds are all its own.
    let mut groups: BTreeMap<(String, bool), BTreeSet<String>> = BTreeMap::new();
    for need in missing {
        let whole = need.target.is_empty();
        let targets = groups.entry((need.kind.clone(), whole)).or_default();
        if !whole {
            targets.insert(need.target.clone());
        }
    }
    let mut ids = Vec::new();
    for ((kind, _), targets) in groups {
        let job = NewJob {
            repo_id: repo,
            tree_id: tree.clone(),
            commit_id: Some(commit.clone()),
            change_id: Some(change.clone()),
            kinds: vec![kind.clone()],
            targets: targets.into_iter().collect(),
            requirements: Vec::new(),
            wall_time: None,
            cpu_time: None,
            priority: BATCH_PRIORITY,
            queue_timeout: Some(BATCH_QUEUE_TIMEOUT),
            duplicate: false,
            agent: Some(AGENT.to_owned()),
            token: None,
        };
        match state.executor.submit(&job) {
            Ok((row, _)) => ids.push(row.id),
            Err(error) => {
                tracing::warn!(repo = %repo, batch_id = %batch.id(), kind, error = %ErrorChain(&error), "cannot ask the executor for a batch's evidence");
            }
        }
    }
    let trial = &mut batch.stored.trials[index];
    for id in &ids {
        if !trial.job_ids.contains(id) {
            trial.job_ids.push(id.clone());
        }
    }
    batch.save(state)?;
    tracing::info!(repo = %repo, batch_id = %batch.id(), prefix = k, jobs = ids.len(), "asked the executor for a batch's evidence");
    Ok(ids)
}

/// Trial `index`'s commit and its change: one already written onto the
/// base with its tree, if still visible, or a new one.
async fn trial_commit(
    state: &AppState,
    repo: RepoId,
    batch: &mut Formed,
    index: usize,
    k: usize,
) -> Result<(String, String), LandingError> {
    let tree = batch.stored.trials[index].tree.clone();
    let base = batch.base().to_owned();
    let reuse = state
        .db
        .trial_commit_for(repo, &base, &tree)?
        .and_then(|(commit, op)| commit_hex(&commit).map(|commit| (commit, op)));
    let id = batch.id().to_owned();
    let n = batch.members.len();
    let destination = batch.destination().to_owned();
    let changes: Vec<String> = batch.members[..k]
        .iter()
        .map(|member| member.target.change.as_str().to_owned())
        .collect();
    let _shared = state.manager.repo_lock(repo).read_owned().await;
    let handle = state.manager.handle(repo)?;
    let written = handle
        .call_async(move |head| {
            async move {
                let repo = head.refresh().await?;
                if let Some((commit, op)) = reuse
                    && let Some(change) = land::visible_change(&repo, &commit)
                        .await
                        .map_err(land_error)?
                {
                    return Ok((commit.as_str().to_owned(), change.as_str().to_owned(), op));
                }
                let base = commit_hex(&base)
                    .ok_or_else(|| LandingError::Input(format!("{base:?} is not a commit id")))?;
                let description = format!(
                    "reef: batch {id} prefix {k} of {n} onto {destination}\n\n{}\n",
                    changes.join("\n")
                );
                let mut meta = OpMeta::new(
                    OpKind::Batch,
                    format!(
                        "{}{id} on {destination} (prefix {k} of {n})",
                        oplog::TEST_BATCH_PREFIX
                    ),
                );
                meta.batch = Some(id.clone());
                meta.agent = Some(AGENT.to_owned());
                let trial = head
                    .repo
                    .write_trial(&base, &tree, &description, &meta)
                    .await
                    .map_err(land_error)?;
                Ok::<_, LandingError>((
                    trial.commit.as_str().to_owned(),
                    trial.change.as_str().to_owned(),
                    Some(trial.op_id.to_string()),
                ))
            }
            .boxed_local()
        })
        .await??;
    let (commit, change, op) = written;
    let trial = &mut batch.stored.trials[index];
    trial.commit_id = Some(commit.clone());
    trial.operation_id = op;
    batch.save(state)?;
    Ok((commit, change))
}

/// How a round of jobs ended, when every one has.
struct Ended {
    /// Some job expired with nobody claiming it.
    unclaimed: bool,
    /// Each job and how it ended.
    how: Vec<String>,
}

/// Whether every job of `round` ended, and how.
fn ended(state: &AppState, round: &[String]) -> Result<Option<Ended>, LandingError> {
    use crate::db::JobState;
    let mut ended = Ended {
        unclaimed: false,
        how: Vec::new(),
    };
    for id in round {
        let Some(job) = state.db.job(id)? else {
            ended.how.push(format!("{id} is gone"));
            continue;
        };
        match job.state {
            JobState::Queued | JobState::Running => return Ok(None),
            JobState::Expired if job.end_reason.as_deref() == Some("queue_timeout") => {
                ended.unclaimed = true;
                ended.how.push(format!(
                    "{id} expired unclaimed after {}m",
                    BATCH_QUEUE_TIMEOUT.as_secs() / 60
                ));
            }
            other => ended.how.push(format!(
                "{id} {}{}",
                other.as_str(),
                job.end_reason
                    .as_deref()
                    .or(job.outcome.as_deref())
                    .map(|why| format!(" ({why})"))
                    .unwrap_or_default()
            )),
        }
    }
    Ok(Some(ended))
}

/// The message of an `unavailable` landing: whether any executor claimed a
/// job of the repository in the last day, and how the jobs ended.
fn unavailable_why(
    state: &AppState,
    repo: RepoId,
    tree: &str,
    ended: &Ended,
) -> Result<String, LandingError> {
    let day = Timestamp(state.executor.now().0 - 24 * 60 * 60 * 1000);
    let how = ended.how.join(", ");
    Ok(if state.db.job_claimed_since(repo, day)? {
        format!("no verdict on the batch's tree {tree}: {how}")
    } else {
        format!(
            "no executor claimed a job of this repository in the last day; no verdict on the batch's tree {tree}: {how}"
        )
    })
}

/// Cancels the open jobs of `round`.
fn cancel_jobs(state: &AppState, round: &[String], why: &str) {
    for id in round {
        match state.executor.cancel(id, None, Some(AGENT), Some(why)) {
            Ok(_) | Err(crate::executor::ExecutorError::Finished { .. }) => {}
            Err(error) => {
                tracing::debug!(job_id = %id, error = %ErrorChain(&error), "cannot cancel a batch's job");
            }
        }
    }
}

/// Cancels every open job of `batch`'s trials.
fn cancel_all(state: &AppState, batch: &Formed, why: &str) {
    for trial in &batch.stored.trials {
        if trial.result.is_none() {
            cancel_jobs(state, &trial.job_ids, why);
        }
    }
}

/// Abandons `batch`'s trial commits, and those of interrupted batches, in
/// an operation of kind `batch`; a failure is logged.
async fn end_trials(state: &AppState, repo: RepoId, batch: &Formed) {
    let mut commits = batch.trial_commits();
    if let Ok(more) = state.db.interrupted_trial_commits(repo) {
        commits.extend(more.iter().filter_map(|commit| commit_hex(commit)));
    }
    commits.sort();
    commits.dedup();
    if commits.is_empty() {
        return;
    }
    let _shared = state.manager.repo_lock(repo).read_owned().await;
    let Ok(handle) = state.manager.handle(repo) else {
        return;
    };
    let id = batch.id().to_owned();
    let ended = handle
        .call_async(move |head| {
            async move {
                head.refresh().await?;
                let mut meta =
                    OpMeta::new(OpKind::Batch, format!("{}{id}", oplog::END_BATCH_PREFIX));
                meta.batch = Some(id);
                meta.agent = Some(AGENT.to_owned());
                head.repo
                    .end_trials(&commits, &meta)
                    .await
                    .map_err(land_error)
            }
            .boxed_local()
        })
        .await;
    if let Err(error) | Ok(Err(error)) = ended.map_err(LandingError::from) {
        tracing::warn!(repo = %repo, batch_id = %batch.id(), error = %ErrorChain(&error), "cannot abandon a batch's trial commits");
    }
}

/// Finishes `batch` with `outcome`.
fn end(
    state: &AppState,
    batch: &mut Formed,
    outcome: &str,
    message: &str,
) -> Result<(), LandingError> {
    let row = &mut batch.stored.row;
    "finished".clone_into(&mut row.state);
    row.outcome = Some(outcome.to_owned());
    row.message = Some(message.to_owned()).filter(|message| !message.is_empty());
    row.finished_at = Some(Timestamp::now());
    for trial in &mut batch.stored.trials {
        if trial.result.is_none() {
            trial.result = Some("cancelled".to_owned());
            trial.finished_at = row.finished_at;
        }
    }
    batch.save(state)?;
    tracing::info!(batch_id = %batch.id(), outcome, "batch finished");
    Ok(())
}

/// Ends `batch` after an error: its jobs cancelled, its trials abandoned,
/// `reformed` with the error.
async fn abort(state: &AppState, repo: RepoId, batch: &mut Formed, message: &str) {
    cancel_all(state, batch, "the batch failed");
    end_trials(state, repo, batch).await;
    if let Err(error) = end(state, batch, "reformed", message) {
        tracing::warn!(batch_id = %batch.id(), error = %ErrorChain(&error), "cannot finish a failed batch");
    }
}

/// Re-forms: the batch's jobs cancelled, its trial commits abandoned, the
/// members that left finished, and the batch finished `reformed` (`busy`
/// after the destination moved under three batches in a row, dequeuing its
/// members).
async fn reform(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    why: &Reform,
    pace: &mut Pace,
) -> Result<Next, LandingError> {
    cancel_all(state, batch, "the batch re-formed");
    end_trials(state, repo, batch).await;
    for index in 0..batch.members.len() {
        refresh(state, repo, &mut batch.members[index])?;
        let member = &mut batch.members[index];
        if let Some(done) = settled(state, repo, &mut member.landing, &member.target)? {
            done?;
            batch.member_result(index, "left");
        }
    }
    match why {
        Reform::Moved => {
            pace.moves += 1;
            if pace.moves >= MAX_ATTEMPTS {
                pace.moves = 0;
                let message = format!(
                    "{} moved under {MAX_ATTEMPTS} batches in a row",
                    batch.destination()
                );
                let _shared = state.manager.repo_lock(repo).read_owned().await;
                for index in 0..batch.members.len() {
                    if batch.stored.members[index].result.is_none() {
                        dequeue_member(
                            state,
                            repo,
                            batch,
                            index,
                            Reason::Busy,
                            &message,
                            (None, None),
                        )
                        .await?;
                        batch.member_result(index, "dequeued");
                    }
                }
                end(state, batch, "busy", &message)?;
                return Ok(Next::Again);
            }
            end(state, batch, "reformed", "the destination moved")?;
        }
        Reform::Left => {
            pace.moves = 0;
            end(state, batch, "reformed", "a change left the batch")?;
        }
    }
    Ok(Next::Again)
}

/// Dequeues the tested members `from..k` (from 0) with reason
/// `unavailable`, and leaves the rest queued.
async fn unavailable(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    (from, k): (usize, usize),
    evaluations: &[Evaluation],
    why: &str,
) -> Result<Next, LandingError> {
    let tested = batch.stored.trials.last().cloned();
    let shared = state.manager.repo_lock(repo).read_owned().await;
    for index in from..k {
        dequeue_member(
            state,
            repo,
            batch,
            index,
            Reason::Unavailable,
            why,
            (evaluations.get(index), tested.as_ref()),
        )
        .await?;
        batch.member_result(index, "unavailable");
    }
    drop(shared);
    for index in k..batch.members.len() {
        batch.member_result(index, "requeued");
    }
    end_trials(state, repo, batch).await;
    end(state, batch, "unavailable", why)?;
    Ok(Next::Again)
}

/// Reads `member`'s landing row again, keeping the changes the batch set:
/// GC may have mapped its operation ids while the batch waited unlocked.
fn refresh(state: &AppState, repo: RepoId, member: &mut Member) -> Result<(), LandingError> {
    if let Some(fresh) = state.db.landing(repo, &member.landing.row.id)? {
        member.landing.row = fresh.row;
    }
    Ok(())
}

/// Returns member `index` to proposed with `reason`, recording the tested
/// evaluation and tree when there are.
async fn dequeue_member(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    index: usize,
    reason: Reason,
    message: &str,
    (evaluation, trial): (Option<&Evaluation>, Option<&BatchTrialRow>),
) -> Result<(), LandingError> {
    refresh(state, repo, &mut batch.members[index])?;
    let member = &mut batch.members[index];
    if let Some(change) = member.landing.changes.first_mut() {
        change.tested_tree = trial.map(|trial| trial.tree.clone());
        change.tested_commit = trial.and_then(|trial| trial.commit_id.clone());
        change.tested_evaluation =
            evaluation.map(|evaluation| serde_json::to_string(evaluation).unwrap_or_default());
    }
    let who = member.who.clone();
    let target = member.target.clone();
    dequeue(
        state,
        repo,
        &mut member.landing,
        &target,
        &who,
        reason,
        message,
    )
    .await
}

/// [`land_prefix_part`], then the batch finished: `landed` when every
/// member landed.
async fn land_prefix(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    p: usize,
    tested: Option<Vec<Evaluation>>,
    pace: &mut Pace,
) -> Result<Next, LandingError> {
    if let Some(next) = land_prefix_part(state, repo, batch, p, tested, pace).await? {
        return Ok(next);
    }
    end(state, batch, "landed", "")?;
    pace.moves = 0;
    Ok(Next::Again)
}

/// Lands members 1 to `p` of `batch` in one operation, each judged again on
/// its submitted tree, with `tested` their evaluations over the tested
/// tree (the last trial's); `Some` when the batch ended otherwise
/// (re-formed, or members dequeued).
#[expect(clippy::too_many_lines, reason = "a landing's steps in order")]
async fn land_prefix_part(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    p: usize,
    tested: Option<Vec<Evaluation>>,
    pace: &mut Pace,
) -> Result<Option<Next>, LandingError> {
    let trial = tested.as_ref().and_then(|_| {
        batch
            .stored
            .trials
            .iter()
            .rev()
            .find(|trial| trial.prefix as usize == p && trial.result.as_deref() == Some("passed"))
            .cloned()
    });
    // Held to the end, as in R3.13: GC can neither delete the evidence
    // judged nor rewrite the operation before the rows are committed.
    let shared = Arc::new(state.manager.repo_lock(repo).read_owned().await);
    // Read again under the lock: GC may have mapped their operation ids
    // while the batch waited.
    for member in &mut batch.members[..p] {
        refresh(state, repo, member)?;
    }
    let mut judged_all: Vec<Box<Judged>> = Vec::with_capacity(p);
    for index in 0..p {
        let member = &batch.members[index];
        let context = JudgeContext {
            landing: Some(member.input.clone()),
            batch: batch.input(index, trial.as_ref()),
            evidence_tree: None,
        };
        match judge_with(state, repo, &member.target, &context).await? {
            Verdict::Allow(judged) if judged.preview.destination.as_str() == batch.base() => {
                judged_all.push(judged);
            }
            Verdict::Allow(_)
            | Verdict::Block {
                reason: Reason::Busy,
                ..
            } => {
                drop(shared);
                return reform(state, repo, batch, &Reform::Moved, pace)
                    .await
                    .map(Some);
            }
            Verdict::Block {
                reason,
                message,
                preview,
                judged,
                destination,
                ..
            } => {
                let member = &mut batch.members[index];
                member.landing.row.destination_before = destination;
                let mut change = change_row(
                    &member.target,
                    member.queued_tree.clone(),
                    preview.as_ref(),
                    judged.as_deref(),
                );
                change.batch_position = Some(u32::try_from(index + 1).unwrap_or(u32::MAX));
                member.landing.changes = vec![change];
                if let Some(judged) = &judged {
                    record_policy(&mut member.landing, judged);
                }
                dequeue_member(state, repo, batch, index, reason, &message, (None, None)).await?;
                batch.member_result(index, "dequeued");
                drop(shared);
                return reform(state, repo, batch, &Reform::Left, pace)
                    .await
                    .map(Some);
            }
        }
    }

    // Land, holding the lifecycle lock from the checks to the record.
    let lifecycle_lock = state.lifecycle.lock(repo);
    let held = lifecycle_lock.lock().await;
    for index in 0..p {
        let member = &mut batch.members[index];
        if let Some(done) = settled(state, repo, &mut member.landing, &member.target)? {
            done?;
            batch.member_result(index, "left");
            drop(held);
            drop(shared);
            return reform(state, repo, batch, &Reform::Left, pace)
                .await
                .map(Some);
        }
    }
    let drop_pending = |state: &AppState, rows: &[TransitionRow]| {
        for row in rows {
            let _ = state.db.delete_pending_transition(repo, &row.id);
        }
    };
    let mut rows: Vec<TransitionRow> = Vec::with_capacity(p);
    let mut records = Vec::with_capacity(p);
    let mut inserted: Result<(), LandingError> = Ok(());
    for member in &batch.members[..p] {
        let row = match state.db.lifecycle_row(repo, member.target.change.as_str()) {
            Ok(Some(row)) => row,
            Ok(None) => {
                inserted = Err(LandingError::Input("a queued change has no row".to_owned()));
                break;
            }
            Err(error) => {
                inserted = Err(error.into());
                break;
            }
        };
        let transition = super::transition_row(
            repo,
            member.target.change.as_str(),
            member.target.commit.as_str(),
            LifecycleState::Queued,
            LifecycleState::Landed,
            row.round,
            &member.target.bookmark,
            None,
            &member.who,
            Actor::Daemon,
        );
        if let Err(error) = state.db.insert_pending_transition(&transition) {
            inserted = Err(error.into());
            break;
        }
        records.push(TransitionRecord {
            id: transition.id.clone(),
            change: transition.change_id.clone(),
            commit: String::new(),
            from: LifecycleState::Queued,
            to: LifecycleState::Landed,
            cause: TransitionCause::Landing,
            round: row.round,
            reason: String::new(),
            superseded_by: String::new(),
            destination: member.target.bookmark.clone(),
            undoes: String::new(),
        });
        rows.push(transition);
    }
    let base = commit_hex(batch.base());
    let (Ok(()), Some(base)) = (inserted, base) else {
        drop_pending(state, &rows);
        return Err(LandingError::Input(
            "cannot record the landing's transitions".to_owned(),
        ));
    };
    let mut trials = batch.trial_commits();
    if let Ok(more) = state.db.interrupted_trial_commits(repo) {
        trials.extend(more.iter().filter_map(|commit| commit_hex(commit)));
    }
    trials.sort();
    trials.dedup();
    let plan = LandPlan {
        bookmark: batch.destination().to_owned(),
        destination: base,
        members: batch.members[..p]
            .iter()
            .map(|member| PlannedMember {
                target: member.target.clone(),
                tree: Some(member.planned.clone()),
            })
            .collect(),
        trials,
    };
    let written = state
        .manager
        .rewrite_held(
            WriteRequest {
                repo,
                workspace: None,
                agent: batch.members[0].who.marked.clone(),
                write: Write::Land {
                    plan,
                    landing: (p == 1).then(|| batch.members[0].landing.row.id.clone()),
                    batch: Some(batch.id().to_owned()),
                    transitions: records,
                },
            },
            Arc::clone(&shared),
        )
        .await;
    let landed: Vec<(String, Option<String>)> = match written {
        Ok(written) => {
            let Some(op_id) = written.op_id.clone() else {
                drop_pending(state, &rows);
                return Err(LandingError::Input("the landing wrote nothing".to_owned()));
            };
            batch.stored.row.operation_id = Some(op_id);
            written
                .landed
                .iter()
                .map(|member| (member.commit.as_str().to_owned(), member.tree.clone()))
                .collect()
        }
        Err(error) if !refused_before_writing(&error) => {
            // The operation may have been written before the failure: look
            // for it, as start-up does.
            let mut found = Vec::with_capacity(p);
            for row in &rows {
                match find_written(state, repo, row).await {
                    Some(written) => found.push(written),
                    None => break,
                }
            }
            if found.len() != p {
                drop_pending(state, &rows);
                let message = ErrorChain(&error).to_string();
                for index in 0..p {
                    let member = &mut batch.members[index];
                    let target = member.target.clone();
                    let who = member.who.clone();
                    dequeue_locked(
                        state,
                        repo,
                        &mut member.landing,
                        &target,
                        &who,
                        Reason::Failed,
                        &message,
                    )
                    .await?;
                    batch.member_result(index, "dequeued");
                }
                drop(held);
                drop(shared);
                end_trials(state, repo, batch).await;
                end(state, batch, "reformed", &message)?;
                return Ok(Some(Next::Again));
            }
            batch.stored.row.operation_id = found.first().map(|(op, _)| op.clone());
            found
                .into_iter()
                .map(|(_, commit)| (commit, None))
                .collect()
        }
        Err(error) => {
            drop_pending(state, &rows);
            drop(held);
            let message = ErrorChain(&error).to_string();
            if matches!(
                error,
                ManagerError::Land(LandError::DestinationMoved { .. })
            ) {
                drop(shared);
                return reform(state, repo, batch, &Reform::Moved, pace)
                    .await
                    .map(Some);
            }
            return refused(state, repo, batch, p, &error, &message, shared, pace)
                .await
                .map(Some);
        }
    };
    let new_head = landed.last().map(|(commit, _)| commit.clone());
    let op_id = batch.stored.row.operation_id.clone().unwrap_or_default();
    let recorded_at = operation_end(state, repo, &op_id).await;
    for (index, ((commit, tree), mut row)) in landed.into_iter().zip(rows).enumerate() {
        // The operation is written: a failure to record one change does not
        // stop the others. Start-up commits a pending row from the operation
        // and finishes its landing from the row (`settled`).
        let committed = state
            .db
            .set_pending_transition_commit(repo, &row.id, &commit)
            .map_err(LandingError::from)
            .and_then(|()| {
                row.commit_id.clone_from(&commit);
                crate::lifecycle::commit_written_transition(state, repo, &row, &op_id, recorded_at)
                    .map_err(LandingError::from)
            });
        if let Err(error) = committed {
            tracing::warn!(repo = %repo, change_id = %row.change_id, error = %ErrorChain(&error), "cannot record a landed change; the next start does");
            continue;
        }
        let judged = &judged_all[index];
        let tested_evaluation = tested.as_ref().and_then(|tested| tested.get(index));
        let member = &mut batch.members[index];
        let mut change = change_row(
            &member.target,
            member.queued_tree.clone(),
            None,
            Some(&**judged),
        );
        change.landed_commit = Some(commit);
        change.landed_tree = tree.or_else(|| Some(member.planned.clone()));
        change.rebased = member.input.rebased;
        change.overlap = Some(
            serde_json::json!({ "paths": member.input.overlap, "total": member.input.overlap_total })
                .to_string(),
        );
        change.batch_position = Some(u32::try_from(index + 1).unwrap_or(u32::MAX));
        change.tested_tree = trial.as_ref().map(|trial| trial.tree.clone());
        change.tested_commit = trial.as_ref().and_then(|trial| trial.commit_id.clone());
        change.tested_evaluation = tested_evaluation
            .map(|evaluation| serde_json::to_string(evaluation).unwrap_or_default());
        let landing = &mut member.landing;
        landing.changes = vec![change];
        record_policy(landing, judged);
        landing.row.destination_after.clone_from(&new_head);
        landing.row.operation_id = Some(op_id.clone());
        Outcome::Landed
            .as_str()
            .clone_into(&mut landing.row.outcome);
        landing.row.reason = None;
        landing.row.finished_at = Some(Timestamp::now());
        let mut ids = pins(&judged.evaluation);
        if let Some(tested) = tested_evaluation {
            ids.extend(tested_pins(tested));
        }
        ids.sort();
        ids.dedup();
        let pinned: Vec<(String, String)> = ids
            .into_iter()
            .map(|id| (member.target.change.as_str().to_owned(), id))
            .collect();
        let policy = judged
            .policy
            .sha256
            .as_deref()
            .zip(judged.policy.bytes.as_deref());
        if let Err(error) = state
            .db
            .finish_landing(&landing.row, &landing.changes, &pinned, policy)
        {
            tracing::warn!(repo = %repo, landing_id = %landing.row.id, error = %ErrorChain(&error), "cannot record a landing; the next start finishes it");
        }
        state.landings.notify_finished();
        tracing::info!(change_id = %member.target.change, landing_id = %landing.row.id, op_id = %op_id, "landed");
        batch.member_result(index, "landed");
    }
    drop(held);
    drop(shared);
    batch.stored.row.landed_count = u32::try_from(p).unwrap_or(u32::MAX);
    batch.save(state)?;
    Ok(None)
}

/// A landing of members 1 to `p` that was refused before its operation was
/// written. The member the error names is dequeued with R3.13's reason and
/// the rest re-form; one member alone is dequeued so; otherwise the members
/// land in batches of their own next (`Pace::solo`), which finds the one
/// that fails, rather than trying the same batch again.
#[expect(clippy::too_many_arguments, reason = "the landing's state, passed on")]
async fn refused(
    state: &Arc<AppState>,
    repo: RepoId,
    batch: &mut Formed,
    p: usize,
    error: &ManagerError,
    message: &str,
    shared: Arc<tokio::sync::OwnedRwLockReadGuard<()>>,
    pace: &mut Pace,
) -> Result<Next, LandingError> {
    let by_change = |change: &reef_lib::jj::ChangeIdHex| {
        batch.members[..p]
            .iter()
            .position(|member| member.target.change == *change)
    };
    let by_commit = |commit: &CommitIdHex| {
        batch.members[..p]
            .iter()
            .position(|member| member.target.commit == *commit)
    };
    let named = match error {
        ManagerError::Land(
            LandError::Amended { change, .. } | LandError::PlanMismatch { change, .. },
        ) => by_change(change),
        ManagerError::Land(
            LandError::UnlandedParents { commit, .. }
            | LandError::Conflicted(commit)
            | LandError::AlreadyOnDestination(commit),
        ) => by_commit(commit),
        _ => None,
    };
    let reason = match error {
        ManagerError::Land(LandError::Amended { .. })
        | ManagerError::Rewrite(reef_lib::jj::rewrite::RewriteError::Hidden(_)) => Reason::Amended,
        ManagerError::Land(LandError::RebaseConflicts | LandError::Conflicted(_)) => {
            Reason::Conflicted
        }
        ManagerError::Land(LandError::UnlandedParents { .. }) => Reason::UnlandedParents,
        ManagerError::Land(
            LandError::DestinationNotFound(_)
            | LandError::DestinationConflicted(_)
            | LandError::AlreadyOnDestination(_),
        ) => Reason::Destination,
        _ => Reason::Failed,
    };
    if let Some(index) = named.or_else(|| (p == 1).then_some(0)) {
        dequeue_member(state, repo, batch, index, reason, message, (None, None)).await?;
        batch.member_result(index, "dequeued");
    } else {
        for member in &batch.members[..p] {
            pace.solo.insert(member.landing.row.id.clone());
        }
    }
    drop(shared);
    reform(state, repo, batch, &Reform::Left, pace).await
}

/// At start: finishes every batch an earlier daemon left open
/// `interrupted`; the worker forms the next from the queue, reusing their
/// trial commits, open jobs and evidence.
pub(crate) fn interrupt_open(state: &AppState) {
    let open = match state.db.open_batches() {
        Ok(open) => open,
        Err(error) => {
            tracing::warn!(error = %ErrorChain(&error), "cannot read open batches");
            return;
        }
    };
    for (repo, id) in open {
        let Ok(Some(mut batch)) = state.db.batch(repo, &id) else {
            continue;
        };
        let now = Timestamp::now();
        "finished".clone_into(&mut batch.row.state);
        batch.row.outcome = Some("interrupted".to_owned());
        batch.row.message = Some("the daemon stopped".to_owned());
        batch.row.finished_at = Some(now);
        for trial in &mut batch.trials {
            if trial.result.is_none() {
                trial.result = Some("cancelled".to_owned());
                trial.finished_at = Some(now);
            }
        }
        // A crash inside the land operation: the members it landed are
        // `landed` in their lifecycle rows once start-up resolved them.
        let mut landed = 0;
        for member in &mut batch.members {
            let row = state
                .db
                .lifecycle_row(repo, &member.change_id)
                .ok()
                .flatten();
            if member.result.is_none()
                && row.is_some_and(|row| row.state == LifecycleState::Landed.as_str())
            {
                member.result = Some("landed".to_owned());
            }
            if member.result.as_deref() == Some("landed") {
                landed += 1;
            }
        }
        batch.row.landed_count = landed;
        if let Err(error) = state.db.write_batch(&batch) {
            tracing::warn!(repo = %repo, batch_id = %id, error = %ErrorChain(&error), "cannot finish an interrupted batch");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use reef_policy::RuleResult;

    fn require(status: Status, missing: &[(&str, &str)], failed: &[(&str, &str)]) -> RuleResult {
        let need = |(kind, target): &(&str, &str)| Need {
            kind: (*kind).to_owned(),
            target: (*target).to_owned(),
            ..Need::default()
        };
        RuleResult {
            name: "tests".to_owned(),
            kind: RuleKind::Require,
            status,
            message: String::new(),
            paths: Vec::new(),
            paths_total: 0,
            missing: missing.iter().map(need).collect(),
            failed: failed.iter().map(need).collect(),
            signoff: None,
            evidence_ids: Vec::new(),
        }
    }

    fn evaluation(rules: Vec<RuleResult>) -> Evaluation {
        let mut evaluation = Evaluation::no_policy(&reef_policy::Input::default());
        evaluation.rules = rules;
        evaluation
    }

    #[test]
    fn only_require_rules_count() {
        let mut signoff = require(Status::Unsatisfied, &[], &[]);
        signoff.kind = RuleKind::Signoff;
        assert!(matches!(
            checked(&evaluation(vec![
                signoff,
                require(Status::Satisfied, &[], &[])
            ])),
            Checked::Passed
        ));
        assert!(matches!(
            checked(&evaluation(vec![require(Status::Unsatisfied, &[("test", "")], &[])])),
            Checked::Missing(needs) if needs.len() == 1
        ));
        // A failure wins over anything missing.
        assert!(matches!(
            checked(&evaluation(vec![
                require(Status::Unsatisfied, &[("lint", "")], &[]),
                require(Status::Unsatisfied, &[], &[("test", "a")]),
            ])),
            Checked::Failed(needs) if needs[0].target == "a"
        ));
        assert!(matches!(
            checked(&evaluation(vec![require(Status::Error, &[], &[])])),
            Checked::Failed(_)
        ));
    }

    #[test]
    fn needs_are_capped_with_their_total() {
        let needs: Vec<Need> = (0..30)
            .map(|i| Need {
                kind: format!("k{i}"),
                ..Need::default()
            })
            .collect();
        let json: serde_json::Value = serde_json::from_str(&needs_json(&needs)).unwrap();
        assert_eq!(json["needs"].as_array().unwrap().len(), MAX_NEEDS);
        assert_eq!(json["total"], 30);
    }
}
