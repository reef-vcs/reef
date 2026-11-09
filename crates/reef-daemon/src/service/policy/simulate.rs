//! `PolicyService.SimulatePolicy` (R3.08, ADR 0062): a proposed policy
//! evaluated over recent landings, beside a baseline.
//!
//! Until the landing service records landings (R3.13), a landing is a
//! commit of the request's revset, by default every non-merge ancestor of
//! `trunk()`, judged as its own change exactly as `EvaluatePolicy` judges
//! it. Each landing's input is read once and evaluated under both
//! policies. A page is bounded by its landings, its summed cost, the paths
//! its landings touch and its encoded size, and holds one evaluation turn.

use std::sync::Arc;
use std::sync::atomic::Ordering;
use std::time::Instant;

use prost::Message as _;
use reef_api::v1::{self, SimulatePolicyRequest, SimulatePolicyResponse};
use reef_lib::jj::revset;
use reef_lib::jj::{CommitIdHex, ReadonlyRepo, log};
use reef_policy::{Decision, Evaluation, RuleKind, Status as RuleStatus};
use tonic::{Request, Response, Status};

use super::super::page::{MAX_PAGE_BYTES, PageRequest};
use super::super::reading::{
    bad_token, op_id, positional, repo_at, snapshot_first, target, uses_working_copy,
};
use super::{
    ChangeRead, Loaded, PolicyError, PolicyFile, PolicyHandler, Source, evaluate_source,
    evaluation_message, policy_at, proposed_policy, read_change, rule_kind, source_message, task,
};
use crate::error::status;

/// The method a page token is bound to.
const SIMULATE_POLICY: &str = "/reef.api.v1.PolicyService/SimulatePolicy";

/// The landings when the request names none: every commit that reached the
/// destination, but merges, whose content is their parents'.
pub const DEFAULT_LANDINGS: &str = "::trunk() ~ root() ~ merges()";

/// Landings per page when the request says 0.
pub const DEFAULT_PAGE_LANDINGS: usize = 20;

/// The most landings in a page.
pub const MAX_PAGE_LANDINGS: usize = 100;

/// The landings in a page `page_size` asks for: 0 means
/// [`DEFAULT_PAGE_LANDINGS`], and more than [`MAX_PAGE_LANDINGS`] means
/// that many. A negative size is refused before this.
pub fn page_landings(page_size: i32) -> usize {
    match usize::try_from(page_size).unwrap_or_default() {
        0 => DEFAULT_PAGE_LANDINGS,
        size => size.min(MAX_PAGE_LANDINGS),
    }
}

/// The policy a simulation reads, loaded or not.
enum Policy {
    /// It loads.
    Loaded(Arc<Loaded>),
    /// The revision has no `reef-policy.toml`.
    Missing,
    /// It does not load, for these reasons; `read` when its bytes were read
    /// (a file too large or not a regular file was not).
    Invalid {
        diagnostics: Vec<reef_policy::Diagnostic>,
        read: bool,
    },
}

impl Policy {
    /// `source` loaded through the daemon's cache. Blocks.
    fn load(state: &crate::daemon::AppState, source: Source<'_>) -> Policy {
        let bytes = match source {
            Source::Text(bytes) => bytes,
            Source::File(None) => return Policy::Missing,
            Source::File(Some(file)) => match &file.bytes {
                Ok(bytes) => bytes,
                Err(problem) => {
                    return Policy::Invalid {
                        diagnostics: vec![reef_policy::Diagnostic {
                            message: problem.clone(),
                            ..reef_policy::Diagnostic::default()
                        }],
                        read: false,
                    };
                }
            },
        };
        let loaded = state.policy.load(bytes);
        match &*loaded {
            Loaded::Policy(_) => Policy::Loaded(loaded),
            Loaded::Invalid(diagnostics) => Policy::Invalid {
                diagnostics: diagnostics.clone(),
                read: true,
            },
        }
    }

    /// The format version a source reports, as `EvaluatePolicy` reports
    /// it: the one read, unless nothing was.
    fn version(&self) -> u32 {
        match self {
            Policy::Loaded(_) | Policy::Invalid { read: true, .. } => reef_policy::VERSION,
            Policy::Missing | Policy::Invalid { read: false, .. } => 0,
        }
    }

    /// The state a response reports: `ALLOW` for a policy that loads.
    fn state(&self) -> v1::PolicyDecision {
        match self {
            Policy::Loaded(_) => v1::PolicyDecision::Allow,
            Policy::Missing => v1::PolicyDecision::NoPolicy,
            Policy::Invalid { .. } => v1::PolicyDecision::InvalidPolicy,
        }
    }
}

/// How a landing's proposed decision compares with its baseline's; `None`
/// for no baseline.
pub fn outcome(proposed: Decision, baseline: Option<Decision>) -> v1::PolicySimulationOutcome {
    use v1::PolicySimulationOutcome as Outcome;
    let blocked = proposed != Decision::Allow;
    match baseline {
        None | Some(Decision::NoPolicy) => {
            if blocked {
                Outcome::Blocked
            } else {
                Outcome::Allowed
            }
        }
        Some(Decision::Allow) => {
            if blocked {
                Outcome::NewlyBlocked
            } else {
                Outcome::StillAllowed
            }
        }
        Some(Decision::Block | Decision::InvalidPolicy) => {
            if blocked {
                Outcome::StillBlocked
            } else {
                Outcome::NewlyAllowed
            }
        }
    }
}

/// Whether a rule result blocks.
fn unmet(rule: &reef_policy::RuleResult) -> bool {
    matches!(rule.status, RuleStatus::Unsatisfied | RuleStatus::Error)
}

/// Whether `evaluation` blocks only for evidence that is missing: every
/// unmet rule is a `[[require]]` that is unsatisfied with nothing failed.
pub fn missing_evidence_only(evaluation: &Evaluation) -> bool {
    let mut unmet_rules = evaluation
        .rules
        .iter()
        .filter(|rule| unmet(rule))
        .peekable();
    evaluation.decision == Decision::Block
        && unmet_rules.peek().is_some()
        && unmet_rules.all(|rule| {
            rule.kind == RuleKind::Require
                && rule.status == RuleStatus::Unsatisfied
                && rule.failed.is_empty()
                && !rule.missing.is_empty()
        })
}

/// One landing evaluated: its message, and what the page's bounds count.
struct Judged {
    landing: v1::PolicySimulatedLanding,
    proposed: Evaluation,
    paths: usize,
    /// Evidence and provenance rows read for it.
    rows: u64,
}

/// `read`'s input evaluated under `proposed` and `baseline` (`None` for no
/// baseline). Blocks.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
fn judge(
    state: &crate::daemon::AppState,
    read: ChangeRead,
    evidence: Vec<reef_policy::input::Evidence>,
    evidence_ignored: i64,
    provenance: Vec<reef_policy::input::Provenance>,
    provenance_ignored: i64,
    proposed: &reef_policy::Policy,
    baseline: Option<Source<'_>>,
    include_evaluations: bool,
) -> Judged {
    let ChangeRead {
        change,
        tree_id,
        proposed: own_file,
        committed_at,
    } = read;
    let input = reef_policy::Input {
        change,
        evidence,
        provenance,
        evidence_ignored,
        provenance_ignored,
        proposed_policy: proposed_policy(state, own_file.as_ref()),
    };
    let evaluation = proposed.evaluate(&input);
    // As `EvaluatePolicy` evaluates it, through the cache.
    let baseline_evaluation = baseline.map(|source| evaluate_source(state, source, &input));
    let decision = |decision: Decision| -> i32 {
        match decision {
            Decision::Allow => v1::PolicyDecision::Allow,
            Decision::Block => v1::PolicyDecision::Block,
            Decision::NoPolicy => v1::PolicyDecision::NoPolicy,
            Decision::InvalidPolicy => v1::PolicyDecision::InvalidPolicy,
        }
        .into()
    };
    let proposed_message = evaluation_message(&evaluation);
    let mut landing = v1::PolicySimulatedLanding {
        change_id: input.change.id.clone(),
        commit_id: input.change.commit.clone(),
        tree_id: tree_id.clone().unwrap_or_default(),
        subject: input
            .change
            .description
            .lines()
            .next()
            .unwrap_or_default()
            .to_owned(),
        author: format!(
            "{} <{}>",
            input.change.author.name, input.change.author.email
        ),
        committed_at: Some(super::super::convert::system_timestamp(committed_at)),
        decision: decision(evaluation.decision),
        baseline_decision: baseline_evaluation
            .as_ref()
            .map_or(0, |baseline| decision(baseline.decision)),
        outcome: outcome(
            evaluation.decision,
            baseline_evaluation
                .as_ref()
                .map(|baseline| baseline.decision),
        )
        .into(),
        unmet: proposed_message
            .rules
            .iter()
            .zip(&evaluation.rules)
            .filter(|(_, rule)| unmet(rule))
            .map(|(message, _)| message.clone())
            .collect(),
        baseline_unmet: baseline_evaluation
            .as_ref()
            .map(|baseline| {
                baseline
                    .rules
                    .iter()
                    .filter(|rule| unmet(rule))
                    .map(|rule| rule.name.clone())
                    .collect()
            })
            .unwrap_or_default(),
        missing_evidence_only: missing_evidence_only(&evaluation),
        cost: evaluation.cost
            + baseline_evaluation
                .as_ref()
                .map_or(0, |baseline| baseline.cost),
        evaluation: None,
        baseline_evaluation: None,
        trimmed: false,
    };
    if include_evaluations {
        let finish = |mut message: v1::PolicyEvaluation| {
            message.change_id.clone_from(&landing.change_id);
            message.commit_id.clone_from(&landing.commit_id);
            message.tree_id.clone_from(&landing.tree_id);
            message
        };
        landing.evaluation = Some(finish(proposed_message));
        landing.baseline_evaluation = baseline_evaluation
            .as_ref()
            .map(|baseline| finish(evaluation_message(baseline)));
    }
    let rows = i64::try_from(input.evidence.len())
        .unwrap_or(i64::MAX)
        .saturating_add(input.evidence_ignored)
        .min(i64::try_from(super::MAX_EVIDENCE_READ).unwrap_or(i64::MAX))
        .saturating_add(i64::try_from(input.provenance.len()).unwrap_or(i64::MAX));
    Judged {
        landing,
        paths: input.change.paths.len(),
        rows: u64::try_from(rows).unwrap_or(0),
        proposed: evaluation,
    }
}

/// The page's counts, from its landings and their proposed evaluations.
fn summary(judged: &[(v1::PolicySimulatedLanding, Evaluation)]) -> v1::PolicySimulationSummary {
    use v1::PolicySimulationOutcome as Outcome;
    let mut summary = v1::PolicySimulationSummary::default();
    // Every evaluation of one policy reports the same rules in the same
    // order (built-ins first, then file order), so the first one orders
    // the counts.
    let mut rules: Vec<v1::PolicySimulationRuleCount> = Vec::new();
    for (landing, evaluation) in judged {
        summary.landings += 1;
        if landing.decision != v1::PolicyDecision::Allow as i32 {
            summary.blocked += 1;
        }
        if landing.missing_evidence_only {
            summary.missing_evidence_only += 1;
        }
        match Outcome::try_from(landing.outcome).unwrap_or_default() {
            Outcome::NewlyBlocked => summary.newly_blocked += 1,
            Outcome::StillBlocked => summary.still_blocked += 1,
            Outcome::NewlyAllowed => summary.newly_allowed += 1,
            Outcome::StillAllowed => summary.still_allowed += 1,
            _ => {}
        }
        for rule in &evaluation.rules {
            let at = if let Some(at) = rules.iter().position(|count| count.name == rule.name) {
                at
            } else {
                rules.push(v1::PolicySimulationRuleCount {
                    name: rule.name.clone(),
                    kind: rule_kind(rule.kind).into(),
                    landings: 0,
                });
                rules.len() - 1
            };
            if unmet(rule) {
                rules[at].landings += 1;
            }
        }
    }
    summary.rules = rules;
    summary
}

/// The error for a page whose read was stopped.
fn stopped() -> Status {
    Status::cancelled(
        "the simulation stopped: its caller went away, or the daemon is shutting down",
    )
}

/// Trims a landing too large for a page alone: first its full
/// evaluations, then the paths its unmet rules list (their totals stay),
/// marking it `trimmed`. Returns its encoded size.
fn fit(landing: &mut v1::PolicySimulatedLanding) -> usize {
    if landing.encoded_len() > MAX_PAGE_BYTES {
        landing.evaluation = None;
        landing.baseline_evaluation = None;
        landing.trimmed = true;
    }
    if landing.encoded_len() > MAX_PAGE_BYTES {
        landing.trimmed = true;
        for rule in &mut landing.unmet {
            rule.paths.clear();
        }
    }
    landing.encoded_len()
}

/// What the first read of a page finds: the page's commits and the
/// policies' files.
struct Window {
    repo: Arc<ReadonlyRepo>,
    commits: Vec<CommitIdHex>,
    more: bool,
    proposed: Option<(String, Option<PolicyFile>)>,
    baseline: Option<(String, Option<PolicyFile>)>,
    at_operation: String,
}

/// Serves `SimulatePolicy`.
#[allow(clippy::too_many_lines)]
pub(super) async fn simulate(
    handler: &PolicyHandler,
    request: Request<SimulatePolicyRequest>,
) -> Result<Response<SimulatePolicyResponse>, Status> {
    let agent = super::super::caller_of(&request)?;
    let request = request.into_inner();
    let from_request = !request.policy.is_empty();
    let policy_revision = request.policy_revision.trim().to_owned();
    if from_request && !policy_revision.is_empty() {
        return Err(status(PolicyError::SourceConflict));
    }
    if !from_request && policy_revision.is_empty() {
        return Err(status(PolicyError::SourceMissing));
    }
    let max = reef_policy::format::MAX_FILE_BYTES;
    if request.policy.len() > max {
        return Err(status(PolicyError::TooLarge {
            bytes: request.policy.len(),
            max,
        }));
    }
    let page = PageRequest::new(
        SIMULATE_POLICY,
        request.page_size,
        &request.page_token,
        &SimulatePolicyRequest {
            page_size: 0,
            page_token: String::new(),
            ..request.clone()
        },
    )
    .map_err(status)?;
    let size = page_landings(request.page_size);
    let (skip, expected) = match page.cursor() {
        Some(cursor) if page.pinned().is_some() => positional(cursor)?,
        Some(_) => return Err(bad_token()),
        None => (0, String::new()),
    };
    let state = &handler.state;
    // At most `CONCURRENT_SIMULATIONS` pages are read at once; each of a
    // page's evaluations also takes one of the daemon's evaluation turns,
    // one at a time, moved into the task that runs it (S-98).
    let _page = state
        .policy
        .simulations
        .acquire()
        .await
        .map_err(|error| status(PolicyError::Task(error.to_string())))?;
    let target = target(state, &request.repo, &request.workspace)?;
    let revisions = match request.revisions.trim() {
        "" => DEFAULT_LANDINGS.to_owned(),
        text => text.to_owned(),
    };
    let baseline_revision = match request.baseline_revision.trim() {
        "" => "trunk()".to_owned(),
        text => text.to_owned(),
    };
    if page.pinned().is_none() && request.at_operation.is_empty() && !request.skip_snapshot {
        let mut texts = vec![revisions.clone(), policy_revision.clone()];
        if !request.no_baseline {
            texts.push(baseline_revision.clone());
        }
        let needed = uses_working_copy(state, &target, texts).await?;
        snapshot_first(state, &target, needed, Instant::now(), agent).await?;
    }
    let head = state.reads.current(&target.handle).await.map_err(status)?;
    let reads = Arc::clone(&state.reads);
    let (stop, _guard) = state.reads.stop();
    let read_target = target.clone();
    let pinned = page.pinned().map(str::to_owned);
    let at_operation = request.at_operation.clone();
    let wanted_policy = policy_revision.clone();
    let wanted_baseline = (!request.no_baseline).then(|| baseline_revision.clone());
    let window_stop = stop.clone();
    let window = state
        .reads
        .run(&target.handle, move || async move {
            let target = read_target;
            let scope = target.scope();
            let repo = repo_at(
                &reads,
                target.repo.id,
                &head,
                pinned.as_deref(),
                &at_operation,
            )
            .await?;
            let parsed = revset::parse(&repo, &revisions, &scope).map_err(status)?;
            let found = log::log_page(&repo, &parsed, skip, size, &|| window_stop.requested())
                .await
                .map_err(status)?;
            if skip > 0
                && found
                    .last_skipped
                    .as_ref()
                    .map(ToString::to_string)
                    .as_deref()
                    != Some(expected.as_str())
            {
                return Err(bad_token());
            }
            let proposed = if wanted_policy.is_empty() {
                None
            } else {
                Some(policy_at(&repo, &scope, &wanted_policy).await?)
            };
            let baseline = match &wanted_baseline {
                Some(revision) => Some(policy_at(&repo, &scope, revision).await?),
                None => None,
            };
            let at_operation = op_id(&repo);
            Ok::<_, Status>(Window {
                commits: found.nodes.into_iter().map(|node| node.commit_id).collect(),
                more: found.more,
                proposed,
                baseline,
                at_operation,
                repo,
            })
        })
        .await
        .map_err(status)??;
    if stop.requested() {
        return Err(stopped());
    }

    // The policies, loaded once through the cache.
    let proposed_source = match &window.proposed {
        Some((_, file)) => Source::File(file.as_ref()),
        None => Source::Text(&request.policy),
    };
    let blocking = Arc::clone(state);
    let proposed_bytes = request.policy.clone();
    let proposed_file = window.proposed.as_ref().map(|(_, file)| file.clone());
    let baseline_file = window.baseline.as_ref().map(|(_, file)| file.clone());
    let (proposed, baseline) = tokio::task::spawn_blocking(move || {
        let source = match &proposed_file {
            Some(file) => Source::File(file.as_ref()),
            None => Source::Text(&proposed_bytes),
        };
        let proposed = Policy::load(&blocking, source);
        let baseline =
            baseline_file.map(|file| Policy::load(&blocking, Source::File(file.as_ref())));
        (proposed, baseline)
    })
    .await
    .map_err(|error| task(&error))?;
    let sha256 = |source: Source<'_>| match source {
        Source::Text(bytes) => reef_policy::sha256_hex(bytes),
        Source::File(Some(PolicyFile {
            bytes: Ok(bytes), ..
        })) => reef_policy::sha256_hex(bytes),
        Source::File(_) => String::new(),
    };
    let mut response = SimulatePolicyResponse {
        proposed_decision: proposed.state().into(),
        proposed: Some(source_message(
            &policy_revision,
            window.proposed.as_ref().map_or("", |(commit, _)| commit),
            proposed_source,
            &sha256(proposed_source),
            proposed.version(),
        )),
        at_operation: window.at_operation.clone(),
        ..SimulatePolicyResponse::default()
    };
    if let (Some(policy), Some((commit, file))) = (&baseline, &window.baseline) {
        let source = Source::File(file.as_ref());
        response.baseline = Some(source_message(
            &baseline_revision,
            commit,
            source,
            &sha256(source),
            policy.version(),
        ));
        response.baseline_state = policy.state().into();
    }
    let compiled = match &proposed {
        Policy::Loaded(loaded) => Arc::clone(loaded),
        Policy::Missing => return Ok(Response::new(response)),
        Policy::Invalid { diagnostics, .. } => {
            response.diagnostics = evaluation_message(&Evaluation::invalid(
                &[],
                diagnostics.clone(),
                &reef_policy::Input::default(),
            ))
            .diagnostics;
            return Ok(Response::new(response));
        }
    };
    drop(baseline);
    let baseline_file = window
        .baseline
        .as_ref()
        .map(|(_, file)| Arc::new(file.clone()));

    let mut judged: Vec<(v1::PolicySimulatedLanding, Evaluation)> = Vec::new();
    let mut bytes = 0usize;
    let mut cost = 0u64;
    let mut paths = 0u64;
    let mut rows = 0u64;
    let mut ended_early = false;
    let repo_id = target.repo.id;
    let max_cost = state.policy.page_cost.load(Ordering::Relaxed);
    let max_paths = state.policy.page_paths.load(Ordering::Relaxed);
    let max_rows = state.policy.page_rows.load(Ordering::Relaxed);
    for (at, commit) in window.commits.iter().enumerate() {
        if stop.requested() {
            return Err(stopped());
        }
        let repo = Arc::clone(&window.repo);
        let read_target = target.clone();
        let commit = commit.clone();
        let read_stop = stop.clone();
        let read = state
            .reads
            .run(&target.handle, move || async move {
                let scope = read_target.scope();
                read_change(&repo, &scope, &commit, &|| read_stop.requested()).await
            })
            .await
            .map_err(status)??;
        let (evidence, evidence_ignored) = match &read.tree_id {
            Some(tree) => handler.evidence(repo_id, tree).await?,
            None => (Vec::new(), 0),
        };
        let (provenance, provenance_ignored) = handler.provenance(repo_id, &read.change.id).await?;
        let blocking = Arc::clone(state);
        let compiled = Arc::clone(&compiled);
        let baseline = baseline_file.clone();
        let include_evaluations = request.include_evaluations;
        let turn = Arc::clone(&state.policy.turns)
            .acquire_owned()
            .await
            .map_err(|error| status(PolicyError::Task(error.to_string())))?;
        let mut one = tokio::task::spawn_blocking(move || {
            let _turn = turn;
            let Loaded::Policy(policy) = &*compiled else {
                unreachable!("only a policy that loads is simulated")
            };
            judge(
                &blocking,
                read,
                evidence,
                evidence_ignored,
                provenance,
                provenance_ignored,
                policy,
                baseline.as_deref().map(|file| Source::File(file.as_ref())),
                include_evaluations,
            )
        })
        .await
        .map_err(|error| task(&error))?;
        // The full evaluations say which policy and which operation, as
        // EvaluatePolicy's do.
        if let Some(evaluation) = &mut one.landing.evaluation {
            evaluation.source.clone_from(&response.proposed);
            evaluation.at_operation.clone_from(&window.at_operation);
        }
        if let Some(evaluation) = &mut one.landing.baseline_evaluation {
            evaluation.source.clone_from(&response.baseline);
            evaluation.at_operation.clone_from(&window.at_operation);
        }
        let size = fit(&mut one.landing);
        if !judged.is_empty() && bytes + size > MAX_PAGE_BYTES {
            ended_early = true;
            break;
        }
        bytes += size;
        cost = cost.saturating_add(one.landing.cost);
        paths = paths.saturating_add(u64::try_from(one.paths).unwrap_or(u64::MAX));
        rows = rows.saturating_add(one.rows);
        judged.push((one.landing, one.proposed));
        if (cost > max_cost || paths > max_paths || rows > max_rows)
            && at + 1 < window.commits.len()
        {
            ended_early = true;
            break;
        }
    }
    response.summary = Some(summary(&judged));
    response.cost = cost;
    response.next_page_token = match judged.last() {
        Some((last, _)) if window.more || ended_early => page.token_at(
            &format!("{}:{}", skip + judged.len(), last.commit_id),
            &window.at_operation,
        ),
        _ => String::new(),
    };
    response.landings = judged.into_iter().map(|(landing, _)| landing).collect();
    Ok(Response::new(response))
}

#[cfg(test)]
mod tests {
    use v1::PolicySimulationOutcome as Outcome;

    use super::*;

    #[test]
    fn page_sizes_default_and_clamp() {
        assert_eq!(page_landings(0), DEFAULT_PAGE_LANDINGS);
        assert_eq!(page_landings(7), 7);
        assert_eq!(page_landings(100), 100);
        assert_eq!(page_landings(500), MAX_PAGE_LANDINGS);
    }

    #[test]
    fn outcomes_follow_the_table() {
        use Decision::{Allow, Block, InvalidPolicy, NoPolicy};
        let cases = [
            (Block, Some(Allow), Outcome::NewlyBlocked),
            (Block, Some(Block), Outcome::StillBlocked),
            (Block, Some(InvalidPolicy), Outcome::StillBlocked),
            (Allow, Some(Block), Outcome::NewlyAllowed),
            (Allow, Some(InvalidPolicy), Outcome::NewlyAllowed),
            (Allow, Some(Allow), Outcome::StillAllowed),
            (Block, Some(NoPolicy), Outcome::Blocked),
            (Allow, Some(NoPolicy), Outcome::Allowed),
            (Block, None, Outcome::Blocked),
            (Allow, None, Outcome::Allowed),
        ];
        for (proposed, baseline, expected) in cases {
            assert_eq!(
                outcome(proposed, baseline),
                expected,
                "{proposed:?} {baseline:?}"
            );
        }
    }

    #[test]
    fn a_landing_too_large_for_a_page_is_trimmed() {
        let rule = v1::PolicyRuleResult {
            name: "secrets".to_owned(),
            paths: vec!["p".repeat(4096); 20],
            paths_total: 20,
            ..v1::PolicyRuleResult::default()
        };
        let evaluation = v1::PolicyEvaluation {
            rules: vec![rule.clone(); 40],
            ..v1::PolicyEvaluation::default()
        };
        let mut small = v1::PolicySimulatedLanding {
            unmet: vec![rule.clone()],
            evaluation: Some(v1::PolicyEvaluation::default()),
            ..v1::PolicySimulatedLanding::default()
        };
        fit(&mut small);
        assert!(!small.trimmed);
        assert!(small.evaluation.is_some());
        // The evaluations alone are too large: they go, the unmet rules stay.
        let mut large = v1::PolicySimulatedLanding {
            unmet: vec![rule.clone()],
            evaluation: Some(evaluation.clone()),
            baseline_evaluation: Some(evaluation),
            ..v1::PolicySimulatedLanding::default()
        };
        let size = fit(&mut large);
        assert!(large.trimmed);
        assert!(large.evaluation.is_none() && large.baseline_evaluation.is_none());
        assert_eq!(large.unmet[0].paths.len(), 20);
        assert!(size <= MAX_PAGE_BYTES);
        // Unmet rules too large: their paths go, their totals stay.
        let mut huge = v1::PolicySimulatedLanding {
            unmet: vec![rule; 60],
            ..v1::PolicySimulatedLanding::default()
        };
        let size = fit(&mut huge);
        assert!(huge.trimmed);
        assert!(
            huge.unmet
                .iter()
                .all(|rule| rule.paths.is_empty() && rule.paths_total == 20)
        );
        assert!(size <= MAX_PAGE_BYTES);
    }
}
