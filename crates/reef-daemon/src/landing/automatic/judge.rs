//! One stack's judgment by the automatic service, and what follows from it
//! (R3.45, ADR 0081): each change's status, the evidence asked of
//! executors for the pairs it lacks, the queue when the destination lands
//! automatically, and the rule for a landing that came back.

use std::collections::{BTreeSet, HashSet};
use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};

use reef_lib::error::ErrorChain;
use reef_lib::jj::land::StackMember;
use reef_policy::{Evaluation, Need, RuleKind, Status as RuleStatus};

use super::{
    BUSY_AFTER, MAX_BUSY, MAX_INFRASTRUCTURE, MAX_OPEN_JOBS, PROPOSAL_PRIORITY, RETRY, Settings,
    State, Status, change_revision, short,
};
use crate::daemon::AppState;
use crate::db::{JobRow, JobState, LifecycleRow};
use crate::error::Advise as _;
use crate::landing::batch::{AGENT, Ask, job_groups, submit_needs};
use crate::landing::{
    Decided, Gate, JudgeContext, Judged, LandRequest, LandingError, Outcome, Reason,
    Verdict as LandVerdict, decide, judge_with, policy_message,
};
use crate::lifecycle::Actor;
use crate::model::{RepoId, Timestamp};
use reef_lib::jj::land::LandTarget;

/// The automatic status's reason while a help request holds it (R3.32).
pub(crate) const HELP_REASON: &str = "help";

/// The open jobs the service may still ask for in a repository.
pub(super) struct Budget {
    open: usize,
}

impl Budget {
    pub(super) fn new(state: &AppState, repo: RepoId, proposals: &[LifecycleRow]) -> Budget {
        let commits: HashSet<&str> = proposals.iter().map(|row| row.commit_id.as_str()).collect();
        let open = state
            .db
            .open_jobs()
            .unwrap_or_default()
            .into_iter()
            .filter(|job| {
                job.repo_id == repo
                    && job.requested_by_agent.as_deref() == Some(AGENT)
                    && job
                        .commit_id
                        .as_deref()
                        .is_some_and(|commit| commits.contains(commit))
            })
            .count();
        Budget { open }
    }
}

/// One stack's judgment and what follows from it.
pub(super) struct Judging<'a> {
    pub(super) state: &'a Arc<AppState>,
    pub(super) repo: RepoId,
    pub(super) destination: &'a str,
    pub(super) settings: Settings,
    pub(super) key: &'a str,
    pub(super) claimed: Option<Timestamp>,
}

/// What one member's verdict asks of the service.
enum Verdict {
    Passes,
    Queued(String),
    Blocked(Box<Status>),
    /// It lacks `[[require]]` pairs only, which may be asked for.
    Lacks(Vec<Need>),
}

impl Judging<'_> {
    fn request(&self, head: &str) -> Result<LandRequest, String> {
        let record = self
            .state
            .db
            .repository(self.repo)
            .map_err(|error| ErrorChain(&error).to_string())?;
        Ok(LandRequest {
            repo: self.repo,
            workspace: None,
            user_email: self.state.settings.user_email().to_owned(),
            root: record.root,
            revision: change_revision(head),
            expected_commit: String::new(),
            wait: Duration::ZERO,
            actor: Some(Actor::Daemon),
            agent: Some(AGENT.to_owned()),
            remote: None,
            automatic_key: Some(self.key.to_owned()),
            needs_touch: false,
        })
    }

    #[expect(
        clippy::too_many_lines,
        reason = "each member's status, then the queue"
    )]
    pub(super) async fn judge(
        &mut self,
        head: &str,
        members: &[StackMember],
        budget: &mut Budget,
    ) -> Vec<(String, Status)> {
        self.state
            .automatic
            .evaluations
            .fetch_add(1, Ordering::Relaxed);
        let head_commit = members
            .last()
            .map(|member| member.commit.as_str().to_owned())
            .unwrap_or_default();
        if let Some(held) = self.held_for_help(members) {
            return held;
        }
        let request = match self.request(head) {
            Ok(request) => request,
            Err(error) => {
                let mut status = Status::new(State::Needs, &head_commit);
                status.message = error;
                return vec![(head.to_owned(), self.keyed(status, false))];
            }
        };
        let judgment = match decide(self.state, &request).await {
            Ok(Gate::Queued(outcome)) => {
                let mut out = Vec::new();
                for landing in std::iter::once(&outcome.landing).chain(&outcome.stack) {
                    for change in &landing.changes {
                        let mut status = Status::new(State::Queued, &change.queued_commit);
                        status.landing_id = Some(landing.row.id.clone());
                        status.message = format!("queued for {}", self.destination);
                        status.next = vec![
                            "reef".into(),
                            "landing".into(),
                            "show".into(),
                            landing.row.id.clone(),
                        ];
                        out.push((change.change_id.clone(), self.keyed(status, false)));
                    }
                }
                return out;
            }
            Ok(Gate::Judged(judgment)) => judgment,
            Err(error) => {
                return Box::pin(self.refused(head, &head_commit, members, &error, budget)).await;
            }
        };
        let mut verdicts: Vec<(String, String, Option<String>, Verdict)> = Vec::new();
        let mut targets: Vec<Option<LandTarget>> = Vec::new();
        let mut sensitive = false;
        for decision in &judgment.decided {
            match decision {
                Decided::Open(open) => {
                    for change in &open.changes {
                        targets.push(None);
                        verdicts.push((
                            change.change_id.clone(),
                            change.queued_commit.clone(),
                            change.queued_tree.clone(),
                            Verdict::Queued(open.row.id.clone()),
                        ));
                    }
                }
                Decided::Pass { target, judged, .. } => {
                    targets.push(Some(target.clone()));
                    verdicts.push((
                        target.change.as_str().to_owned(),
                        target.commit.as_str().to_owned(),
                        judged.tree.clone(),
                        Verdict::Passes,
                    ));
                }
                Decided::Block {
                    target,
                    row,
                    judged,
                } => {
                    let (row, changes) = &**row;
                    let reason = row.reason.clone().unwrap_or_default();
                    let tree = changes
                        .first()
                        .and_then(|change| change.queued_tree.clone());
                    let commit = target.commit.as_str().to_owned();
                    let verdict = self.blocked(
                        &reason,
                        row.message.clone().unwrap_or_default(),
                        judged.as_deref(),
                        &commit,
                    );
                    if matches!(reason.as_str(), "conflicted" | "empty" | "destination") {
                        sensitive = true;
                    }
                    targets.push(Some(target.clone()));
                    verdicts.push((target.change.as_str().to_owned(), commit, tree, verdict));
                }
            }
        }
        // Ask for what is missing, then turn each verdict into a status.
        let mut out: Vec<(String, Status)> = Vec::with_capacity(verdicts.len());
        let mut first_open: Option<usize> = None;
        for (index, (change, commit, tree, verdict)) in verdicts.into_iter().enumerate() {
            let mut status = match verdict {
                Verdict::Passes => {
                    let mut status = Status::new(State::Ready, &commit);
                    status.message = format!("the gate allows it onto {}", self.destination);
                    status.next = vec!["reef".into(), "land".into(), change_revision(&change)];
                    status
                }
                Verdict::Queued(landing) => {
                    let mut status = Status::new(State::Queued, &commit);
                    status.message = format!("queued for {}", self.destination);
                    status.next = vec![
                        "reef".into(),
                        "landing".into(),
                        "show".into(),
                        landing.clone(),
                    ];
                    status.landing_id = Some(landing);
                    status
                }
                Verdict::Blocked(status) => *status,
                Verdict::Lacks(needs) => {
                    self.lacks(&change, &commit, tree.as_deref(), needs, budget)
                }
            };
            status.tree = tree;
            if first_open.is_none() && !matches!(status.state, State::Ready | State::Queued) {
                first_open = Some(index);
            }
            out.push((change, status));
        }
        // Members stopped by one below take its wait, or keep the gate's
        // reason.
        if let Some(stop) = first_open {
            let below = out[stop].clone();
            let waits = matches!(below.1.state, State::Waiting | State::NoExecutor);
            for index in stop + 1..out.len() {
                if !waits
                    || out[index].1.reason.as_deref() != Some(Reason::UnlandedParents.as_str())
                {
                    continue;
                }
                // Its own checks are asked for now too, so the stack's
                // checks run side by side rather than one change at a time.
                let own = match targets.get(index).cloned().flatten() {
                    Some(target) => {
                        let below: Vec<LandTarget> =
                            targets[..index].iter().flatten().cloned().collect();
                        self.own_checks(&target, below, budget).await
                    }
                    None => Vec::new(),
                };
                let status = &mut out[index].1;
                status.state = below.1.state;
                status.message = format!("waits for {} below it", short(&below.0));
                status.jobs = own;
            }
        }
        let queueable = match first_open {
            None => true,
            Some(stop) => {
                !matches!(
                    out[stop].1.state,
                    State::Waiting | State::Pending | State::NoExecutor
                ) && out[..stop]
                    .iter()
                    .any(|(_, status)| status.state == State::Ready)
            }
        };
        let prefix = first_open.unwrap_or(out.len());
        if self.settings.automatic && queueable && prefix > 0 {
            if let Some(returned) = self.returned(&out[..prefix]) {
                for (change, status) in &mut out {
                    if let Some(found) = returned.iter().find(|(name, _)| name == change) {
                        *status = found.1.clone();
                    }
                }
            } else {
                match crate::landing::write(self.state, request, *judgment).await {
                    Ok(outcome) => {
                        crate::landing::worker::wake(self.state, self.repo);
                        for landing in std::iter::once(&outcome.landing).chain(&outcome.stack) {
                            if landing.row.outcome != Outcome::Queued.as_str() {
                                continue;
                            }
                            for change in &landing.changes {
                                if let Some((_, status)) =
                                    out.iter_mut().find(|(name, _)| *name == change.change_id)
                                {
                                    status.state = State::Queued;
                                    status.message =
                                        format!("queued automatically for {}", self.destination);
                                    status.landing_id = Some(landing.row.id.clone());
                                    status.next = vec![
                                        "reef".into(),
                                        "landing".into(),
                                        "show".into(),
                                        landing.row.id.clone(),
                                    ];
                                }
                            }
                        }
                        tracing::info!(repo = %self.repo, change_id = head, landing_id = %outcome.landing.row.id, "queued automatically");
                    }
                    Err(error) => {
                        tracing::warn!(repo = %self.repo, change_id = head, error = %ErrorChain(&error), "cannot queue automatically");
                        let advice = error.advice().unwrap_or_default();
                        // An unset identity is the operator's to fix (R3.17);
                        // anything else is tried again shortly.
                        let lasting = matches!(error, LandingError::IdentityUnset { .. });
                        for (_, status) in out.iter_mut().take(prefix) {
                            if status.state != State::Ready {
                                continue;
                            }
                            status.state = State::Needs;
                            status.message = format!("could not queue it: {error}");
                            status.reason = Some(Reason::Failed.as_str().to_owned());
                            status.next.clone_from(&advice.next);
                            if !lasting {
                                status.retry_at = Some(Instant::now() + RETRY);
                            }
                        }
                    }
                }
            }
        }
        out.into_iter()
            .map(|(change, status)| (change, self.keyed(status, sensitive)))
            .collect()
    }

    /// A stack with a member whose change has an open help request (R3.32)
    /// is held whole: its author asked a person, so nothing more is queued
    /// before the answer. `None` when no member has one, when a member is
    /// already queued (its landing goes on, and its status says queued; a
    /// person withdraws the proposal to stop it), or when the rows cannot be
    /// read (logged; the gate judges as before).
    fn held_for_help(&self, members: &[StackMember]) -> Option<Vec<(String, Status)>> {
        let queued = members.iter().any(|member| {
            self.state
                .db
                .lifecycle_row(self.repo, member.change.as_str())
                .ok()
                .flatten()
                .is_some_and(|row| row.state == "queued")
        });
        if queued {
            return None;
        }
        let asked = members.iter().find_map(|member| {
            match self
                .state
                .db
                .open_help_request(self.repo, member.change.as_str())
            {
                Ok(found) => found.map(|row| (member.change.as_str().to_owned(), row.id)),
                Err(error) => {
                    tracing::warn!(repo = %self.repo, error = %ErrorChain(&error), "cannot read help requests; judging without them");
                    None
                }
            }
        })?;
        let (change, id) = asked;
        Some(
            members
                .iter()
                .map(|member| {
                    let mut status = Status::new(State::Needs, member.commit.as_str());
                    status.reason = Some(HELP_REASON.to_owned());
                    status.message = if member.change.as_str() == change {
                        format!("its author asked for help ({id}); it waits for the answer")
                    } else {
                        format!(
                            "change {} in its stack asked for help ({id}); it waits for the answer",
                            short(&change)
                        )
                    };
                    status.next = vec!["reef".into(), "comment".into(), "show".into(), id.clone()];
                    (member.change.as_str().to_owned(), self.keyed(status, false))
                })
                .collect(),
        )
    }

    fn keyed(&self, mut status: Status, sensitive: bool) -> Status {
        self.key.clone_into(&mut status.key);
        status.sensitive = sensitive;
        status.evaluated_at = Timestamp::now();
        status
    }

    /// The status of a judgment that stopped with an error. A change of the
    /// stack the error names (amended, not proposed, proposed elsewhere)
    /// and the changes above it need what the error says; the changes below
    /// it are judged as their own stack, so a problem at the top never
    /// holds back the bottom. Any other error is tried again shortly.
    async fn refused(
        &mut self,
        head: &str,
        head_commit: &str,
        members: &[StackMember],
        error: &LandingError,
        budget: &mut Budget,
    ) -> Vec<(String, Status)> {
        let (named, reason) = match error {
            LandingError::Amended { change, .. } => (Some(change.clone()), "amended"),
            LandingError::NotProposed { change, .. } => (Some(change.clone()), "not_proposed"),
            LandingError::DependencyLeftReview { change, .. } => {
                (Some(change.clone()), Reason::Dependency.as_str())
            }
            LandingError::StackDestinationMismatch { change, .. } => {
                (Some(change.clone()), "destination")
            }
            _ => (None, "failed"),
        };
        let advice = error.advice().unwrap_or_default();
        let at = named
            .as_deref()
            .and_then(|named| members.iter().position(|m| m.change.as_str() == named))
            .unwrap_or(members.len().saturating_sub(1));
        let mut out = Vec::new();
        for (index, member) in members.iter().enumerate().skip(at) {
            let change = member.change.as_str();
            let commit = if change == head {
                head_commit.to_owned()
            } else {
                member.commit.as_str().to_owned()
            };
            let mut status = Status::new(State::Needs, &commit);
            status.tree.clone_from(&member.tree);
            if index == at {
                status.message = error.to_string();
                status.reason = Some(reason.to_owned());
                status.next.clone_from(&advice.next);
                if named.is_none() {
                    status.retry_at = Some(Instant::now() + RETRY);
                }
            } else {
                status.message =
                    format!("{} below it: {error}", short(members[at].change.as_str()));
                status.reason = Some(Reason::UnlandedParents.as_str().to_owned());
            }
            out.push((change.to_owned(), self.keyed(status, false)));
        }
        if out.is_empty() {
            let mut status = Status::new(State::Needs, head_commit);
            status.message = error.to_string();
            status.reason = Some(reason.to_owned());
            status.next = advice.next;
            status.retry_at = Some(Instant::now() + RETRY);
            out.push((head.to_owned(), self.keyed(status, false)));
        }
        if named.is_some() && at > 0 {
            let below = &members[..at];
            let new_head = below[at - 1].change.as_str().to_owned();
            out.extend(Box::pin(self.judge(&new_head, below, budget)).await);
        }
        out
    }

    /// Asks for the `[[require]]` pairs `target` lacks on its own tree when
    /// a change below it in its stack waits for checks (judged as the gate
    /// would, with `below` landing first). Returns the jobs it waits on.
    async fn own_checks(
        &self,
        target: &LandTarget,
        below: Vec<LandTarget>,
        budget: &mut Budget,
    ) -> Vec<String> {
        let context = JudgeContext {
            below,
            ..JudgeContext::default()
        };
        let Ok(LandVerdict::Block {
            reason: Reason::Policy,
            judged: Some(judged),
            ..
        }) = judge_with(self.state, self.repo, target, &context).await
        else {
            return Vec::new();
        };
        let found = requirements(&judged.evaluation);
        if !found.failed.is_empty() || found.missing.is_empty() {
            return Vec::new();
        }
        self.lacks(
            target.change.as_str(),
            target.commit.as_str(),
            judged.tree.as_deref(),
            found.missing,
            budget,
        )
        .jobs
    }

    /// What a member the gate blocked for `reason` asks of the service.
    fn blocked(
        &self,
        reason: &str,
        message: String,
        judged: Option<&Judged>,
        commit: &str,
    ) -> Verdict {
        let mut status = Status::new(State::Needs, commit);
        status.reason = Some(reason.to_owned());
        status.message = message;
        if reason == Reason::Policy.as_str()
            && let Some(judged) = judged
        {
            let found = requirements(&judged.evaluation);
            if found.failed.is_empty() && !found.other && !found.missing.is_empty() {
                return Verdict::Lacks(found.missing);
            }
            status.message = policy_message(&judged.evaluation);
            status.next = vec![
                "reef".into(),
                "policy".into(),
                "eval".into(),
                "--stack".into(),
            ];
            let mut needs = found.failed;
            needs.extend(found.missing);
            needs.extend(found.signoffs);
            needs.extend(found.rules);
            return Verdict::Blocked(Box::new(status.with_missing(needs)));
        }
        status.next = match reason {
            "conflicted" => vec![
                "reef".into(),
                "rebase".into(),
                "--onto".into(),
                self.destination.to_owned(),
            ],
            "empty" => vec!["reef".into(), "proposal".into(), "withdraw".into()],
            _ => Vec::new(),
        };
        Verdict::Blocked(Box::new(status))
    }

    /// The status of a member that lacks `[[require]]` pairs only: asked
    /// for, unless the destination does not ask, no executor answers, the
    /// repository has its share open, or the pair failed twice for want of
    /// a verdict.
    #[expect(
        clippy::too_many_lines,
        reason = "each way a lacking pair can go, in order"
    )]
    fn lacks(
        &self,
        change: &str,
        commit: &str,
        tree: Option<&str>,
        needs: Vec<Need>,
        budget: &mut Budget,
    ) -> Status {
        let mut status = Status::new(State::Waiting, commit);
        status.reason = Some(Reason::Policy.as_str().to_owned());
        status.next = vec!["reef".into(), "land".into(), change_revision(change)];
        let kinds: BTreeSet<&str> = needs.iter().map(|need| need.kind.as_str()).collect();
        let kinds = kinds.into_iter().collect::<Vec<_>>().join(", ");
        let Some(tree) = tree.filter(|_| self.settings.requests) else {
            status.state = State::Needs;
            status.message = format!(
                "it lacks evidence ({kinds}) that this destination does not ask executors for"
            );
            status.next = vec![
                "reef".into(),
                "policy".into(),
                "eval".into(),
                change_revision(change),
            ];
            return status.with_missing(needs);
        };
        let jobs = self
            .state
            .db
            .jobs_on_tree(self.repo, tree)
            .unwrap_or_default();
        let mut open = Vec::new();
        let mut submit = Vec::new();
        let mut failed: Vec<String> = Vec::new();
        for (kind, targets) in job_groups(&needs) {
            let ours: Vec<&JobRow> = jobs
                .iter()
                .filter(|job| job.kinds == [kind.clone()] && job.targets == targets)
                .collect();
            if let Some(job) = ours.iter().find(|job| job.state.is_open()) {
                open.push(job.id.clone());
                continue;
            }
            let ended: Vec<&&JobRow> = ours
                .iter()
                .filter(|job| no_verdict(job.state, job.end_reason.as_deref()))
                .collect();
            if ended.len() >= MAX_INFRASTRUCTURE {
                failed.extend(
                    ended
                        .iter()
                        .take(MAX_INFRASTRUCTURE)
                        .map(|job| job.id.clone()),
                );
                continue;
            }
            submit.push(kind);
        }
        if !failed.is_empty() {
            status.state = State::Needs;
            status.message = format!(
                "{} ended twice with no verdict on this tree: run it by hand with `reef job submit`",
                failed.join(", ")
            );
            status.jobs = failed;
            status.next = vec![
                "reef".into(),
                "job".into(),
                "show".into(),
                status.jobs[0].clone(),
            ];
            return status.with_missing(needs);
        }
        // A kind no executor answers is asked for once per repository at a
        // time: the open job is the one a newly started executor claims,
        // and its claim lets every other request through.
        let mut unserved: Vec<String> = Vec::new();
        submit.retain(|kind| {
            let (claimed, unclaimed) = self
                .state
                .db
                .executor_seen_kind(self.repo, kind)
                .unwrap_or_default();
            let asked = no_executor(claimed, unclaimed)
                && self
                    .state
                    .db
                    .open_service_job_of_kind(self.repo, kind)
                    .unwrap_or(true);
            if asked {
                unserved.push(kind.clone());
            }
            !asked
        });
        if open.is_empty() && submit.is_empty() && !unserved.is_empty() {
            status.state = State::NoExecutor;
            status.message = format!(
                "No executor answered for kinds {}. Run one (`reef executor run`), or set `[landing] checks = \"proposal\"` if this repository's evidence does not come through executor jobs.",
                unserved.join(", ")
            );
            status.next = vec!["reef".into(), "executor".into(), "run".into()];
            return status.with_missing(needs);
        }
        if !submit.is_empty() {
            let wanted: Vec<Need> = needs
                .iter()
                .filter(|need| submit.contains(&need.kind))
                .cloned()
                .collect();
            if budget.open + submit.len() > MAX_OPEN_JOBS {
                status.state = if open.is_empty() {
                    State::Pending
                } else {
                    State::Waiting
                };
                status.message = format!(
                    "waits for a slot: {MAX_OPEN_JOBS} jobs are open for this repository's proposals"
                );
                status.jobs = open;
                status.retry_at = Some(Instant::now() + RETRY);
                return status.with_missing(needs);
            }
            let ask = Ask {
                repo: self.repo,
                tree,
                commit,
                change,
                priority: PROPOSAL_PRIORITY,
                queue_timeout: None,
            };
            for (kind, submitted) in submit_needs(self.state, &ask, &wanted) {
                match submitted {
                    Ok((job, created)) => {
                        if created {
                            budget.open += 1;
                        }
                        open.push(job.id);
                    }
                    Err(error) => {
                        tracing::warn!(repo = %self.repo, change_id = change, kind, error = %ErrorChain(&error), "cannot ask an executor for a proposal's evidence");
                    }
                }
            }
            if !open.is_empty() {
                tracing::info!(repo = %self.repo, change_id = change, jobs = open.len(), "asked executors for a proposal's evidence");
            }
        }
        if open.is_empty() {
            status.state = State::Pending;
            status.message = format!("asking executors for {kinds}");
            status.retry_at = Some(Instant::now() + RETRY);
        } else {
            status.message = format!("waits for {kinds} ({})", open.join(", "));
        }
        status.jobs = open;
        status.with_missing(needs)
    }

    /// The members of a passing prefix whose latest landing the service
    /// queued under this key and that came back for a reason that holds
    /// until something changes; `None` when the prefix may be queued.
    fn returned(&self, prefix: &[(String, Status)]) -> Option<Vec<(String, Status)>> {
        let mut found = Vec::new();
        for (change, status) in prefix {
            if status.state != State::Ready {
                continue;
            }
            let Ok(landings) = self.state.db.landings_of_change(self.repo, change, 8) else {
                continue;
            };
            let Some(latest) = landings.first() else {
                continue;
            };
            if latest.row.automatic_key.as_deref() != Some(self.key)
                || latest.row.outcome != Outcome::Dequeued.as_str()
            {
                continue;
            }
            let reason = latest.row.reason.clone().unwrap_or_default();
            let finished = latest.row.finished_at.unwrap_or(latest.row.created_at);
            let retry_at = match reason.as_str() {
                "busy" => {
                    let busy = landings
                        .iter()
                        .take_while(|landing| {
                            landing.row.automatic_key.as_deref() == Some(self.key)
                                && landing.row.reason.as_deref() == Some("busy")
                        })
                        .count();
                    let after = finished.0 + i64::try_from(BUSY_AFTER.as_millis()).unwrap_or(0);
                    let now = Timestamp::now().0;
                    if busy < MAX_BUSY && now >= after {
                        continue;
                    }
                    (busy < MAX_BUSY).then(|| {
                        Instant::now()
                            + Duration::from_millis(u64::try_from(after - now).unwrap_or(0))
                    })
                }
                "unavailable" => {
                    if self.claimed.is_some_and(|claimed| claimed.0 > finished.0) {
                        continue;
                    }
                    None
                }
                _ => None,
            };
            let mut back = Status::new(State::Returned, &status.commit);
            back.reason = Some(reason.clone());
            back.landing_id = Some(latest.row.id.clone());
            back.message = format!(
                "its automatic landing returned ({reason}): {}",
                latest.row.message.clone().unwrap_or_default()
            );
            back.next = vec![
                "reef".into(),
                "landing".into(),
                "show".into(),
                latest.row.id.clone(),
            ];
            back.retry_at = retry_at;
            found.push((change.clone(), back));
        }
        (!found.is_empty()).then_some(found)
    }
}

/// Whether no executor answers in a repository: a job of it expired with
/// nobody claiming it, and none was claimed since.
fn no_executor(claimed: Option<Timestamp>, unclaimed: Option<Timestamp>) -> bool {
    unclaimed.is_some_and(|unclaimed| claimed.is_none_or(|claimed| claimed.0 < unclaimed.0))
}

/// Whether `job` ended with no verdict: an executor ran it and it failed,
/// ran out of time or attempts, or completed without the evidence
/// counting. Unclaimed expiries and cancellations are not the tree's.
fn no_verdict(state: JobState, end_reason: Option<&str>) -> bool {
    match state {
        JobState::Completed => true,
        JobState::Expired => end_reason != Some("queue_timeout"),
        JobState::Queued | JobState::Running | JobState::Cancelled => false,
    }
}

/// The `[[require]]` needs of `evaluation`, and whether another rule
/// blocks it.
struct Requirements {
    failed: Vec<Need>,
    missing: Vec<Need>,
    signoffs: Vec<Need>,
    /// Rules that block with no evidence or sign-off to ask for.
    rules: Vec<Need>,
    other: bool,
}

/// The kind of a sign-off's need in a status's `missing`.
pub(crate) const SIGNOFF_NEED: &str = "signoff";
/// The kind of a blocking rule's need in a status's `missing`: a rule that
/// asks for no evidence or sign-off (`[[forbid]]`, `[limits]`, `[[rule]]`,
/// a built-in, or any rule that could not be evaluated), targeted at it.
pub(crate) const RULE_NEED: &str = "rule";

fn requirements(evaluation: &Evaluation) -> Requirements {
    let mut found = Requirements {
        failed: Vec::new(),
        missing: Vec::new(),
        signoffs: Vec::new(),
        rules: Vec::new(),
        other: false,
    };
    for rule in &evaluation.rules {
        if matches!(
            rule.status,
            RuleStatus::Satisfied | RuleStatus::NotApplicable
        ) {
            continue;
        }
        match rule.kind {
            RuleKind::Require if !rule.failed.is_empty() => {
                found.failed.extend(rule.failed.iter().cloned());
            }
            RuleKind::Require if !rule.missing.is_empty() => {
                found.missing.extend(rule.missing.iter().cloned());
            }
            // A sign-off a rule still needs, its own or built in (the
            // owners' on a policy file, a resolution's approval): one need
            // of kind `signoff`, targeted at the rule, from those who may
            // sign. A rule that could not be evaluated needs no sign-off.
            _ if rule.status == RuleStatus::Unsatisfied && rule.signoff.is_some() => {
                let mut attestors = Vec::new();
                if let Some(signoff) = &rule.signoff {
                    attestors.extend(signoff.groups.iter().cloned());
                    attestors.extend(signoff.identities.iter().cloned());
                }
                found.signoffs.push(Need {
                    kind: SIGNOFF_NEED.to_owned(),
                    target: rule.name.clone(),
                    attestors,
                    ..Need::default()
                });
                found.other = true;
            }
            _ => {
                found.rules.push(Need {
                    kind: RULE_NEED.to_owned(),
                    target: rule.name.clone(),
                    ..Need::default()
                });
                found.other = true;
            }
        }
    }
    found
}

#[cfg(test)]
mod tests {
    use super::*;

    fn rule(
        name: &str,
        kind: RuleKind,
        status: RuleStatus,
        signs: bool,
    ) -> reef_policy::RuleResult {
        reef_policy::RuleResult {
            name: name.to_owned(),
            kind,
            status,
            message: String::new(),
            paths: Vec::new(),
            paths_total: 0,
            missing: Vec::new(),
            failed: Vec::new(),
            signoff: signs.then(|| reef_policy::SignoffNeed {
                identities: vec!["alice".to_owned()],
                count: 1,
                ..reef_policy::SignoffNeed::default()
            }),
            evidence_ids: Vec::new(),
        }
    }

    fn kinds(rules: Vec<reef_policy::RuleResult>) -> Vec<(String, String)> {
        let mut evaluation = Evaluation::no_policy(&reef_policy::Input::default());
        evaluation.rules = rules;
        let found = requirements(&evaluation);
        found
            .signoffs
            .iter()
            .chain(&found.rules)
            .map(|need| (need.kind.clone(), need.target.clone()))
            .collect()
    }

    // R3.33: a sign-off a rule needs, its own or built in, is a need of
    // kind `signoff`; a rule that blocks with nothing to ask for is one of
    // kind `rule`, so a sign-off beside it is not the only thing missing;
    // and a sign-off rule that could not be evaluated asks for no sign-off.
    #[test]
    fn sign_offs_and_blocking_rules_are_needs() {
        use RuleKind as K;
        use RuleStatus as S;
        let pair = |kind: &str, target: &str| (kind.to_owned(), target.to_owned());
        assert_eq!(
            kinds(vec![rule(
                "builtin:owners",
                K::Builtin,
                S::Unsatisfied,
                true
            )]),
            [pair("signoff", "builtin:owners")]
        );
        assert_eq!(
            kinds(vec![
                rule("review", K::Signoff, S::Unsatisfied, true),
                rule("no-vendor", K::Forbid, S::Unsatisfied, false),
            ]),
            [pair("signoff", "review"), pair("rule", "no-vendor")]
        );
        assert_eq!(
            kinds(vec![rule("review", K::Signoff, S::Error, true)]),
            [pair("rule", "review")]
        );
        assert_eq!(
            kinds(vec![rule("review", K::Signoff, S::Satisfied, true)]),
            Vec::<(String, String)>::new()
        );
    }

    #[test]
    fn no_executor_until_a_claim_after_an_unclaimed_expiry() {
        let at = |millis| Some(Timestamp(millis));
        assert!(!no_executor(None, None), "nothing expired");
        assert!(no_executor(None, at(10)), "expired, never claimed");
        assert!(no_executor(at(5), at(10)), "claimed before it expired");
        assert!(!no_executor(at(11), at(10)), "claimed since");
    }

    #[test]
    fn only_an_executor_run_with_no_verdict_counts() {
        assert!(no_verdict(JobState::Completed, None));
        assert!(no_verdict(JobState::Expired, Some("deadline")));
        assert!(no_verdict(JobState::Expired, Some("attempts")));
        assert!(
            !no_verdict(JobState::Expired, Some("queue_timeout")),
            "nobody ran it"
        );
        assert!(!no_verdict(JobState::Cancelled, None));
        assert!(!no_verdict(JobState::Running, None));
    }
}
