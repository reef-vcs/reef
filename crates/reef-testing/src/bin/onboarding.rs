//! `reef-onboarding`: the agent onboarding benchmark (RX.07).
//!
//! ```text
//! reef-onboarding run    [--scenario A,B,C,D] [--trials 3] [--reef-bin-dir target/debug] ...
//! reef-onboarding report --results <dir>... [--docs docs/onboarding-benchmark.md] [--note TEXT]
//! ```
//!
//! `run` runs each scenario's trials one after another, writes
//! `<out>/<date>-<scenario>-<trial>.json` with the saved stream and stderr
//! beside it, then `<out>/latest.md`, and prints that summary. A trial that
//! fails is a result, not an error: the exit status is 1 only when the
//! harness itself cannot run. `report` reads result files (CI's artifacts)
//! and prints the summary, or adds the rows to the results page. See
//! `docs/onboarding-benchmark.md`.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "a CLI's output")]

use std::fmt::Write as _;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use reef_testing::onboarding::run::{
    self, DEFAULT_REPOSITORY, DEFAULT_REVISION, RepoSource, RunConfig,
};
use reef_testing::onboarding::{Scenario, TrialResult, report};

#[derive(Parser)]
#[command(about = "Measure what it costs an agent new to Reef to reach a working `reef ws run`")]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Run trials. Needs `claude` and `ANTHROPIC_API_KEY`; each trial may
    /// spend up to --budget-usd.
    Run(RunArgs),
    /// Summarize result files, or add their rows to the results page.
    Report(ReportArgs),
}

#[derive(Args)]
struct RunArgs {
    /// Scenarios to run, comma-separated.
    #[arg(long, value_enum, value_delimiter = ',', default_value = "A,B,C,D")]
    scenario: Vec<Scenario>,
    /// Trials per scenario.
    #[arg(long, default_value_t = 3)]
    trials: u32,
    /// The model, pinned so rows compare like with like.
    #[arg(
        long,
        env = "REEF_ONBOARDING_MODEL",
        default_value = "claude-sonnet-5-5"
    )]
    model: String,
    /// The directory with the `reef` and `reef-daemon` builds to test.
    #[arg(long, default_value = "target/debug")]
    reef_bin_dir: PathBuf,
    /// Claude Code's executable.
    #[arg(long, default_value = "claude")]
    claude: PathBuf,
    /// The Reef checkout, for scenario C's installer mirror.
    #[arg(long, default_value = ".")]
    source_root: PathBuf,
    /// Where results go.
    #[arg(long, default_value = "crates/reef-testing/onboarding/results")]
    out: PathBuf,
    /// The repository the agent works in.
    #[arg(long, default_value = DEFAULT_REPOSITORY)]
    repository: String,
    /// The commit to reset the clone to.
    #[arg(long, default_value = DEFAULT_REVISION)]
    revision: String,
    /// Use a synthetic repository instead of cloning (no network for the
    /// repository; the agent still needs the API).
    #[arg(long)]
    offline: bool,
    /// How long an agent may run, in seconds.
    #[arg(long, default_value_t = 600)]
    timeout_secs: u64,
    /// `--max-budget-usd` per trial. A trial that hits it fails.
    #[arg(long, default_value_t = 2.0)]
    budget_usd: f64,
    /// Where trial directories go [default: the temporary directory's
    /// `reef-onboarding`].
    #[arg(long)]
    work_root: Option<PathBuf>,
    /// Keep each trial's directory (home, state, repository) for reading.
    #[arg(long)]
    keep: bool,
}

#[derive(Args)]
struct ReportArgs {
    /// Directories to read `*.json` results from, recursively.
    #[arg(long, required = true, num_args = 1..)]
    results: Vec<PathBuf>,
    /// Add the rows to this results page, above each scenario's marker.
    #[arg(long)]
    docs: Option<PathBuf>,
    /// Text for the new rows' Notes column.
    #[arg(long, default_value = "")]
    note: String,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command::Run(args) => run(args),
        Command::Report(args) => report(&args),
    }
}

fn run(args: RunArgs) -> Result<()> {
    if std::env::var_os("ANTHROPIC_API_KEY").is_none() {
        eprintln!(
            "onboarding: warning: ANTHROPIC_API_KEY is not set; `claude --bare` (scenarios A, C, D) has no other credential"
        );
    }
    let claude = which(&args.claude)
        .with_context(|| format!("cannot find {} on PATH", args.claude.display()))?;
    let work_root = args
        .work_root
        .unwrap_or_else(|| std::env::temp_dir().join("reef-onboarding"));
    let config = RunConfig {
        claude,
        model: args.model,
        reef_bin_dir: absolute(&args.reef_bin_dir)?,
        source_root: absolute(&args.source_root)?,
        repository: if args.offline {
            RepoSource::Synthetic
        } else {
            RepoSource::Clone {
                url: args.repository,
                rev: args.revision,
            }
        },
        timeout: Duration::from_secs(args.timeout_secs),
        budget_usd: args.budget_usd,
        out_dir: args.out,
        work_root: absolute(&work_root)?,
        keep: args.keep,
    };
    let mut scenarios = args.scenario;
    scenarios.sort();
    scenarios.dedup();
    let prepared = run::prepare(&config, &scenarios).map_err(anyhow::Error::msg)?;
    eprintln!(
        "onboarding: {} with {} (Claude Code {}), {} trial(s) of {}",
        prepared.reef_version,
        config.model,
        prepared.claude_version.as_deref().unwrap_or("?"),
        args.trials,
        scenarios
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join(", "),
    );

    let mut results = Vec::new();
    for &scenario in &scenarios {
        for trial in 1..=args.trials {
            eprintln!(
                "onboarding: scenario {scenario} ({}), trial {trial}",
                scenario.title()
            );
            let result =
                run::run_trial(&config, &prepared, scenario, trial).map_err(anyhow::Error::msg)?;
            eprintln!(
                "onboarding:   {}: {}",
                if result.success { "success" } else { "failure" },
                describe(&result)
            );
            results.push(result);
        }
    }
    run::finish(&config);
    let summary = run::scrub(&report::summary(&results));
    std::fs::write(config.out_dir.join("latest.md"), &summary)
        .with_context(|| format!("writing {}", config.out_dir.join("latest.md").display()))?;
    println!("{summary}");
    Ok(())
}

fn describe(result: &TrialResult) -> String {
    if let Some(verdict) = &result.verdict {
        let mut text = verdict.reason.clone();
        if result.budget_exceeded {
            text.push_str("; hit the budget");
        }
        if let Some(metrics) = &result.metrics {
            let _ = write!(
                text,
                "; {} tool calls, {} failed reef calls",
                metrics.tool_calls, metrics.failed_reef_calls
            );
        }
        text
    } else {
        format!("{:?}", result.outcome)
    }
}

fn report(args: &ReportArgs) -> Result<()> {
    let mut results = Vec::new();
    for dir in &args.results {
        collect(dir, &mut results)?;
    }
    if results.is_empty() {
        bail!("no result files under {:?}", args.results);
    }
    results.sort_by(|a, b| (a.scenario, &a.started_at).cmp(&(b.scenario, &b.started_at)));
    let Some(docs) = &args.docs else {
        println!("{}", run::scrub(&report::summary(&results)));
        return Ok(());
    };
    let mut rows = report::rows(&results);
    for row in &mut rows {
        row.note.clone_from(&args.note);
    }
    let page =
        std::fs::read_to_string(docs).with_context(|| format!("reading {}", docs.display()))?;
    let page = report::insert_rows(&page, &rows).map_err(anyhow::Error::msg)?;
    std::fs::write(docs, page).with_context(|| format!("writing {}", docs.display()))?;
    for row in &rows {
        println!("{}", row.markdown());
    }
    Ok(())
}

/// Every trial result under `dir`: `*.json` files that parse as one, past
/// the judge's inputs (`*.workspaces.json`).
fn collect(dir: &Path, results: &mut Vec<TrialResult>) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        if path.is_dir() {
            collect(&path, results)?;
        } else if path.extension().is_some_and(|ext| ext == "json")
            && !path.to_string_lossy().ends_with(".workspaces.json")
        {
            let text = std::fs::read_to_string(&path)?;
            match serde_json::from_str::<TrialResult>(&text) {
                Ok(result) => results.push(result),
                Err(error) => eprintln!("onboarding: skipping {}: {error}", path.display()),
            }
        }
    }
    Ok(())
}

fn absolute(path: &Path) -> Result<PathBuf> {
    std::path::absolute(path).with_context(|| format!("resolving {}", path.display()))
}

/// `program` as an absolute path: itself if it has a slash, else the first
/// match on `PATH`.
fn which(program: &Path) -> Option<PathBuf> {
    if program.components().count() > 1 {
        return absolute(program).ok();
    }
    std::env::split_paths(&std::env::var_os("PATH")?)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
}
