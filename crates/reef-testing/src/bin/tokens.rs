//! `reef-tokens`: the token-efficiency benchmark (R2.17).
//!
//! ```text
//! reef-tokens scripted [--question ID,...] [--no-sweep] [--source .] [--reef-bin-dir target/debug] ...
//! reef-tokens live     [--question ID,...] [--arm git,reef,mcp] [--trials 3] [--model ...] ...
//! reef-tokens report   --results <dir>... [--docs docs/token-benchmark.md] [--note TEXT]
//! reef-tokens fixture  [--source .] [--reef-bin-dir target/debug]
//! ```
//!
//! `scripted` builds the fixture, runs every candidate, the MCP fixed cost
//! and the diff sweep, and writes `<out>/<date>-scripted.json`. `live` runs
//! real agents and writes one file per trial. Both write `<out>/latest.md`
//! and print it. A trial that fails is a result, not an error: the exit
//! status is 1 only when the harness itself cannot run. `report` reads
//! result files and prints the summary, or adds their rows to the results
//! page. `fixture` builds a fixture, prints where its parts are, and leaves
//! it (and its daemon) for reading. See `docs/token-benchmark.md`.

#![allow(clippy::print_stdout, clippy::print_stderr, reason = "a CLI's output")]

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Parser, Subcommand, ValueEnum};
use reef_testing::agent::harness::Harness;
use reef_testing::agent::harness::claude::Claude;
use reef_testing::agent::process::output;
use reef_testing::agent::scrub::scrub;
use reef_testing::tokens::Arm;
use reef_testing::tokens::count::{self, Counter};
use reef_testing::tokens::fixture::{Fixture, FixtureConfig};
use reef_testing::tokens::live::{self, LiveConfig, TrialResult};
use reef_testing::tokens::report;
use reef_testing::tokens::scenario::Scenario;
use reef_testing::tokens::scripted::{self, Scripted, ScriptedResult};

#[derive(Parser)]
#[command(
    about = "Measure the tokens an agent needs to answer standard questions with git, the reef CLI, and the reef MCP server"
)]
struct Cli {
    #[command(subcommand)]
    command: Command_,
}

#[derive(Subcommand)]
enum Command_ {
    /// Run the scripted candidates, the fixed cost and the diff sweep.
    /// Free; `ANTHROPIC_API_KEY` adds the `anthropic` counter.
    Scripted(ScriptedArgs),
    /// Run live trials. Needs the harness and its API key; each trial may
    /// spend up to --budget-usd.
    Live(LiveArgs),
    /// Summarize result files, or add their rows to the results page.
    Report(ReportArgs),
    /// Build a fixture and leave it for reading.
    Fixture(FixtureArgs),
}

#[derive(Args, Clone)]
struct FixtureArgs {
    /// A git checkout holding the scenario's commits (a full clone of
    /// Reef).
    #[arg(long, default_value = ".")]
    source: PathBuf,
    /// The directory with the `reef` and `reef-daemon` builds to test.
    #[arg(long, default_value = "target/debug")]
    reef_bin_dir: PathBuf,
    /// Where the fixture goes [default: the temporary directory's
    /// `reef-tokens`].
    #[arg(long)]
    work_root: Option<PathBuf>,
    /// How long to wait for the daemon's indexes, in seconds.
    #[arg(long, default_value_t = 300)]
    settle_secs: u64,
}

#[derive(Args)]
struct ScriptedArgs {
    #[command(flatten)]
    fixture: FixtureArgs,
    /// Questions to run, comma-separated [default: all].
    #[arg(long, value_delimiter = ',')]
    question: Vec<String>,
    /// Skip the diff sweep.
    #[arg(long)]
    no_sweep: bool,
    /// The model the `anthropic` counter counts for.
    #[arg(long, env = "REEF_TOKENS_MODEL", default_value = "claude-sonnet-5-5")]
    model: String,
    /// Where results go.
    #[arg(long, default_value = "crates/reef-testing/tokens/results")]
    out: PathBuf,
    /// Keep the fixture (and its daemon) for reading.
    #[arg(long)]
    keep: bool,
}

/// The harnesses with an adapter.
#[derive(Clone, Copy, ValueEnum)]
enum HarnessName {
    /// Claude Code (`claude -p`).
    #[value(name = "claude-code", alias = "claude")]
    ClaudeCode,
}

#[derive(Args)]
struct LiveArgs {
    #[command(flatten)]
    fixture: FixtureArgs,
    /// The harness.
    #[arg(long, value_enum, default_value = "claude-code")]
    harness: HarnessName,
    /// The harness's executable [default: its usual name on PATH].
    #[arg(long)]
    harness_bin: Option<PathBuf>,
    /// Questions to run, comma-separated [default: all].
    #[arg(long, value_delimiter = ',')]
    question: Vec<String>,
    /// Arms to run, comma-separated.
    #[arg(
        long,
        value_enum,
        value_delimiter = ',',
        default_value = "git,reef,mcp"
    )]
    arm: Vec<Arm>,
    /// Trials per question and arm.
    #[arg(long, default_value_t = 3)]
    trials: u32,
    /// The model, pinned so rows compare like with like.
    #[arg(long, env = "REEF_TOKENS_MODEL", default_value = "claude-sonnet-5-5")]
    model: String,
    /// The most a trial may spend, in US dollars. A trial that hits it
    /// fails.
    #[arg(long, default_value_t = 1.0)]
    budget_usd: f64,
    /// How long a trial may run, in seconds.
    #[arg(long, default_value_t = 300)]
    timeout_secs: u64,
    /// Where results go.
    #[arg(long, default_value = "crates/reef-testing/tokens/results")]
    out: PathBuf,
    /// Keep each trial's directory and the fixture.
    #[arg(long)]
    keep: bool,
}

#[derive(Args)]
struct ReportArgs {
    /// Directories to read `*.json` results from, recursively.
    #[arg(long, required = true, num_args = 1..)]
    results: Vec<PathBuf>,
    /// Add the rows to this results page, above each table's marker.
    #[arg(long)]
    docs: Option<PathBuf>,
    /// Text for the new rows' Notes column.
    #[arg(long, default_value = "")]
    note: String,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Command_::Scripted(args) => scripted(&args),
        Command_::Live(args) => live(&args),
        Command_::Report(args) => report(&args),
        Command_::Fixture(args) => fixture(&args),
    }
}

fn fixture_config(args: &FixtureArgs) -> Result<FixtureConfig> {
    let work_root = args
        .work_root
        .clone()
        .unwrap_or_else(|| std::env::temp_dir().join("reef-tokens"));
    Ok(FixtureConfig {
        source: absolute(&args.source)?,
        reef_bin_dir: absolute(&args.reef_bin_dir)?,
        work_root: absolute(&work_root)?,
        settle_timeout: Duration::from_secs(args.settle_secs),
    })
}

fn build_fixture(config: &FixtureConfig, scenario: &Scenario) -> Result<Fixture> {
    eprintln!(
        "tokens: building the fixture {} from {}",
        Scenario::version(),
        config.source.display()
    );
    Fixture::build(config, scenario).map_err(|e| anyhow!("fixture: {e}"))
}

fn reef_version(config: &FixtureConfig) -> Result<String> {
    let reef = config.reef_bin_dir.join("reef");
    output(Command::new(&reef).arg("--version"))
        .map_err(|e| anyhow!("cannot run {}: {e}", reef.display()))
}

fn scripted(args: &ScriptedArgs) -> Result<()> {
    let scenario = Scenario::load().map_err(anyhow::Error::msg)?;
    let config = fixture_config(&args.fixture)?;
    let version = reef_version(&config)?;
    std::fs::create_dir_all(&args.out)?;
    let cache = absolute(&args.out)?.join("anthropic-counts.json");
    let (mut counters, missing) = count::available(&args.model, Some(&cache));
    for (name, why) in &missing {
        eprintln!("tokens: the `{name}` counter will not run: {why}");
    }
    let mut result = ScriptedResult::new(
        &version,
        &args.model,
        missing
            .iter()
            .map(|(n, w)| ((*n).to_owned(), w.clone()))
            .collect(),
    );
    if let Some(unknown) = args
        .question
        .iter()
        .find(|q| scenario.question(q).is_none())
    {
        bail!("no question `{unknown}`");
    }
    let fixture = build_fixture(&config, &scenario)?;
    let mut failed = Vec::new();
    let run = (|| -> Result<()> {
        let mut run = Scripted {
            scenario: &scenario,
            fixture: &fixture,
            counters: &mut counters,
            out_dir: &args.out,
            failed: Vec::new(),
        };
        eprintln!("tokens: the MCP server's fixed cost");
        result.fixed = run.fixed_cost().map_err(anyhow::Error::msg)?;
        eprintln!("tokens: scripted candidates");
        result.candidates = run.candidates(&args.question).map_err(anyhow::Error::msg)?;
        if !args.no_sweep {
            eprintln!(
                "tokens: the diff sweep ({} commits)",
                scenario.sweep.commits
            );
            // A failed sweep is recorded; the rest of the run stands.
            match run.sweep(&config.source, scenario.sweep.commits) {
                Ok(sweep) => result.sweep = sweep,
                Err(error) => {
                    eprintln!("tokens: the diff sweep failed: {error}");
                    result.sweep_error = Some(error);
                }
            }
        }
        failed = run.failed;
        Ok(())
    })();
    for (name, why) in &failed {
        eprintln!("tokens: the `{name}` counter failed and is left out: {why}");
        result.drop_counter(name, &format!("failed during the run: {why}"));
    }
    fixture.teardown(args.keep);
    if args.keep {
        eprintln!("tokens: kept the fixture at {}", fixture.root.display());
    }
    run?;
    let json = serde_json::to_string_pretty(&result)?;
    let path = scripted::result_path(&args.out, &result.date);
    std::fs::write(&path, scrub(&json) + "\n")
        .with_context(|| format!("writing {}", path.display()))?;
    let summary = scrub(&report::summary(Some(&result), &[], &[]));
    std::fs::write(args.out.join("latest.md"), &summary)?;
    println!("{summary}");
    Ok(())
}

#[expect(clippy::too_many_lines, reason = "setup, the trial loop, the summary")]
fn live(args: &LiveArgs) -> Result<()> {
    let scenario = Scenario::load().map_err(anyhow::Error::msg)?;
    let config = fixture_config(&args.fixture)?;
    let version = reef_version(&config)?;
    let harness: Box<dyn Harness> = match args.harness {
        HarnessName::ClaudeCode => {
            let program = args
                .harness_bin
                .clone()
                .unwrap_or_else(|| PathBuf::from("claude"));
            Box::new(Claude {
                program: which(&program)
                    .with_context(|| format!("cannot find {} on PATH", program.display()))?,
            })
        }
    };
    let harness_version = output(Command::new(harness.program()).arg("--version"))
        .ok()
        .and_then(|v| v.split_whitespace().next().map(str::to_owned));
    std::fs::create_dir_all(&args.out)?;
    let cache = absolute(&args.out)?.join("anthropic-counts.json");
    let mut vendor: Option<Box<dyn Counter>> = match harness.vendor_counter() {
        "anthropic" => count::Anthropic::from_env(&args.model, Some(&cache))
            .map(|c| Box::new(c) as Box<dyn Counter>),
        _ => None,
    };
    if vendor.is_none() {
        eprintln!(
            "tokens: answer tokens will not be counted: no `{}` counter (its API key?)",
            harness.vendor_counter()
        );
    }
    let questions: Vec<_> = scenario
        .questions
        .iter()
        .filter(|q| args.question.is_empty() || args.question.contains(&q.id))
        .collect();
    if questions.is_empty() {
        bail!("no question matches {:?}", args.question);
    }
    let live_config = LiveConfig {
        harness: harness.as_ref(),
        model: args.model.clone(),
        budget_usd: args.budget_usd,
        timeout: Duration::from_secs(args.timeout_secs),
        out_dir: args.out.clone(),
        reef_version: version.clone(),
        harness_version: harness_version.clone(),
        keep: args.keep,
    };
    eprintln!(
        "tokens: {version} with {} {} ({}), {} trial(s) of {} question(s) × {} arm(s)",
        harness.name(),
        harness_version.as_deref().unwrap_or("?"),
        args.model,
        args.trials,
        questions.len(),
        args.arm.len()
    );
    let mut fixture = build_fixture(&config, &scenario)?;
    let mut results: Vec<TrialResult> = Vec::new();
    let run = (|| -> Result<()> {
        for question in &questions {
            for &arm in &args.arm {
                for trial in 1..=args.trials {
                    eprintln!("tokens: {} / {arm}, trial {trial}", question.id);
                    let result = live::run_trial(
                        &live_config,
                        &scenario,
                        &fixture,
                        question,
                        arm,
                        trial,
                        vendor.as_mut().map(|c| c.as_mut() as &mut dyn Counter),
                    )
                    .map_err(anyhow::Error::msg)?;
                    eprintln!("tokens:   {}", describe(&result));
                    let mutated = result.mutated;
                    results.push(result);
                    if mutated {
                        eprintln!("tokens:   the trial changed the fixture; rebuilding it");
                        fixture.teardown(args.keep);
                        if args.keep {
                            eprintln!(
                                "tokens:   kept the changed fixture at {}",
                                fixture.root.display()
                            );
                        }
                        fixture = build_fixture(&config, &scenario)?;
                    }
                }
            }
        }
        Ok(())
    })();
    fixture.teardown(args.keep);
    run?;
    let order: Vec<String> = scenario.questions.iter().map(|q| q.id.clone()).collect();
    let cells = report::live_cells(&results, &order);
    let summary = scrub(&report::summary(None, &results, &cells));
    std::fs::write(args.out.join("latest.md"), &summary)?;
    println!("{summary}");
    Ok(())
}

fn describe(result: &TrialResult) -> String {
    let grade = result
        .grade
        .as_ref()
        .map_or_else(|| format!("{:?}", result.outcome), |g| g.answer.clone());
    format!(
        "{}: {grade}; {} answer bytes, {} tokens, ${:.3}{}",
        if result.correct { "correct" } else { "wrong" },
        result.answer.as_ref().map_or(0, |a| a.bytes),
        result
            .answer
            .as_ref()
            .and_then(|a| a.tokens)
            .map_or_else(|| "?".to_owned(), |t| t.to_string()),
        result.cost_usd.unwrap_or_default(),
        if result.escapes.is_empty() {
            ""
        } else {
            "; escaped"
        },
    )
}

fn report(args: &ReportArgs) -> Result<()> {
    let mut scripted_results = Vec::new();
    let mut trials = Vec::new();
    for dir in &args.results {
        collect(dir, &mut scripted_results, &mut trials)?;
    }
    if scripted_results.is_empty() && trials.is_empty() {
        bail!("no result files under {:?}", args.results);
    }
    // The newest scripted run is the one reported.
    scripted_results.sort_by(|a, b| a.started_at.cmp(&b.started_at));
    let scripted = scripted_results.last();
    trials.sort_by(|a, b| {
        (&a.question, a.arm, &a.started_at).cmp(&(&b.question, b.arm, &b.started_at))
    });
    let scenario = Scenario::load().map_err(anyhow::Error::msg)?;
    let order: Vec<String> = scenario.questions.iter().map(|q| q.id.clone()).collect();
    let cells = report::live_cells(&trials, &order);
    if let Some(docs) = &args.docs {
        let page =
            std::fs::read_to_string(docs).with_context(|| format!("reading {}", docs.display()))?;
        let page =
            report::insert_rows(&page, scripted, &cells, &args.note).map_err(anyhow::Error::msg)?;
        std::fs::write(docs, page).with_context(|| format!("writing {}", docs.display()))?;
    }
    println!("{}", scrub(&report::summary(scripted, &trials, &cells)));
    Ok(())
}

/// Every result under `dir`: scripted runs and live trials, told apart by
/// name.
fn collect(
    dir: &Path,
    scripted: &mut Vec<ScriptedResult>,
    trials: &mut Vec<TrialResult>,
) -> Result<()> {
    for entry in std::fs::read_dir(dir).with_context(|| format!("reading {}", dir.display()))? {
        let path = entry?.path();
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        if path.is_dir() {
            collect(&path, scripted, trials)?;
        } else if name.ends_with("-scripted.json") {
            match serde_json::from_str(&std::fs::read_to_string(&path)?) {
                Ok(result) => scripted.push(result),
                Err(error) => eprintln!("tokens: skipping {}: {error}", path.display()),
            }
        } else if name.contains("-live-")
            && path
                .extension()
                .is_some_and(|ext| ext.eq_ignore_ascii_case("json"))
        {
            match serde_json::from_str(&std::fs::read_to_string(&path)?) {
                Ok(result) => trials.push(result),
                Err(error) => eprintln!("tokens: skipping {}: {error}", path.display()),
            }
        }
    }
    Ok(())
}

fn fixture(args: &FixtureArgs) -> Result<()> {
    let scenario = Scenario::load().map_err(anyhow::Error::msg)?;
    let config = fixture_config(args)?;
    let fixture = build_fixture(&config, &scenario)?;
    println!("root        {}", fixture.root.display());
    println!(
        "state       {} (REEF_STATE_DIR; the daemon is running)",
        fixture.state.display()
    );
    println!("git         {}", fixture.git_root.display());
    for (agent, path) in &fixture.worktrees {
        println!("  {agent:<9} {}", path.display());
    }
    println!(
        "reef        {} ({})",
        fixture.reef_root.display(),
        fixture.repo_id
    );
    for (agent, ws) in &fixture.workspaces {
        println!("  {agent:<9} {} {} {}", ws.id, ws.change, ws.path.display());
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
