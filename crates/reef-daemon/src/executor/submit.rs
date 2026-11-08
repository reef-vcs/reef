//! `SubmitJob`: a job request's bounds, the dedupe and the open-job cap
//! (R3.05).
//!
//! A request names a tree, kinds, targets, a budget, requirements and a
//! priority, and never a command (S-75): what runs for a kind and a target
//! is the executor's. [`Executor::submit`] is also what R3.13 and R3.14
//! call in process.

use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

use rand::RngExt as _;

use super::{Executor, ExecutorError, millis};
use crate::config::ExecutorSection;
use crate::db::{
    DbError, JobRow, JobState, insert_job_in, open_job_by_spec_in, open_job_count_in,
    repository_exists_in, update_job_in,
};
use crate::model::{RepoId, Timestamp};

/// Kinds a job requests at most.
pub const MAX_KINDS: usize = 16;
/// Targets a job requests at most.
pub const MAX_TARGETS: usize = 1000;
/// Requested (kind, target) pairs at most.
pub const MAX_PAIRS: usize = 1000;
/// Requirements a job names at most.
pub const MAX_REQUIREMENTS: usize = 16;
/// Labels an executor declares at most.
pub const MAX_LABELS: usize = 32;
/// Kinds an executor declares at most.
pub const MAX_EXECUTOR_KINDS: usize = 64;
/// The longest CPU time a request may ask for.
pub const MAX_CPU_TIME: Duration = Duration::from_hours(7 * 24);
/// The shortest queue timeout a request may ask for.
pub const MIN_QUEUE_TIMEOUT: Duration = Duration::from_mins(1);
/// The longest queue timeout a request may ask for.
pub const MAX_QUEUE_TIMEOUT: Duration = Duration::from_hours(7 * 24);

/// A job request, its tree resolved and checked.
#[derive(Clone, Debug)]
pub struct NewJob {
    /// The repository.
    pub repo_id: RepoId,
    /// The tree, checked present.
    pub tree_id: String,
    /// A commit with that tree, when submitted by revision.
    pub commit_id: Option<String>,
    /// The change, when submitted by revision.
    pub change_id: Option<String>,
    /// The kinds, as sent.
    pub kinds: Vec<String>,
    /// The targets, as sent.
    pub targets: Vec<String>,
    /// The requirements, as sent.
    pub requirements: Vec<String>,
    /// The wall time per attempt; `None` for the default.
    pub wall_time: Option<Duration>,
    /// The CPU time per attempt; `None` for no limit.
    pub cpu_time: Option<Duration>,
    /// Higher is claimed first.
    pub priority: i32,
    /// How long it may wait unclaimed; `None` for the default.
    pub queue_timeout: Option<Duration>,
    /// Queue a new job even when an identical one is open.
    pub duplicate: bool,
    /// The submitter's `reef-agent`.
    pub agent: Option<String>,
    /// The submitting remote token.
    pub token: Option<String>,
}

/// The bounds a request is checked against, from `[executor]`.
#[derive(Clone, Copy, Debug)]
pub struct SpecLimits {
    /// The default wall time.
    pub default_wall_time: Duration,
    /// The longest wall time.
    pub max_wall_time: Duration,
}

impl From<&ExecutorSection> for SpecLimits {
    fn from(settings: &ExecutorSection) -> Self {
        SpecLimits {
            default_wall_time: settings.default_wall_time,
            max_wall_time: settings.max_wall_time,
        }
    }
}

fn invalid(field: &'static str, message: impl Into<String>) -> ExecutorError {
    ExecutorError::InvalidSpec {
        field,
        message: message.into(),
    }
}

/// `list` trimmed, sorted and deduplicated.
fn normalized(list: &[String]) -> Vec<String> {
    list.iter()
        .map(|item| item.trim().to_owned())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect()
}

/// Why `label` is not `key=value`, if it is not: the key
/// `[a-z0-9][a-z0-9._-]{0,62}`, the value 1 to 128 bytes of visible ASCII.
pub fn label_problem(label: &str) -> Option<String> {
    let Some((key, value)) = label.split_once('=') else {
        return Some(format!("{label:?} is not key=value"));
    };
    let key_ok = (1..=63).contains(&key.len())
        && key
            .bytes()
            .next()
            .is_some_and(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
        && key.bytes().all(|b| {
            b.is_ascii_lowercase() || b.is_ascii_digit() || matches!(b, b'.' | b'_' | b'-')
        });
    if !key_ok {
        return Some(format!("key {key:?} is not [a-z0-9][a-z0-9._-]{{0,62}}"));
    }
    if !(1..=128).contains(&value.len()) || !value.bytes().all(|b| b.is_ascii_graphic()) {
        return Some(format!(
            "value {value:?} is not 1 to 128 bytes of visible ASCII"
        ));
    }
    None
}

/// Checks a list of labels: at most `max`, each `key=value`. Returns them
/// sorted and deduplicated.
pub fn labels(list: &[String], max: usize) -> Result<Vec<String>, String> {
    let list = normalized(list);
    if list.len() > max {
        return Err(format!("{} labels; at most {max}", list.len()));
    }
    for label in &list {
        if let Some(problem) = label_problem(label) {
            return Err(problem);
        }
    }
    Ok(list)
}

/// Checks a list of kinds: 1 to `max`, each in R3.01's form. Returns them
/// sorted and deduplicated.
pub fn kinds(list: &[String], max: usize) -> Result<Vec<String>, String> {
    let list = normalized(list);
    if list.is_empty() || list.len() > max {
        return Err(format!("{} kinds; 1 to {max}", list.len()));
    }
    if let Some(kind) = list
        .iter()
        .find(|kind| !reef_evidence::verify::is_kind(kind))
    {
        return Err(format!("{kind:?} is not ^[a-z][a-z0-9-]{{0,62}}$"));
    }
    Ok(list)
}

/// The checked spec: everything a [`JobRow`] takes from the request.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Spec {
    /// Sorted kinds.
    pub kinds: Vec<String>,
    /// Sorted targets.
    pub targets: Vec<String>,
    /// Sorted requirements.
    pub requirements: Vec<String>,
    /// Wall time per attempt.
    pub wall_time: Duration,
    /// CPU time per attempt.
    pub cpu_time: Option<Duration>,
    /// Queue timeout.
    pub queue_timeout: Duration,
}

/// Checks `job` against `limits` and `queue_timeout`, the default.
pub fn check(
    job: &NewJob,
    limits: SpecLimits,
    default_queue_timeout: Duration,
) -> Result<Spec, ExecutorError> {
    let kinds = kinds(&job.kinds, MAX_KINDS).map_err(|message| invalid("kinds", message))?;
    let targets: Vec<String> = job
        .targets
        .iter()
        .cloned()
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    if targets.len() > MAX_TARGETS {
        return Err(invalid(
            "targets",
            format!("{} targets; at most {MAX_TARGETS}", targets.len()),
        ));
    }
    if let Some(problem) = targets
        .iter()
        .find_map(|target| reef_evidence::verify::target_problem(target))
    {
        return Err(invalid("targets", problem));
    }
    let pairs = kinds.len() * targets.len().max(1);
    if pairs > MAX_PAIRS {
        return Err(invalid(
            "pairs",
            format!("{pairs} (kind, target) pairs; at most {MAX_PAIRS}"),
        ));
    }
    let requirements = labels(&job.requirements, MAX_REQUIREMENTS)
        .map_err(|message| invalid("requirements", message))?;
    let wall_time = job.wall_time.unwrap_or(limits.default_wall_time);
    if !(Duration::from_secs(1)..=limits.max_wall_time).contains(&wall_time) {
        return Err(invalid(
            "wall_time",
            format!(
                "{}s; from 1s to [executor] max_wall_time ({}s)",
                wall_time.as_secs(),
                limits.max_wall_time.as_secs()
            ),
        ));
    }
    if let Some(cpu) = job.cpu_time
        && !(Duration::from_secs(1)..=MAX_CPU_TIME).contains(&cpu)
    {
        return Err(invalid(
            "cpu_time",
            format!("{}s; from 1s to 7 days", cpu.as_secs()),
        ));
    }
    let queue_timeout = job.queue_timeout.unwrap_or(default_queue_timeout);
    if !(MIN_QUEUE_TIMEOUT..=MAX_QUEUE_TIMEOUT).contains(&queue_timeout) {
        return Err(invalid(
            "queue_timeout",
            format!("{}s; from 1 minute to 7 days", queue_timeout.as_secs()),
        ));
    }
    Ok(Spec {
        kinds,
        targets,
        requirements,
        wall_time,
        cpu_time: job.cpu_time,
        queue_timeout,
    })
}

/// SHA-256 of the canonical JSON of the fields that make two jobs the same
/// work: the tree, kinds, targets and requirements, each sorted.
pub fn spec_digest(tree_id: &str, spec: &Spec) -> String {
    let canonical: BTreeMap<&str, serde_json::Value> = BTreeMap::from([
        ("kinds", serde_json::json!(spec.kinds)),
        ("requirements", serde_json::json!(spec.requirements)),
        ("targets", serde_json::json!(spec.targets)),
        ("tree_id", serde_json::json!(tree_id)),
    ]);
    let bytes = serde_json::to_vec(&canonical).unwrap_or_default();
    reef_evidence::sha256_hex(&bytes)
}

impl Executor {
    /// Queues `job`, or returns the identical open one (`false`) with its
    /// priority raised to the higher of the two, unless `job.duplicate`.
    /// Past `[executor] max_open_jobs`, `TOO_MANY_JOBS`.
    pub fn submit(&self, job: &NewJob) -> Result<(JobRow, bool), ExecutorError> {
        let spec = check(
            job,
            SpecLimits::from(&self.settings),
            self.settings.queue_timeout,
        )?;
        let digest = spec_digest(&job.tree_id, &spec);
        let now = self.now();
        let limit = self.settings.max_open_jobs;
        let max_attempts = self.settings.max_attempts;
        let (row, created, _) = self.db.transact_then(
            |tx| {
                // A forget that committed after the request resolved its
                // repository leaves nothing to queue in.
                if !repository_exists_in(tx, job.repo_id)? {
                    return Err(ExecutorError::Db(DbError::NotFound {
                        what: "repository",
                        id: job.repo_id.as_hex(),
                    }));
                }
                // Dedupe within one submitter: another token's job is not the
                // caller's to cancel or reprioritize.
                if !job.duplicate
                    && let Some(mut open) =
                        open_job_by_spec_in(tx, job.repo_id, &digest, job.token.as_deref())?
                {
                    let before = open.state;
                    if job.priority > open.priority {
                        open.priority = job.priority;
                        update_job_in(tx, &open)?;
                    }
                    return Ok::<_, ExecutorError>((open, false, before));
                }
                if open_job_count_in(tx)? >= u64::from(limit) {
                    return Err(ExecutorError::TooMany { limit });
                }
                let row = JobRow {
                    repo_id: job.repo_id,
                    id: format!("job_{:016x}", rand::rng().random::<u64>()),
                    protocol: reef_api::EXECUTOR_PROTOCOL,
                    tree_id: job.tree_id.clone(),
                    commit_id: job.commit_id.clone(),
                    change_id: job.change_id.clone(),
                    kinds: spec.kinds.clone(),
                    targets: spec.targets.clone(),
                    requirements: spec.requirements.clone(),
                    spec_digest: digest.clone(),
                    wall_time_ms: millis(spec.wall_time),
                    cpu_time_ms: spec.cpu_time.map(millis),
                    priority: job.priority,
                    state: JobState::Queued,
                    outcome: None,
                    end_reason: None,
                    message: None,
                    attempt: 0,
                    max_attempts,
                    submitted_at: now,
                    queue_expires_at: Timestamp(now.0.saturating_add(millis(spec.queue_timeout))),
                    requested_by_agent: job.agent.clone(),
                    requested_by_token: job.token.clone(),
                    claim_digest: None,
                    claimed_at: None,
                    lease_ms: None,
                    lease_expires_at: None,
                    deadline_at: None,
                    executor: None,
                    progress: None,
                    usage_wall_ms: None,
                    usage_cpu_ms: None,
                    elapsed_ms: 0,
                    finished_at: None,
                    cancelled_by_agent: None,
                    cancel_reason: None,
                };
                insert_job_in(tx, &row)?;
                Ok((row, true, JobState::Queued))
            },
            |(row, created, before)| {
                if *created {
                    super::audit::submitted(row);
                }
                self.after(*before, row);
            },
        )?;
        Ok((row, created))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn new_job() -> NewJob {
        NewJob {
            repo_id: "00000000000000aa".parse().unwrap(),
            tree_id: "4b825dc642cb6eb9a060e54bf8d69288fbee4904".into(),
            commit_id: None,
            change_id: None,
            kinds: vec!["test".into(), "lint".into(), "test".into()],
            targets: vec!["b".into(), "a".into()],
            requirements: vec!["os=linux".into()],
            wall_time: None,
            cpu_time: None,
            priority: 0,
            queue_timeout: None,
            duplicate: false,
            agent: None,
            token: None,
        }
    }

    fn limits() -> SpecLimits {
        SpecLimits::from(&ExecutorSection::default())
    }

    fn field(job: &NewJob) -> &'static str {
        match check(job, limits(), Duration::from_hours(1)) {
            Err(ExecutorError::InvalidSpec { field, .. }) => field,
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn specs_are_normalized_and_bounded() {
        let spec = check(&new_job(), limits(), Duration::from_hours(1)).unwrap();
        assert_eq!(spec.kinds, ["lint", "test"]);
        assert_eq!(spec.targets, ["a", "b"]);
        assert_eq!(spec.wall_time, Duration::from_mins(30));
        let mut job = new_job();
        job.kinds = (0..17).map(|i| format!("k{i}")).collect();
        assert_eq!(field(&job), "kinds");
        job = new_job();
        job.kinds = vec!["Test".into()];
        assert_eq!(field(&job), "kinds");
        job = new_job();
        job.targets = (0..1001).map(|i| format!("t{i}")).collect();
        assert_eq!(field(&job), "targets");
        job = new_job();
        job.targets = vec!["a\u{7}".into()];
        assert_eq!(field(&job), "targets");
        job = new_job();
        job.targets = (0..600).map(|i| format!("t{i}")).collect();
        assert_eq!(field(&job), "pairs");
        job = new_job();
        job.wall_time = Some(Duration::from_hours(7));
        assert_eq!(field(&job), "wall_time");
        job = new_job();
        job.cpu_time = Some(Duration::ZERO);
        assert_eq!(field(&job), "cpu_time");
        job = new_job();
        job.requirements = vec!["OS=linux".into()];
        assert_eq!(field(&job), "requirements");
        job = new_job();
        job.queue_timeout = Some(Duration::from_secs(5));
        assert_eq!(field(&job), "queue_timeout");
    }

    #[test]
    fn the_digest_ignores_order_and_duplicates() {
        let one = check(&new_job(), limits(), Duration::from_hours(1)).unwrap();
        let mut other = new_job();
        other.kinds.reverse();
        other.priority = 9;
        let two = check(&other, limits(), Duration::from_hours(1)).unwrap();
        assert_eq!(spec_digest("t", &one), spec_digest("t", &two));
        assert_ne!(spec_digest("t", &one), spec_digest("u", &one));
    }
}
