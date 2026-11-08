//! The claim loop (`docs/executor-protocol.md` section 3): claim with a
//! wait, run the job, repeat; `--jobs N` runs N loops.

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use reef_api::v1;
use tokio::sync::watch;

use crate::Error;
use crate::job::{self, Context};
use crate::report::JobReport;

/// The longest wait a claim may ask for.
pub const MAX_WAIT: Duration = Duration::from_mins(1);

/// How `reef executor run` serves.
#[derive(Clone, Debug)]
pub struct Options {
    /// Only these repositories (added to `[executor] repos`).
    pub repos: Vec<String>,
    /// Claim loops, 1 to 16.
    pub jobs: usize,
    /// Claim one job, run it, and stop.
    pub once: bool,
    /// With `once`: how long to wait for a job.
    pub wait: Duration,
    /// Stop after this long with no job.
    pub exit_when_idle: Option<Duration>,
}

/// What a whole run did.
#[derive(Clone, Debug, Default)]
pub struct Served {
    /// Jobs run to an end.
    pub jobs: u32,
}

/// What a claim says about this executor.
pub fn info(ctx: &Context) -> v1::ExecutorInfo {
    let mut labels = vec![
        format!("os={}", std::env::consts::OS),
        format!("arch={}", std::env::consts::ARCH),
        format!("sandbox={}", ctx.backend.kind()),
    ];
    labels.extend(ctx.config.executor.labels.iter().cloned());
    v1::ExecutorInfo {
        name: ctx.name.clone(),
        version: crate::VERSION.to_owned(),
        kinds: ctx.config.kinds(),
        labels,
    }
}

/// Takes the work directory: creates it (mode 0700), locks `<work>/lock`
/// for this process, and removes job directories a crashed run left. The
/// returned lock is held for as long as it lives.
pub fn take_work_dir(work: &Path) -> Result<nix::fcntl::Flock<std::fs::File>, Error> {
    use std::os::unix::fs::{DirBuilderExt as _, PermissionsExt as _};
    let io = |context: String| move |source| Error::Io { context, source };
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(work)
        .map_err(io(format!("creating {}", work.display())))?;
    std::fs::set_permissions(work, std::fs::Permissions::from_mode(0o700))
        .map_err(io(format!("restricting {}", work.display())))?;
    let file = std::fs::OpenOptions::new()
        .create(true)
        .truncate(false)
        .write(true)
        .open(work.join("lock"))
        .map_err(io(format!("opening {}/lock", work.display())))?;
    let lock = nix::fcntl::Flock::lock(file, nix::fcntl::FlockArg::LockExclusiveNonblock).map_err(
        |_| Error::WorkLocked {
            dir: work.display().to_string(),
        },
    )?;
    let jobs = work.join("jobs");
    if let Ok(entries) = std::fs::read_dir(&jobs) {
        for entry in entries.filter_map(Result::ok) {
            job::remove_dir(&entry.path());
        }
    }
    std::fs::create_dir_all(&jobs).map_err(io(format!("creating {}", jobs.display())))?;
    Ok(lock)
}

/// Makes this process's memory unreadable to others of its user where the
/// platform allows, and leaves no core file: the keys and tokens live in
/// it.
#[allow(unsafe_code)]
pub fn harden() {
    let none = libc::rlimit {
        rlim_cur: 0,
        rlim_max: 0,
    };
    // SAFETY: a valid rlimit for this process.
    unsafe {
        libc::setrlimit(libc::RLIMIT_CORE, &raw const none);
    }
    #[cfg(target_os = "linux")]
    // SAFETY: PR_SET_DUMPABLE takes one integer argument.
    unsafe {
        libc::prctl(libc::PR_SET_DUMPABLE, 0, 0, 0, 0);
    }
}

/// Turns `SIGINT` and `SIGTERM` into `shutdown`: the first stops claiming
/// and releases running jobs; a second within five seconds exits at once.
pub fn handle_signals(shutdown: Arc<watch::Sender<bool>>) {
    tokio::spawn(async move {
        use tokio::signal::unix::{SignalKind, signal};
        let (Ok(mut int), Ok(mut term)) = (
            signal(SignalKind::interrupt()),
            signal(SignalKind::terminate()),
        ) else {
            return;
        };
        let first = tokio::select! {
            _ = int.recv() => 130,
            _ = term.recv() => 143,
        };
        tracing::info!("stopping: releasing running jobs (again within 5s to exit now)");
        let _ = shutdown.send(true);
        let at = Instant::now();
        loop {
            let code = tokio::select! {
                _ = int.recv() => 130,
                _ = term.recv() => 143,
            };
            if at.elapsed() < Duration::from_secs(5) {
                std::process::exit(code.max(first));
            }
        }
    });
}

/// Serves until shutdown, `once`, or idle. `on_report` sees each finished
/// job.
pub async fn serve(
    ctx: Arc<Context>,
    options: Options,
    on_report: Arc<dyn Fn(&JobReport) + Send + Sync>,
) -> Result<Served, Error> {
    let loops = options.jobs.clamp(1, 16);
    let mut set = tokio::task::JoinSet::new();
    for _ in 0..loops {
        let ctx = Arc::clone(&ctx);
        let options = options.clone();
        let on_report = Arc::clone(&on_report);
        set.spawn(async move { claim_loop(&ctx, &options, &*on_report).await });
    }
    let mut served = Served::default();
    let mut first_error = None;
    while let Some(done) = set.join_next().await {
        let error = match done {
            Ok(Ok(jobs)) => {
                served.jobs += jobs;
                continue;
            }
            Ok(Err(error)) => error,
            Err(error) => Error::Io {
                context: "a claim loop".into(),
                source: std::io::Error::other(error),
            },
        };
        // A key it cannot get or evidence the daemon refuses would stop
        // every loop the same way: stop them all, releasing their jobs.
        let _ = ctx.stop.send(true);
        first_error.get_or_insert(error);
    }
    match first_error {
        Some(error) => Err(error),
        None => Ok(served),
    }
}

async fn claim_loop(
    ctx: &Context,
    options: &Options,
    on_report: &(dyn Fn(&JobReport) + Send + Sync),
) -> Result<u32, Error> {
    let mut repos = ctx.config.executor.repos.clone();
    repos.extend(options.repos.iter().cloned());
    let info = info(ctx);
    let mut shutdown = ctx.shutdown.clone();
    let mut idle_since = Instant::now();
    let once_until = Instant::now() + options.wait;
    let mut jobs = 0;
    let mut backoff = Duration::from_millis(500);
    loop {
        if *shutdown.borrow() {
            return Ok(jobs);
        }
        let wait = if options.once {
            once_until
                .saturating_duration_since(Instant::now())
                .min(MAX_WAIT)
        } else if let Some(idle) = options.exit_when_idle {
            idle.saturating_sub(idle_since.elapsed()).min(MAX_WAIT)
        } else {
            MAX_WAIT
        };
        let request = v1::ClaimJobRequest {
            executor: Some(info.clone()),
            protocol_version: crate::PROTOCOL_VERSION,
            repos: repos.clone(),
            wait: Some(proto_duration(wait)),
            lease: Some(proto_duration(ctx.config.executor.lease)),
        };
        let claim = ctx.client.claim_job(request);
        tokio::pin!(claim);
        let claimed = tokio::select! {
            claimed = &mut claim => claimed,
            _ = shutdown.changed() => {
                // A claim the daemon made just now would sit for a lease:
                // give a moment for its answer, and give back what came.
                if let Ok(Ok(response)) = tokio::time::timeout(Duration::from_secs(2), &mut claim).await
                    && let Some(job) = response.job
                {
                    let _ = ctx
                        .client
                        .update_job(v1::UpdateJobRequest {
                            job_id: job.job_id,
                            claim_token: response.claim_token,
                            release: true,
                            ..v1::UpdateJobRequest::default()
                        })
                        .await;
                }
                return Ok(jobs);
            }
        };
        match claimed {
            Ok(response) => {
                backoff = Duration::from_millis(500);
                if response.protocol_version > crate::PROTOCOL_VERSION {
                    tracing::debug!(
                        daemon = response.protocol_version,
                        "the daemon speaks a newer protocol; this executor speaks 1"
                    );
                }
                if let Some(job) = response.job {
                    let report = job::run_job(ctx, job, response.claim_token).await?;
                    on_report(&report);
                    jobs += 1;
                    idle_since = Instant::now();
                    if options.once {
                        return Ok(jobs);
                    }
                }
            }
            Err(error) if retryable(&error) => {
                tracing::warn!(%error, "claiming failed; retrying");
                tokio::select! {
                    () = tokio::time::sleep(backoff) => {}
                    _ = shutdown.changed() => return Ok(jobs),
                }
                backoff = (backoff * 2).min(Duration::from_secs(30));
            }
            Err(error) => return Err(error.into()),
        }
        if options.once && Instant::now() >= once_until {
            return Ok(jobs);
        }
        if let Some(idle) = options.exit_when_idle
            && idle_since.elapsed() >= idle
        {
            return Ok(jobs);
        }
    }
}

/// A failure that waiting may fix: the daemon restarting, the network.
fn retryable(error: &reef_client::ClientError) -> bool {
    use reef_error::{Classify as _, ErrorKind};
    matches!(error.kind(), ErrorKind::Unavailable | ErrorKind::Io)
        || matches!(
            error,
            reef_client::ClientError::Connect { .. }
                | reef_client::ClientError::RemoteConnect { .. }
        )
}

fn proto_duration(d: Duration) -> prost_types::Duration {
    prost_types::Duration {
        seconds: i64::try_from(d.as_secs()).unwrap_or(i64::MAX),
        nanos: i32::try_from(d.subsec_nanos()).unwrap_or(0),
    }
}
