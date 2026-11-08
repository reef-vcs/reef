//! One claimed job, end to end: a session key, the tree, the plan, each
//! check in its sandbox, the evidence, and the completion.

use std::collections::VecDeque;
use std::ffi::OsString;
use std::os::unix::fs::OpenOptionsExt as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime};

use reef_api::v1::{self, JobOutcome};
use reef_client::{Client, ClientError, SessionKey};
use reef_evidence::session::Actor;
use reef_evidence::{Artifact, Claim, Outcome, TreeDigest};
use tokio::sync::{Notify, watch};

use crate::Error;
use crate::config::{Config, RepoChecks, RepositoryChecks};
use crate::declare::{self, PairPlan, Planned, TreeChecks};
use crate::materialize::{self, BlobCache, MaterializeError};
use crate::report::{JobReport, PairReport, TreeReport};
use crate::result::{self, CheckRun};
use crate::run::{self, Ended, StopReason};
use crate::sandbox::{Backend, Policy};

/// Kept back from a job's wall time for uploads and the completion: 30
/// seconds, or a quarter of a shorter job's.
const RESERVE: Duration = Duration::from_secs(30);
/// A session key is renewed when less than this is left of it.
const RENEW_BEFORE: Duration = Duration::from_mins(10);
/// Evidence objects per `UpdateJob` or `CompleteJob`.
const PER_CALL: usize = 50;
/// Upload attempts per artifact.
const UPLOAD_TRIES: u32 = 3;

/// The host's paths a check must never see, beyond the home.
#[derive(Clone, Debug, Default)]
pub struct Hidden {
    /// The user's home, by `HOME` and by the password database: hidden,
    /// except `[sandbox] read`.
    pub homes: Vec<PathBuf>,
    /// Reef's state and configuration, the daemon's socket, the runtime
    /// directory, the executor's configuration and token file: hidden even
    /// under a readable path.
    pub denied: Vec<PathBuf>,
}

/// What every job of one executor shares.
pub struct Context {
    /// The daemon.
    pub client: Client,
    /// The configuration.
    pub config: Arc<Config>,
    /// The sandbox.
    pub backend: Backend,
    /// The verified blobs.
    pub cache: BlobCache,
    /// What checks never see.
    pub hidden: Hidden,
    /// The executor's name.
    pub name: String,
    /// Whether job directories are kept.
    pub keep_work: bool,
    /// Becomes true when the executor shuts down.
    pub shutdown: watch::Receiver<bool>,
    /// Shuts the executor down: a signal, or a claim loop's fatal error.
    pub stop: Arc<watch::Sender<bool>>,
}

impl Context {
    /// `<work>/jobs`.
    pub fn jobs_dir(&self) -> PathBuf {
        self.config.work_dir.join("jobs")
    }
}

/// Runs `job`, claimed with `token`, to its end. `Err` is a reason to stop
/// the executor (a key it cannot get, evidence the daemon refuses); every
/// other end is in the report.
pub async fn run_job(ctx: &Context, job: v1::Job, token: String) -> Result<JobReport, Error> {
    let started = Instant::now();
    let spec = job.spec.clone().unwrap_or_default();
    let mut report = JobReport {
        job_id: job.job_id.clone(),
        repo_id: job.repo_id.clone(),
        attempt: job.attempt,
        outcome: String::new(),
        message: None,
        pass: 0,
        fail: 0,
        error: 0,
        pairs: Vec::new(),
        elapsed_ms: 0,
        cpu_ms: 0,
        sandbox: ctx.backend.kind().to_owned(),
        tree: None,
    };
    // Per attempt: a lapsed attempt's directory may still be here.
    let dir = ctx
        .jobs_dir()
        .join(format!("{}-{}", job.job_id, job.attempt));
    let outcome = Box::pin(work(ctx, &job, &spec, &token, &dir, &mut report)).await;
    if !ctx.keep_work {
        let remove = dir.clone();
        let _ = tokio::task::spawn_blocking(move || remove_dir(&remove)).await;
    }
    if let Err(error) = ctx.cache.evict().await {
        tracing::warn!(%error, "evicting the blob cache");
    }
    report.elapsed_ms = millis(started.elapsed());
    match outcome {
        Ok(end) => {
            report.outcome = end.name.to_owned();
            report.message = end.message;
            Ok(report)
        }
        Err(error) => Err(error),
    }
}

/// How a job ended here.
struct End {
    name: &'static str,
    message: Option<String>,
}

impl End {
    fn new(name: &'static str, message: Option<String>) -> End {
        End { name, message }
    }
}

#[allow(clippy::too_many_lines)]
async fn work(
    ctx: &Context,
    job: &v1::Job,
    spec: &v1::JobSpec,
    token: &str,
    dir: &Path,
    report: &mut JobReport,
) -> Result<End, Error> {
    let claimed = Instant::now();
    let lease = ctx.config.executor.lease;
    // The job's stop: the uplink sends Cancelled when the claim is lost,
    // the executor Shutdown when it stops.
    let (stop_tx, stop_rx) = watch::channel(None::<StopReason>);
    let stop_tx = Arc::new(stop_tx);
    let forward = {
        let stop_tx = Arc::clone(&stop_tx);
        let mut shutdown = ctx.shutdown.clone();
        tokio::spawn(async move {
            while !*shutdown.borrow() {
                if shutdown.changed().await.is_err() {
                    return;
                }
            }
            stop_tx.send_if_modified(|stop| {
                if stop.is_none() {
                    *stop = Some(StopReason::Shutdown);
                    true
                } else {
                    false
                }
            });
        })
    };
    let uplink = Uplink::start(
        ctx.client.clone(),
        job.job_id.clone(),
        token.to_owned(),
        lease,
        Arc::clone(&stop_tx),
    );
    let ended = Box::pin(steps(
        ctx, job, spec, dir, report, &uplink, stop_rx, claimed,
    ))
    .await;
    forward.abort();
    let (state, lost) = uplink.close().await;
    if let Some(error) = state.fatal {
        let _ = release(&ctx.client, &job.job_id, token, Vec::new()).await;
        return Err(error);
    }
    if let Some(why) = lost {
        return Ok(End::new("lost", Some(why)));
    }
    let pending: Vec<v1::SignedEvidence> = state.pending.into_iter().map(|(_, e)| e).collect();
    let step = match ended {
        Err(error) => {
            // A key that cannot be issued or renewed: give the job back.
            let _ = release(&ctx.client, &job.job_id, token, pending).await;
            return Err(error);
        }
        Ok(step) => step,
    };
    let usage = v1::JobUsage {
        wall_time_ms: millis(claimed.elapsed()),
        cpu_time_ms: report.cpu_ms,
    };
    let (outcome, message, name) = match step {
        Step::Shutdown => {
            return match release(&ctx.client, &job.job_id, token, pending).await {
                Ok(()) => Ok(End::new("released", Some("the executor stopped".into()))),
                Err(error) => Ok(End::new("lost", Some(format!("releasing: {error}")))),
            };
        }
        Step::Lost(why) => return Ok(End::new("lost", Some(why))),
        Step::Done => (JobOutcome::Done, String::new(), "done"),
        Step::Budget(why) => (JobOutcome::BudgetExhausted, why, "budget_exhausted"),
        Step::Failed(why) => (JobOutcome::Failed, why, "failed"),
    };
    match complete(
        &ctx.client,
        &job.job_id,
        token,
        outcome,
        pending,
        usage,
        &message,
    )
    .await
    {
        Ok(()) => Ok(End::new(name, (!message.is_empty()).then_some(message))),
        Err(Completion::Lost(why)) => Ok(End::new("lost", Some(why))),
        Err(Completion::Refused(error)) => Err(*error),
    }
}

/// Where the job's steps stopped.
enum Step {
    /// Every pair has its summary.
    Done,
    /// The wall time or CPU time ran out.
    Budget(String),
    /// The job could not be run at all.
    Failed(String),
    /// The executor is stopping: give the job back.
    Shutdown,
    /// The claim is gone: send nothing more.
    Lost(String),
}

#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn steps(
    ctx: &Context,
    job: &v1::Job,
    spec: &v1::JobSpec,
    dir: &Path,
    report: &mut JobReport,
    uplink: &Uplink,
    stop: watch::Receiver<Option<StopReason>>,
    claimed: Instant,
) -> Result<Step, Error> {
    let stopped = |stop: &watch::Receiver<Option<StopReason>>| -> Option<Step> {
        match *stop.borrow() {
            Some(StopReason::Shutdown) => Some(Step::Shutdown),
            Some(_) => Some(Step::Lost(
                "the claim was lost: cancelled, lapsed or refused".into(),
            )),
            None => None,
        }
    };
    // The key, before any work.
    let mut keys = Keys::issue(ctx, &job.repo_id).await?;
    let pairs = declare::pairs(&spec.kinds, &spec.targets);
    let wall = spec
        .budget
        .as_ref()
        .and_then(|b| b.wall_time)
        .map_or(Duration::from_hours(1), duration_of);
    let cpu_budget = spec
        .budget
        .as_ref()
        .and_then(|b| b.cpu_time)
        .map(duration_of)
        .filter(|d| !d.is_zero());
    let job_deadline =
        tokio::time::Instant::from_std(claimed) + wall.saturating_sub(RESERVE.min(wall / 4));

    // The tree.
    uplink.progress("materialize", 0, 0, "blobs");
    if let Err(error) = std::fs::DirBuilder::new()
        .recursive(true)
        .create(dir)
        .and_then(|()| {
            use std::os::unix::fs::PermissionsExt as _;
            std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        })
    {
        return Ok(Step::Failed(format!("the job's directory: {error}")));
    }
    for sub in ["home", "tmp", "out"] {
        if let Err(error) = std::fs::create_dir(dir.join(sub)) {
            return Ok(Step::Failed(format!("the job's directory: {error}")));
        }
    }
    let dir = crate::sandbox::real_path(dir);
    let progress = |done: u64, total: u64| uplink.progress("materialize", done, total, "blobs");
    let tree = match materialize::materialize(
        &ctx.client,
        &job.repo_id,
        spec,
        &dir.join("tree"),
        &ctx.cache,
        ctx.config.materialize.verify,
        &progress,
    )
    .await
    {
        Ok(tree) => tree,
        Err(MaterializeError::Daemon(error)) if lost_claim(&error) => {
            return Ok(Step::Lost(error.to_string()));
        }
        Err(error) => {
            if let Some(step) = stopped(&stop) {
                return Ok(step);
            }
            return Ok(Step::Failed(error.to_string()));
        }
    };
    report.tree = Some(TreeReport {
        tree_id: spec.tree_id.clone(),
        entries: tree.entries as u64,
        fetched: tree.fetched as u64,
        fetched_bytes: tree.fetched_bytes,
        verify: tree.verify.as_str().to_owned(),
    });
    if let Some(step) = stopped(&stop) {
        return Ok(step);
    }

    // The plan.
    let tree_checks = if ctx.config.repository.checks == RepositoryChecks::Deny {
        TreeChecks::Denied
    } else {
        match &tree.checks_file {
            None => TreeChecks::Absent,
            Some(bytes) => match RepoChecks::parse(bytes) {
                Ok(checks) => TreeChecks::Parsed(checks),
                Err(message) => TreeChecks::Invalid(message),
            },
        }
    };
    let plans = declare::plan(&ctx.config, &tree_checks, &pairs);
    report.pairs = plans
        .iter()
        .map(|plan| PairReport {
            kind: plan.pair.kind.clone(),
            target: plan.pair.target.clone(),
            result: None,
            checks: plan.checks.iter().map(|p| p.check.name.clone()).collect(),
            note: plan.note.clone(),
        })
        .collect();
    let context = result::Context {
        executor: ctx.name.clone(),
        job: job.job_id.clone(),
        attempt: job.attempt,
        sandbox: ctx.backend.kind(),
        verify: tree.verify.as_str(),
        repository_checks: tree_checks.digest().map(str::to_owned),
    };
    let tree_digest = TreeDigest::git(&spec.tree_id).map_err(|error| Error::Config {
        message: format!("the job's tree id: {error}"),
    })?;

    // Each pair, in the spec's order.
    let total = plans.len() as u64;
    let mut cpu = Duration::ZERO;
    let mut serial = 0usize;
    for (index, plan) in plans.iter().enumerate() {
        if let Some(step) = stopped(&stop) {
            return Ok(step);
        }
        if let Some(budget) = cpu_budget
            && cpu >= budget
        {
            return Ok(Step::Budget(format!(
                "CPU time ran out after {index} of {total} pairs"
            )));
        }
        if tokio::time::Instant::now() >= job_deadline {
            return Ok(Step::Budget(format!(
                "wall time ran out after {index} of {total} pairs"
            )));
        }
        uplink.progress(&phase(&plan.pair), index as u64, total, "pairs");
        let mut runs = Vec::new();
        let mut budget_hit = false;
        for planned in &plan.checks {
            if let Some(budget) = cpu_budget
                && cpu >= budget
            {
                // Mid-pair: no summary would cover every check, so none is
                // sent for this pair.
                return Ok(Step::Budget(format!(
                    "CPU time ran out during {}",
                    plan.pair.label()
                )));
            }
            serial += 1;
            let run = Box::pin(run_check(
                ctx,
                job,
                spec,
                &dir,
                plan,
                planned,
                serial,
                job_deadline,
                stop.clone(),
            ))
            .await;
            let run = match run {
                Ok(run) => run,
                Err(error) => {
                    return Ok(Step::Failed(format!(
                        "running {}: {error}",
                        planned.check.name
                    )));
                }
            };
            cpu += run.ran.cpu;
            report.cpu_ms = millis(cpu);
            let stopped_why = match run.ran.ended {
                Ended::Stopped(why) => Some(why),
                _ => None,
            };
            runs.push(run);
            match stopped_why {
                Some(StopReason::Budget) => {
                    budget_hit = true;
                    break;
                }
                Some(StopReason::Cancelled | StopReason::Shutdown) => {
                    if let Some(step) = stopped(&stop) {
                        return Ok(step);
                    }
                }
                _ => {}
            }
        }
        if let Some(step) = stopped(&stop) {
            return Ok(step);
        }
        let claims = result::claims(
            &context,
            &plan.pair,
            &runs,
            plan.note.as_deref(),
            SystemTime::now(),
        );
        let summary = claims[0].result;
        report.pairs[index].result = Some(summary.as_str().to_owned());
        match summary {
            Outcome::Pass => report.pass += 1,
            Outcome::Fail => report.fail += 1,
            Outcome::Error => report.error += 1,
        }
        keys.renew_if_needed(ctx).await?;
        for claim in claims {
            let label = claim_label(&claim);
            uplink.push(label, sign(&claim, &tree_digest, keys.current())?);
        }
        uplink.flush();
        if budget_hit {
            return Ok(Step::Budget(format!(
                "wall time ran out during {}",
                plan.pair.label()
            )));
        }
    }
    uplink.progress("done", total, total, "pairs");
    Ok(Step::Done)
}

/// Runs one planned check and uploads what it left.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
async fn run_check(
    ctx: &Context,
    job: &v1::Job,
    spec: &v1::JobSpec,
    dir: &Path,
    plan: &PairPlan,
    planned: &Planned,
    serial: usize,
    job_deadline: tokio::time::Instant,
    stop: watch::Receiver<Option<StopReason>>,
) -> std::io::Result<CheckRun> {
    let check = &planned.check;
    let tree = dir.join("tree");
    let cache_dir = check.cache.then(|| {
        ctx.config
            .work_dir
            .join("caches")
            .join(&job.repo_id)
            .join(&check.name)
    });
    if let Some(cache) = &cache_dir {
        std::fs::create_dir_all(cache)?;
    }
    let cache_dir = cache_dir.map(|c| crate::sandbox::real_path(&c));
    let path_var = std::env::var_os("PATH").unwrap_or_else(|| "/usr/bin:/bin".into());
    let target = plan.pair.target.as_deref();
    let mut env: Vec<(OsString, OsString)> = vec![
        ("PATH".into(), path_var.clone()),
        ("TERM".into(), "dumb".into()),
        ("HOME".into(), dir.join("home").into()),
        ("TMPDIR".into(), dir.join("tmp").into()),
        ("REEF_CHECK_JOB".into(), job.job_id.clone().into()),
        ("REEF_CHECK_KIND".into(), plan.pair.kind.clone().into()),
        (
            "REEF_CHECK_TARGET".into(),
            target.unwrap_or_default().into(),
        ),
        ("REEF_CHECK_TREE".into(), spec.tree_id.clone().into()),
        ("REEF_CHECK_NAME".into(), check.name.clone().into()),
    ];
    for name in ["LANG", "LC_ALL", "TZ"] {
        if let Some(value) = std::env::var_os(name) {
            env.push((name.into(), value));
        }
    }
    if let Some(cache) = &cache_dir {
        env.push(("REEF_CHECK_CACHE".into(), cache.into()));
    }
    for (name, value) in ctx.config.sandbox_env() {
        if !ctx.config.sandbox.pass_env.contains(&name) || std::env::var_os(&name).is_none() {
            env.push((name.into(), value.into()));
        }
    }
    for name in &ctx.config.sandbox.pass_env {
        if let Some(value) = std::env::var_os(name) {
            env.push((name.into(), value));
        }
    }
    for (name, value) in &check.env {
        env.push((
            name.into(),
            value.render(target, cache_dir.as_deref()).into(),
        ));
    }
    let mut writable = vec![tree.clone(), dir.join("home"), dir.join("tmp")];
    writable.extend(cache_dir.iter().cloned());
    let mut hidden: Vec<PathBuf> = ctx.hidden.homes.clone();
    hidden.push(crate::sandbox::real_path(&ctx.config.work_dir));
    let policy = Policy {
        cwd: tree.clone(),
        writable,
        readable: ctx.config.read_paths(),
        hidden,
        denied: ctx
            .hidden
            .denied
            .iter()
            .cloned()
            .chain(ctx.config.deny_paths())
            .collect(),
        network: planned.network,
        env,
    };
    let log_name = format!("{serial:03}-{}.log", check.name);
    let log = dir.join("out").join(&log_name);
    let started = SystemTime::now();
    let ran = match ctx.backend.command(&policy, &planned.argv) {
        Ok(command) => {
            let now = tokio::time::Instant::now();
            let deadline = match check.timeout {
                Some(timeout) if now + timeout < job_deadline => {
                    (now + timeout, StopReason::Timeout)
                }
                _ => (job_deadline, StopReason::Budget),
            };
            run::run(run::Spec {
                command,
                program: &planned.argv[0],
                path: &path_var,
                log: &log,
                max_log: ctx.config.run.max_log,
                deadline: Some(deadline),
                stop,
                kill_grace: ctx.config.run.kill_grace,
                sweep: dir.to_owned(),
                seatbelt: matches!(ctx.backend, Backend::Seatbelt { .. }),
            })
            .await?
        }
        Err(error) => {
            std::fs::write(&log, format!("reef executor: {error}\n"))?;
            run::Ran {
                ended: Ended::NotStarted(error.to_string()),
                wall: Duration::ZERO,
                cpu: Duration::ZERO,
                logged: 0,
                dropped: 0,
            }
        }
    };
    let finished = SystemTime::now();
    let mut artifacts = Vec::new();
    let mut notes = Vec::new();
    match upload(
        ctx,
        &job.repo_id,
        &log,
        &log_name,
        Some("text/plain"),
        u64::MAX,
    )
    .await
    {
        Ok(artifact) => artifacts.push(artifact),
        Err(why) => notes.push(format!("log {log_name}: {why}")),
    }
    for (n, relative) in check.artifacts.iter().enumerate() {
        let copy = dir.join("out").join(format!("{serial:03}-{n}-artifact"));
        match copy_declared(&tree, relative, &copy, ctx.config.run.max_artifact) {
            Ok(path) => match upload(
                ctx,
                &job.repo_id,
                &path,
                relative,
                media_type(relative),
                ctx.config.run.max_artifact,
            )
            .await
            {
                Ok(artifact) => artifacts.push(artifact),
                Err(why) => notes.push(format!("artifact {relative}: {why}")),
            },
            Err(why) => notes.push(format!("artifact {relative}: {why}")),
        }
    }
    Ok(CheckRun {
        planned: planned.clone(),
        ran,
        started,
        finished,
        artifacts,
        notes,
    })
}

/// Copies a declared artifact out of the tree into `copy` (in the job's
/// `out/`, which no check can reach), opening each component with
/// `O_NOFOLLOW` from the one before: a link anywhere on the way, a FIFO, or
/// anything but a regular file is refused, and a process that outlived the
/// check cannot swap a component for a link between the check and the read.
fn copy_declared(tree: &Path, relative: &str, copy: &Path, max: u64) -> Result<PathBuf, String> {
    use nix::fcntl::{OFlag, openat};
    use nix::sys::stat::Mode;
    let components: Vec<&str> = relative.split('/').collect();
    let (last, parents) = components
        .split_last()
        .ok_or_else(|| "an empty path".to_owned())?;
    let directory = OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_NOFOLLOW | OFlag::O_CLOEXEC;
    let mut dir: std::os::fd::OwnedFd =
        std::fs::File::open(tree).map_err(|e| e.to_string())?.into();
    for component in parents {
        dir = openat(&dir, *component, directory, Mode::empty())
            .map_err(|_| "not written by the check, or a link on the way".to_owned())?;
    }
    let file = openat(
        &dir,
        *last,
        OFlag::O_RDONLY | OFlag::O_NOFOLLOW | OFlag::O_NONBLOCK | OFlag::O_CLOEXEC,
        Mode::empty(),
    )
    .map_err(|_| "not written by the check, or a symbolic link".to_owned())?;
    let mut file = std::fs::File::from(file);
    let meta = file.metadata().map_err(|e| e.to_string())?;
    if !meta.is_file() {
        return Err("not a regular file".into());
    }
    if meta.len() > max {
        return Err(format!(
            "{} bytes, over [run] max_artifact ({max})",
            meta.len()
        ));
    }
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(copy)
        .map_err(|e| e.to_string())?;
    let copied = std::io::copy(&mut std::io::Read::take(&mut file, max + 1), &mut out)
        .map_err(|e| e.to_string())?;
    if copied > max {
        return Err(format!("over [run] max_artifact ({max}) while it was read"));
    }
    Ok(copy.to_owned())
}

fn media_type(name: &str) -> Option<&'static str> {
    let extension = name.rsplit('.').next()?;
    Some(match extension {
        "xml" => "application/xml",
        "json" => "application/json",
        "txt" | "log" => "text/plain",
        "html" => "text/html",
        _ => return None,
    })
}

/// Uploads `path` by the digest computed here; the artifact to name.
async fn upload(
    ctx: &Context,
    repo: &str,
    path: &Path,
    name: &str,
    media: Option<&str>,
    max: u64,
) -> Result<Artifact, String> {
    let file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW)
        .open(path)
        .map_err(|error| error.to_string())?;
    let size = file.metadata().map_err(|e| e.to_string())?.len();
    if size > max {
        return Err(format!("{size} bytes, over [run] max_artifact ({max})"));
    }
    let digest = tokio::task::spawn_blocking(move || reef_evidence::sha256_hex_of(&file))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let artifact = Artifact {
        name: result::truncate(name, 256),
        sha256: digest.clone(),
        uri: None,
        media_type: media.map(str::to_owned),
    };
    let mut last = String::new();
    for attempt in 0..UPLOAD_TRIES {
        match ctx
            .client
            .upload_artifact_file_as(repo, path, Some(&digest))
            .await
        {
            Ok(_) => return Ok(artifact),
            Err(error) => {
                last = error.to_string();
                tokio::time::sleep(Duration::from_millis(200 << attempt)).await;
            }
        }
    }
    Err(format!("not uploaded after {UPLOAD_TRIES} tries: {last}"))
}

/// A pair as a progress phase: `kind` or `kind:target`, visible ASCII of at
/// most 64 bytes, as the daemon requires.
fn phase(pair: &declare::Pair) -> String {
    let text = match &pair.target {
        Some(target) => format!("{}:{target}", pair.kind),
        None => pair.kind.clone(),
    };
    let visible: String = text
        .chars()
        .map(|c| if c.is_ascii_graphic() { c } else { '_' })
        .collect();
    result::truncate(&visible, 64)
}

fn claim_label(claim: &Claim) -> String {
    match &claim.target {
        Some(target) => format!("{} {target}", claim.kind),
        None => claim.kind.clone(),
    }
}

fn sign(claim: &Claim, tree: &TreeDigest, key: &SessionKey) -> Result<v1::SignedEvidence, Error> {
    let envelope = claim
        .sign(tree, &key.signing_key)
        .map_err(|error| Error::Config {
            message: format!("evidence the executor built is invalid: {error}"),
        })?;
    Ok(v1::SignedEvidence {
        envelope: envelope.to_bytes(),
        public_key: key.signing_key.public_key().spki().to_vec(),
    })
}

/// The job's session: its keys, renewed within the session.
struct Keys {
    current: SessionKey,
    identity: String,
}

impl Keys {
    async fn issue(ctx: &Context, _repo: &str) -> Result<Keys, Error> {
        let identity = ctx.config.executor.identity.clone();
        let current = ctx
            .client
            .issue_session_key(&identity, Actor::Executor, None, "")
            .await
            .map_err(|e| Error::Key(Box::new(e)))?;
        Ok(Keys { current, identity })
    }

    fn current(&self) -> &SessionKey {
        &self.current
    }

    /// A new key in the same session when less than ten minutes remain.
    async fn renew_if_needed(&mut self, ctx: &Context) -> Result<(), Error> {
        let Some(not_after) = self.current.record.not_after else {
            return Ok(());
        };
        let not_after = SystemTime::UNIX_EPOCH
            + Duration::from_secs(u64::try_from(not_after.seconds).unwrap_or(0));
        if SystemTime::now() + RENEW_BEFORE < not_after {
            return Ok(());
        }
        let session = self.current.record.session_id.clone();
        self.current = ctx
            .client
            .issue_session_key(&self.identity, Actor::Executor, None, &session)
            .await
            .map_err(|e| Error::Key(Box::new(e)))?;
        Ok(())
    }
}

/// Heartbeats, progress and evidence, sent in the background.
struct Uplink {
    state: Arc<Mutex<UplinkState>>,
    notify: Arc<Notify>,
    task: tokio::task::JoinHandle<Option<String>>,
}

#[derive(Default)]
struct UplinkState {
    progress: Option<v1::JobProgress>,
    pending: VecDeque<(String, v1::SignedEvidence)>,
    fatal: Option<Error>,
    closing: bool,
}

impl Uplink {
    fn start(
        client: Client,
        job_id: String,
        token: String,
        lease: Duration,
        stop: Arc<watch::Sender<Option<StopReason>>>,
    ) -> Uplink {
        let state = Arc::new(Mutex::new(UplinkState::default()));
        let notify = Arc::new(Notify::new());
        let task = tokio::spawn(heartbeat(
            client,
            job_id,
            token,
            lease,
            stop,
            Arc::clone(&state),
            Arc::clone(&notify),
        ));
        Uplink {
            state,
            notify,
            task,
        }
    }

    /// Records progress; a new phase is sent at once (the heartbeat keeps
    /// to one call a second), a count within a phase with the next one.
    fn progress(&self, phase: &str, done: u64, total: u64, unit: &str) {
        let phase = result::truncate(phase, 64);
        let mut changed = false;
        if let Ok(mut state) = self.state.lock() {
            changed = state.progress.as_ref().is_none_or(|p| p.phase != phase);
            state.progress = Some(v1::JobProgress {
                phase,
                done,
                total,
                unit: unit.to_owned(),
            });
        }
        if changed {
            self.notify.notify_one();
        }
    }

    fn push(&self, label: String, evidence: v1::SignedEvidence) {
        if let Ok(mut state) = self.state.lock() {
            state.pending.push_back((label, evidence));
        }
    }

    fn flush(&self) {
        self.notify.notify_one();
    }

    /// Stops the heartbeats; what is left to send, and why the claim was
    /// lost, if it was.
    async fn close(self) -> (UplinkState, Option<String>) {
        if let Ok(mut state) = self.state.lock() {
            state.closing = true;
        }
        self.notify.notify_one();
        let lost = self.task.await.unwrap_or(None);
        let state = self
            .state
            .lock()
            .map(|mut s| std::mem::take(&mut *s))
            .unwrap_or_default();
        (state, lost)
    }
}

/// The heartbeat loop: every third of the lease, or when evidence is
/// ready. Returns why the claim was lost, if it was.
async fn heartbeat(
    client: Client,
    job_id: String,
    token: String,
    lease: Duration,
    stop: Arc<watch::Sender<Option<StopReason>>>,
    state: Arc<Mutex<UplinkState>>,
    notify: Arc<Notify>,
) -> Option<String> {
    let every = lease / 3;
    let mut last_ok = Instant::now();
    let mut last_call: Option<Instant> = None;
    let mut backoff = Duration::from_millis(250);
    loop {
        tokio::select! {
            () = tokio::time::sleep(every) => {}
            () = notify.notified() => {}
        }
        // At most one call a second (docs/executor-protocol.md section 7):
        // what arrives meanwhile goes in the next. Closing does not wait
        // for it: what is left goes in the completion.
        if let Some(at) = last_call {
            let until = tokio::time::Instant::from_std(at + Duration::from_secs(1));
            loop {
                if state.lock().map_or(true, |s| s.closing) {
                    break;
                }
                tokio::select! {
                    () = tokio::time::sleep_until(until) => break,
                    // Only closing ends the wait early.
                    () = notify.notified() => {}
                }
            }
        }
        last_call = Some(Instant::now());
        let (progress, batch) = {
            let Ok(mut state) = state.lock() else {
                return None;
            };
            if state.closing {
                return None;
            }
            let take = state.pending.len().min(PER_CALL);
            let batch: Vec<(String, v1::SignedEvidence)> = state.pending.drain(..take).collect();
            (state.progress.clone(), batch)
        };
        let request = v1::UpdateJobRequest {
            job_id: job_id.clone(),
            claim_token: token.clone(),
            progress,
            evidence: batch.iter().map(|(_, e)| e.clone()).collect(),
            release: false,
        };
        match client.update_job(request).await {
            Ok(_) => {
                last_ok = Instant::now();
                backoff = Duration::from_millis(250);
                let more = state.lock().is_ok_and(|s| !s.pending.is_empty());
                if more {
                    notify.notify_one();
                }
            }
            Err(error) if lost_claim(&error) => {
                cancel(&stop);
                return Some(error.to_string());
            }
            Err(error) if refused_evidence(&error) => {
                let which = batch
                    .first()
                    .map_or_else(String::new, |(label, _)| label.clone());
                if let Ok(mut state) = state.lock() {
                    state.fatal = Some(Error::EvidenceRefused {
                        evidence: which,
                        error: Box::new(error),
                    });
                }
                cancel(&stop);
                return None;
            }
            Err(error) => {
                // Put the batch back and retry until the lease would lapse.
                if let Ok(mut state) = state.lock() {
                    for item in batch.into_iter().rev() {
                        state.pending.push_front(item);
                    }
                }
                if last_ok.elapsed() >= lease {
                    cancel(&stop);
                    return Some(format!(
                        "the daemon was unreachable for the whole lease: {error}"
                    ));
                }
                tracing::warn!(%error, job = %job_id, "a heartbeat failed; retrying");
                tokio::time::sleep(backoff).await;
                backoff = (backoff * 2).min(every.max(Duration::from_secs(1)));
                notify.notify_one();
            }
        }
    }
}

fn cancel(stop: &watch::Sender<Option<StopReason>>) {
    stop.send_if_modified(|current| {
        if current.is_none() {
            *current = Some(StopReason::Cancelled);
            true
        } else {
            false
        }
    });
}

/// The claim is gone: cancelled, lapsed, finished, or the job forgotten.
fn lost_claim(error: &ClientError) -> bool {
    use reef_api::error as reason;
    [
        reason::JOB_NOT_CLAIMED,
        reason::JOB_NOT_FOUND,
        reason::JOB_FINISHED,
        reason::JOB_NOT_OWNED,
    ]
    .contains(&error.reason())
}

/// The daemon refused what this executor sent: evidence, progress, a
/// message. An executor bug, which every later job would repeat.
fn refused_evidence(error: &ClientError) -> bool {
    use reef_api::error as reason;
    [
        reason::JOB_EVIDENCE_MISMATCH,
        reason::INVALID_EVIDENCE,
        reason::INVALID_JOB_UPDATE,
    ]
    .contains(&error.reason())
}

/// Sends `pending` and gives the claim back.
async fn release(
    client: &Client,
    job_id: &str,
    token: &str,
    mut pending: Vec<v1::SignedEvidence>,
) -> Result<(), ClientError> {
    loop {
        let rest = pending.split_off(pending.len().min(PER_CALL));
        let last = rest.is_empty();
        let request = v1::UpdateJobRequest {
            job_id: job_id.to_owned(),
            claim_token: token.to_owned(),
            progress: None,
            evidence: std::mem::replace(&mut pending, rest),
            release: last,
        };
        client.update_job(request).await?;
        if last {
            return Ok(());
        }
    }
}

enum Completion {
    Lost(String),
    Refused(Box<Error>),
}

/// Sends what is left and completes the job, retrying a failed connection.
async fn complete(
    client: &Client,
    job_id: &str,
    token: &str,
    outcome: JobOutcome,
    mut pending: Vec<v1::SignedEvidence>,
    usage: v1::JobUsage,
    message: &str,
) -> Result<(), Completion> {
    // All but the last call's worth through UpdateJob.
    while pending.len() > PER_CALL {
        let rest = pending.split_off(PER_CALL);
        let batch = std::mem::replace(&mut pending, rest);
        let request = v1::UpdateJobRequest {
            job_id: job_id.to_owned(),
            claim_token: token.to_owned(),
            progress: None,
            evidence: batch,
            release: false,
        };
        retry(|| client.update_job(request.clone())).await?;
    }
    let request = v1::CompleteJobRequest {
        job_id: job_id.to_owned(),
        claim_token: token.to_owned(),
        outcome: outcome.into(),
        evidence: pending,
        usage: Some(usage),
        message: result::truncate(message, 1024),
    };
    retry(|| client.complete_job(request.clone())).await?;
    Ok(())
}

async fn retry<T, F, Fut>(mut call: F) -> Result<T, Completion>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T, ClientError>>,
{
    let mut wait = Duration::from_millis(250);
    let mut tries = 0;
    loop {
        match call().await {
            Ok(value) => return Ok(value),
            Err(error) if lost_claim(&error) => return Err(Completion::Lost(error.to_string())),
            Err(error) if refused_evidence(&error) => {
                return Err(Completion::Refused(Box::new(Error::EvidenceRefused {
                    evidence: "the completion's".into(),
                    error: Box::new(error),
                })));
            }
            Err(error) if tries >= 5 => {
                return Err(Completion::Lost(format!("completing: {error}")));
            }
            Err(error) => {
                tracing::warn!(%error, "completing a job failed; retrying");
                tries += 1;
                tokio::time::sleep(wait).await;
                wait *= 2;
            }
        }
    }
}

fn duration_of(d: prost_types::Duration) -> Duration {
    Duration::from_secs(u64::try_from(d.seconds).unwrap_or(0))
        + Duration::from_nanos(u64::try_from(d.nanos).unwrap_or(0))
}

fn millis(d: Duration) -> u64 {
    u64::try_from(d.as_millis()).unwrap_or(u64::MAX)
}

/// Removes a job's directory, making read-only directories a check left
/// writable first. Nothing is followed through a link: each directory is
/// opened with `O_NOFOLLOW` and changed through its descriptor.
pub fn remove_dir(dir: &Path) {
    if std::fs::remove_dir_all(dir).is_ok() || !dir.exists() {
        return;
    }
    let mut stack = vec![dir.to_owned()];
    while let Some(current) = stack.pop() {
        let Ok(handle) = std::fs::OpenOptions::new()
            .read(true)
            .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW)
            .open(&current)
        else {
            continue;
        };
        let _ = nix::sys::stat::fchmod(&handle, nix::sys::stat::Mode::from_bits_truncate(0o700));
        if let Ok(entries) = std::fs::read_dir(&current) {
            for entry in entries.filter_map(Result::ok) {
                if entry.file_type().is_ok_and(|t| t.is_dir()) {
                    stack.push(entry.path());
                }
            }
        }
    }
    let _ = std::fs::remove_dir_all(dir);
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    #[test]
    fn removing_a_job_never_follows_a_link_out() {
        let outside = tempfile::tempdir().unwrap();
        let victim = outside.path().join("victim");
        std::fs::create_dir(&victim).unwrap();
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o500)).unwrap();
        let job = tempfile::tempdir().unwrap();
        let dir = job.path().join("job");
        let locked = dir.join("tree/locked");
        std::fs::create_dir_all(&locked).unwrap();
        std::fs::write(locked.join("file"), "x").unwrap();
        std::fs::set_permissions(&locked, std::fs::Permissions::from_mode(0o500)).unwrap();
        std::os::unix::fs::symlink(&victim, dir.join("tree/link")).unwrap();
        remove_dir(&dir);
        assert!(!dir.exists());
        let mode = std::fs::metadata(&victim).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o500, "the link's target was not changed");
        std::fs::set_permissions(&victim, std::fs::Permissions::from_mode(0o700)).unwrap();
    }
}
