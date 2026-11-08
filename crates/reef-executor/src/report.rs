//! What `reef executor run` and `reef executor check` print with
//! `-o json`: one [`JobReport`] per finished job, a JSON line each, and one
//! [`CheckReport`].

use schemars::JsonSchema;
use serde::Serialize;

/// One job, as the executor finished with it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct JobReport {
    /// The job's id.
    pub job_id: String,
    /// Its repository's id.
    pub repo_id: String,
    /// The attempt this executor ran.
    pub attempt: u32,
    /// How it ended here: `done`, `budget_exhausted` or `failed` (completed
    /// with that outcome); `released` (given back at shutdown); `lost`
    /// (cancelled, lapsed, or its claim refused: nothing more was sent).
    pub outcome: String,
    /// Why, for `failed`, `released` and `lost`.
    pub message: Option<String>,
    /// Summaries that passed.
    pub pass: u32,
    /// Summaries that failed.
    pub fail: u32,
    /// Summaries with no verdict.
    pub error: u32,
    /// Each requested pair, in the spec's order.
    pub pairs: Vec<PairReport>,
    /// From the claim to the end.
    pub elapsed_ms: u64,
    /// CPU time of every check that ran.
    pub cpu_ms: u64,
    /// The sandbox kind.
    pub sandbox: String,
    /// The tree, once materialized.
    pub tree: Option<TreeReport>,
}

/// One requested pair.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PairReport {
    /// The kind.
    pub kind: String,
    /// The target; null for the whole check.
    pub target: Option<String>,
    /// The summary's result: `pass`, `fail` or `error`; null when the job
    /// stopped before it.
    pub result: Option<String>,
    /// The checks that ran for it, by name.
    pub checks: Vec<String>,
    /// Why it is `error` when nothing ran.
    pub note: Option<String>,
}

/// The tree the job ran on.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct TreeReport {
    /// The tree's id.
    pub tree_id: String,
    /// Entries laid out.
    pub entries: u64,
    /// Blobs read from the daemon; the rest came from the cache.
    pub fetched: u64,
    /// Bytes read from the daemon.
    pub fetched_bytes: u64,
    /// `disk` or `listing`.
    pub verify: String,
}

/// What `reef executor check` found.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct CheckReport {
    /// The configuration file read; null for the defaults.
    pub config: Option<String>,
    /// The executor's name.
    pub name: String,
    /// The kinds it claims.
    pub kinds: Vec<String>,
    /// The labels it claims with.
    pub labels: Vec<String>,
    /// `allow` or `deny`: whether the tree's reef-checks.toml is read.
    pub repository_checks: String,
    /// The most network a repository's check gets.
    pub max_repository_network: String,
    /// The work directory.
    pub work_dir: String,
    /// The sandbox.
    pub sandbox: SandboxReport,
    /// The executor's own checks, by name.
    pub checks: Vec<String>,
    /// With `-r`: what each pair would run.
    pub plan: Option<Vec<PlanReport>>,
}

/// The sandbox and its probes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct SandboxReport {
    /// `seatbelt`, `bubblewrap` or `none`.
    pub kind: String,
    /// Whether it can be applied here, and every probe held.
    pub holds: bool,
    /// Why not, when it cannot be applied.
    pub problem: Option<String>,
    /// Each escape probe.
    pub probes: Vec<ProbeReport>,
}

/// One escape probe.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct ProbeReport {
    /// The probe.
    pub name: String,
    /// What it tries.
    pub tries: String,
    /// Whether this platform's sandbox promises to refuse it; false for a
    /// documented limit (macOS lets a process read another's environment).
    pub boundary: bool,
    /// Whether the sandbox refused it: true is what the sandbox promises.
    pub refused: bool,
    /// What happened.
    pub detail: String,
}

/// What one pair would run at a revision.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PlanReport {
    /// The kind.
    pub kind: String,
    /// The target; null for the whole check.
    pub target: Option<String>,
    /// The checks, in the order they would run.
    pub checks: Vec<PlannedReport>,
    /// Why nothing would run, or a note on a lowered network.
    pub note: Option<String>,
}

/// One planned check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, JsonSchema)]
pub struct PlannedReport {
    /// Its name.
    pub name: String,
    /// `executor` or `repository`.
    pub source: String,
    /// The argv, `{target}` substituted.
    pub argv: Vec<String>,
    /// The network it would get.
    pub network: String,
}
