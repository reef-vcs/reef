//! The agent onboarding benchmark (RX.07): what it costs an agent that has
//! never seen Reef to reach a working `reef ws run`.
//!
//! Each trial sets up a fresh home, state directory, and clone, runs
//! Claude Code headless (`claude -p`) with a fixed prompt through
//! [`crate::agent`], reads its metrics from the transcript ([`stream`]),
//! judges the result from the daemon's records ([`judge`]), and writes one
//! JSON file per trial and a Markdown summary ([`report`]). [`run`] does
//! the setup and the running.
//!
//! The scenarios:
//!
//! - **A**, in-band only: `reef` on `PATH`, a plain git checkout,
//!   `claude --bare`.
//! - **B**, after `reef init`: the same checkout after `reef init --agents
//!   claude`, `claude` without `--bare`, so `CLAUDE.md` and the skill load.
//! - **C**, from nothing: no `reef` on `PATH`; the prompt says where to
//!   install it from.
//! - **D**, oriented: the agent runs inside `reef ws run` and is asked
//!   where it is.
//! - **E**, after the plugin: a plain git checkout, and Reef's Claude Code
//!   plugin installed into the trial's home from the checkout under test
//!   (R2.25), `claude` without `--bare`, so its skill and MCP server load.
//!
//! See `docs/onboarding-benchmark.md`.

use serde::{Deserialize, Serialize};

pub mod judge;
pub mod report;
pub mod run;
pub mod stream;

use judge::Verdict;
use stream::Metrics;

/// One of the benchmark's starting points.
#[derive(
    Clone,
    Copy,
    Debug,
    PartialEq,
    Eq,
    PartialOrd,
    Ord,
    Hash,
    Serialize,
    Deserialize,
    clap::ValueEnum,
)]
pub enum Scenario {
    /// In-band only: `reef` on `PATH`, no Reef configuration.
    #[value(name = "A", alias = "a")]
    A,
    /// After `reef init`: `CLAUDE.md`, `AGENTS.md`, and the skill load.
    #[value(name = "B", alias = "b")]
    B,
    /// From nothing: Reef is not installed.
    #[value(name = "C", alias = "c")]
    C,
    /// Oriented: started by `reef ws run`, asked where it is.
    #[value(name = "D", alias = "d")]
    D,
    /// After the plugin: Reef's Claude Code plugin is installed.
    #[value(name = "E", alias = "e")]
    E,
}

impl Scenario {
    /// Every scenario, in order.
    pub const ALL: [Scenario; 5] = [
        Scenario::A,
        Scenario::B,
        Scenario::C,
        Scenario::D,
        Scenario::E,
    ];

    /// The scenario's letter.
    pub fn letter(self) -> char {
        match self {
            Scenario::A => 'A',
            Scenario::B => 'B',
            Scenario::C => 'C',
            Scenario::D => 'D',
            Scenario::E => 'E',
        }
    }

    /// A few words on what the scenario measures.
    pub fn title(self) -> &'static str {
        match self {
            Scenario::A => "in-band only",
            Scenario::B => "after `reef init`",
            Scenario::C => "from nothing",
            Scenario::D => "oriented",
            Scenario::E => "after the plugin",
        }
    }

    /// The prompt, from `onboarding/prompts/`. B and E share A's: the
    /// scenarios differ in what the agent has been given, not in what it is
    /// told.
    pub fn prompt(self) -> &'static str {
        match self {
            Scenario::A | Scenario::B | Scenario::E => {
                include_str!("../../onboarding/prompts/a.txt")
            }
            Scenario::C => include_str!("../../onboarding/prompts/c.txt"),
            Scenario::D => include_str!("../../onboarding/prompts/d.txt"),
        }
        .trim_end()
    }

    /// Whether Claude Code runs with `--bare` (no `CLAUDE.md`, skills,
    /// hooks, or memory).
    pub fn bare(self) -> bool {
        !matches!(self, Scenario::B | Scenario::E)
    }
}

impl std::fmt::Display for Scenario {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.letter())
    }
}

/// What a trial ran against, so rows compare like with like.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pins {
    /// The model the trial asked for (`--model`).
    pub model: String,
    /// The Claude Code version, from the stream's `init` event, else
    /// `claude --version`.
    pub claude_code_version: Option<String>,
    /// `reef --version`.
    pub reef_version: String,
    /// The commit in `reef --version`, if it names one.
    pub reef_commit: Option<String>,
    /// `linux` or `macos`.
    pub os: String,
    /// The repository the agent worked in: `<url>@<rev>`, or the synthetic
    /// fallback.
    pub repository: String,
    /// The prompt, verbatim.
    pub prompt: String,
}

pub use crate::agent::process::Outcome;

/// One trial, as written to `results/<date>-<scenario>-<trial>.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TrialResult {
    /// The file format's version.
    pub schema: u32,
    /// The scenario.
    pub scenario: Scenario,
    /// 1-based.
    pub trial: u32,
    /// UTC, `YYYY-MM-DD`.
    pub date: String,
    /// UTC, RFC 3339.
    pub started_at: String,
    /// What the trial ran against.
    pub pins: Pins,
    /// How it ended.
    pub outcome: Outcome,
    /// The `result` event's subtype, such as `success` or
    /// `error_max_budget_usd`.
    pub result_subtype: Option<String>,
    /// Whether the run hit `--max-budget-usd`. Such a trial is a failure.
    pub budget_exceeded: bool,
    /// What the stream showed; `None` after a setup failure.
    pub metrics: Option<Metrics>,
    /// The judge's finding; `None` after a setup failure.
    pub verdict: Option<Verdict>,
    /// The overall result: the agent exited, within budget, and the judge
    /// found the work done.
    pub success: bool,
    /// The saved stream, next to this file.
    pub stream_file: Option<String>,
}

/// [`TrialResult::schema`] for files this build writes.
pub const SCHEMA: u32 = 1;

impl TrialResult {
    /// Whether the agent ran, so the trial counts in the medians.
    pub fn agent_ran(&self) -> bool {
        !matches!(self.outcome, Outcome::SetupFailed { .. })
    }
}
