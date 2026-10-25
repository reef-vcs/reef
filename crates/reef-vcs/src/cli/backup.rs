//! `reef backup` (R1.18, ADR 0020): a consistent copy of the daemon's state
//! into a new or empty directory, taken while agents keep writing. Progress
//! goes to stderr; the result is the backup's manifest, as the daemon's last
//! progress message carries it.

use std::path::PathBuf;
use std::process::ExitCode;
use std::time::{Duration, Instant};

use reef_api::client::ClientError;
use reef_api::json::BackupManifest;

use super::render::size;
use super::repo::missing;
use super::{CliError, Ctx, RepoArg, absolute};

/// The backup's deadline when `--timeout` is not given: it copies every git
/// object, which takes minutes on a large repository.
const BACKUP_TIMEOUT: Duration = Duration::from_hours(1);

/// `reef backup`.
#[derive(Debug, clap::Args)]
#[command(after_help = "Restore with `reef daemon stop && reef restore <dir> --force`.")]
pub(crate) struct Args {
    /// The directory to write the backup to: new, or empty.
    dir: PathBuf,
    /// Only this repository's jj state (id, id prefix, or path); the
    /// daemon's database is copied whole either way [default: every
    /// repository].
    #[arg(long, value_name = "REPO")]
    repo: Option<String>,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<ExitCode, CliError> {
    let begun = Instant::now();
    let dest = absolute(&args.dir)?;
    let deadline = ctx.timeout_flag.unwrap_or(BACKUP_TIMEOUT);
    let streamed = |repo: String| {
        let dest = &dest;
        async move {
            let mut steps = ctx.client.backup(dest, &repo).await?;
            while let Some(step) = steps.message().await.map_err(ClientError::from)? {
                if step.done {
                    return Ok(step.summary.as_ref().map(BackupManifest::from));
                }
                let what = if step.repo_id.is_empty() {
                    "database".to_owned()
                } else {
                    format!("repo {}", step.repo_id)
                };
                ctx.out.note(&format!(
                    "reef: {what}: {} ({} files, {})",
                    step.step,
                    step.files,
                    size(step.bytes)
                ));
            }
            Ok(None)
        }
    };
    let manifest = match &args.repo {
        Some(repo) => {
            ctx.call_with_repo_within(deadline, &RepoArg::parse(repo)?, streamed)
                .await?
        }
        None => ctx.call_within(deadline, streamed(String::new())).await?,
    };
    let Some(manifest) = manifest else {
        return Err(missing("backup result"));
    };
    let files: u64 = manifest.repos.iter().map(|repo| repo.files).sum();
    let bytes: u64 =
        manifest.repos.iter().map(|repo| repo.bytes).sum::<u64>() + manifest.sqlite.bytes;
    let count = manifest.repos.len();
    let note = format!(
        "backed up the database and {count} {} ({files} files, {}) to {} in {:.1}s",
        if count == 1 {
            "repository"
        } else {
            "repositories"
        },
        size(bytes),
        dest.display(),
        begun.elapsed().as_secs_f64()
    );
    ctx.out.done(&manifest, &note)?;
    Ok(ExitCode::SUCCESS)
}
