//! `reef-fleet`: what five tickets at once cost a standard laptop (#264).
//!
//! ```text
//! reef-fleet run    --repo synthetic|all|<name>,... [--profile dev] [--seed off] [--out DIR] ...
//! reef-fleet report DIR... [--gate]
//! ```
//!
//! `run` runs each repository's scenario against a daemon with its own
//! state directory and writes `<out>/report.json`, `<out>/summary.md` and
//! the ticket logs. `report` prints a run's summary again; with `--gate` it
//! exits 1 when a gate failed. See `docs/fleet-benchmark.md`.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "a CLI's output")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use reef_testing::fleet::report::{self, Report};
use reef_testing::fleet::repos::{self, Manifest, RepoSpec};
use reef_testing::fleet::run::{self, RunConfig};
use reef_testing::fleet::{LimitMode, Profile, SeedMode, Toggle, Variant, sample};

#[derive(Parser)]
#[command(about = "Measure what five tickets at once cost a standard laptop (#264)")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run the scenario on each repository.
    Run(Box<RunArgs>),
    /// Print a run's summary; with --gate, exit 1 if a gate failed.
    Report(ReportArgs),
}

#[derive(Args)]
#[allow(clippy::struct_excessive_bools, reason = "independent flags")]
struct RunArgs {
    /// Repositories, comma-separated: names from repos.toml, `all` (every
    /// one in it), or `synthetic` (generated, offline, about a minute).
    #[arg(long, value_delimiter = ',', default_value = "synthetic")]
    repo: Vec<String>,
    /// The machine the run stands for; only `standard` and `linux` runs are
    /// judged.
    #[arg(long, value_enum, default_value_t = Profile::Dev)]
    profile: Profile,
    /// The machine-wide build limit (#271): `off` passes `--no-build-limit`
    /// to every `ws run` (the #264 baseline); `both` runs each repository
    /// off, then on, and G8 compares them.
    #[arg(long, value_enum, default_value_t = LimitMode::On)]
    limit: LimitMode,
    /// How each of the five tickets' workspaces starts (the spike, E1-E3).
    #[arg(long, value_enum, default_value_t = SeedMode::Off)]
    seed: SeedMode,
    /// Add KEY=VALUE to every repository's build environment, replacing
    /// its own value (the spike's variants, such as `GOFLAGS=-trimpath`).
    #[arg(long = "env", value_name = "KEY=VALUE")]
    env: Vec<String>,
    /// Declare and build the repository's `reef.toml` stores first.
    #[arg(long, value_enum, default_value_t = Toggle::Off)]
    stores: Toggle,
    /// Tickets per repository (each needs its patches).
    #[arg(long, default_value_t = 5)]
    tickets: u32,
    /// How many tickets also run alone first, as the correctness reference.
    #[arg(long, default_value_t = 5)]
    solo_tickets: u32,
    /// How many times solo ticket 1 runs; the report takes the median (3
    /// for a gated `--limit both` run).
    #[arg(long, default_value_t = 1)]
    solo_runs: u32,
    /// How long the five stay idle before release, seconds.
    #[arg(long, default_value_t = 300)]
    idle_secs: u64,
    /// The five tickets start within this many milliseconds.
    #[arg(long, default_value_t = 1000)]
    stagger_ms: u64,
    /// The directory holding the `reef` and `reef-daemon` to measure
    /// [default: the one `reef` is in on PATH].
    #[arg(long)]
    reef_bin_dir: Option<PathBuf>,
    /// Reef's own checkout, for the `reef` repository.
    #[arg(long, default_value = ".")]
    reef_checkout: PathBuf,
    /// A repos.toml to use instead of the shipped one.
    #[arg(long)]
    manifest: Option<PathBuf>,
    /// The patches' root [default: the shipped `fleet/patches`].
    #[arg(long)]
    patches: Option<PathBuf>,
    /// Where the run's daemon state goes; keep the path short (the socket
    /// lives there) [default: the temporary directory's `reef-fleet`].
    #[arg(long)]
    work_root: Option<PathBuf>,
    /// Where clones are kept between runs [default: the work root's `repos`].
    #[arg(long)]
    cache: Option<PathBuf>,
    /// Where the report goes.
    #[arg(long, default_value = "fleet-report")]
    out: PathBuf,
    /// Keep the run's state directory afterwards.
    #[arg(long)]
    keep_state: bool,
    /// The run's daemon's `[disk] floor` (#272), such as `0` on a machine
    /// whose free space is under the default; the daemon's default when
    /// absent.
    #[arg(long)]
    disk_floor: Option<String>,
    /// Clone and set up the repositories, check their patches, and stop.
    #[arg(long)]
    setup_only: bool,
    /// Exit 1 if a gate failed.
    #[arg(long)]
    gate: bool,
}

#[derive(Args)]
struct ReportArgs {
    /// Run directories (holding report.json).
    #[arg(required = true)]
    dirs: Vec<PathBuf>,
    /// Exit 1 if a gate failed in any of them.
    #[arg(long)]
    gate: bool,
    /// Re-read each run's log and recompute outcomes and summaries, then
    /// rewrite report.json and summary.md (after a parser fix).
    #[arg(long)]
    reparse: bool,
}

fn main() -> Result<ExitCode> {
    match Cli::parse().command {
        Command::Run(args) => run(*args),
        Command::Report(args) => report(&args),
    }
}

fn run(args: RunArgs) -> Result<ExitCode> {
    let manifest = match &args.manifest {
        Some(path) => Manifest::parse(
            &std::fs::read_to_string(path).with_context(|| path.display().to_string())?,
        ),
        None => Manifest::shipped(),
    }
    .map_err(anyhow::Error::msg)?;
    let selected = select(&manifest, &args.repo)?;
    let reef_bin_dir = match args.reef_bin_dir {
        Some(dir) => absolute(&dir)?,
        None => repos::which("reef")
            .and_then(|p| p.parent().map(Path::to_path_buf))
            .context("no `reef` on PATH; pass --reef-bin-dir")?,
    };
    for binary in ["reef", "reef-daemon"] {
        if !reef_bin_dir.join(binary).is_file() {
            bail!("{} has no {binary}", reef_bin_dir.display());
        }
    }
    let work_root = args
        .work_root
        .unwrap_or_else(|| std::env::temp_dir().join("reef-fleet"));
    let work_root = absolute(&work_root)?;
    let caches = work_root.join("caches");
    std::fs::create_dir_all(&caches).with_context(|| caches.display().to_string())?;
    let mut extra = std::collections::BTreeMap::new();
    for pair in &args.env {
        let (key, value) = pair
            .split_once('=')
            .with_context(|| format!("--env {pair}: not KEY=VALUE"))?;
        extra.insert(key.to_owned(), value.to_owned());
    }
    let repos = selected
        .into_iter()
        .map(|mut spec| {
            spec.env.extend(extra.clone());
            repos::with_caches(spec, &caches)
        })
        .collect();
    let config = RunConfig {
        reef_bin_dir,
        repos,
        patches_root: match args.patches {
            Some(dir) => absolute(&dir)?,
            None => Path::new(env!("CARGO_MANIFEST_DIR")).join("fleet/patches"),
        },
        reef_checkout: absolute(&args.reef_checkout)?,
        cache: match args.cache {
            Some(dir) => absolute(&dir)?,
            None => work_root.join("repos"),
        },
        work_root,
        out: absolute(&args.out)?,
        profile: args.profile,
        variant: Variant {
            limit: if args.limit == LimitMode::Off {
                Toggle::Off
            } else {
                Toggle::On
            },
            seed: args.seed,
            stores: args.stores,
        },
        tickets: args.tickets,
        solo_tickets: args.solo_tickets,
        solo_runs: args.solo_runs,
        limits: args.limit,
        idle: Duration::from_secs(args.idle_secs),
        stagger: Duration::from_millis(args.stagger_ms),
        keep_state: args.keep_state,
        disk_floor: args.disk_floor,
    };
    if args.setup_only {
        run::setup_only(&config).map_err(anyhow::Error::msg)?;
        println!("fleet: set up {} repositories", config.repos.len());
        return Ok(ExitCode::SUCCESS);
    }
    if let Some(why) = run::profile_mismatch(
        config.profile,
        sample::cores(),
        host_memory(),
        std::env::consts::OS,
    ) {
        eprintln!("fleet: warning: {why}");
    }
    let report = run::run(&config).map_err(anyhow::Error::msg)?;
    print!("{}", report::render(&report));
    eprintln!("fleet: wrote {}", config.out.join("report.json").display());
    Ok(exit(args.gate && report::any_failed(&report)))
}

fn report(args: &ReportArgs) -> Result<ExitCode> {
    let mut failed = false;
    for dir in &args.dirs {
        let path = dir.join("report.json");
        let text = std::fs::read_to_string(&path).with_context(|| path.display().to_string())?;
        let mut report: Report =
            serde_json::from_str(&text).with_context(|| path.display().to_string())?;
        if report.schema != report::SCHEMA {
            bail!(
                "{}: schema {} (this build reads {})",
                path.display(),
                report.schema,
                report::SCHEMA
            );
        }
        if args.reparse {
            report::reparse(&mut report, dir);
            run::write(&report, dir).map_err(anyhow::Error::msg)?;
        }
        print!("{}", report::render(&report));
        failed |= report::any_failed(&report);
    }
    Ok(exit(args.gate && failed))
}

fn exit(failed: bool) -> ExitCode {
    if failed {
        eprintln!("fleet: a gate failed");
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

/// The repositories `names` asks for, in the manifest's order.
fn select(manifest: &Manifest, names: &[String]) -> Result<Vec<RepoSpec>> {
    let mut out = Vec::new();
    for name in names {
        match name.as_str() {
            repos::SYNTHETIC => out.push(repos::synthetic()),
            "all" => out.extend(manifest.repos.iter().cloned()),
            name => match manifest.repos.iter().find(|r| r.name == name) {
                Some(repo) => out.push(repo.clone()),
                None => bail!(
                    "no repository `{name}`; repos.toml has {}, plus `synthetic` and `all`",
                    manifest
                        .repos
                        .iter()
                        .map(|r| r.name.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ),
            },
        }
    }
    Ok(out)
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| path.display().to_string())
}

fn host_memory() -> u64 {
    if cfg!(target_os = "macos") {
        std::process::Command::new("sysctl")
            .args(["-n", "hw.memsize"])
            .output()
            .ok()
            .and_then(|o| String::from_utf8_lossy(&o.stdout).trim().parse().ok())
            .unwrap_or(0)
    } else {
        std::fs::read_to_string("/proc/meminfo")
            .ok()
            .and_then(|text| {
                text.lines()
                    .find_map(|l| l.strip_prefix("MemTotal:"))
                    .and_then(|v| v.split_whitespace().next()?.parse::<u64>().ok())
            })
            .map_or(0, |kib| kib * 1024)
    }
}
