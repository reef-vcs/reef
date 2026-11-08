//! `reef policy` (R3.07, ADR 0061): a repository's `reef-policy.toml`,
//! checked offline, evaluated over a change by the daemon, and started
//! from a template.
//!
//! `check` needs no daemon: it loads a file with `reef-policy`, and with
//! `--input` evaluates it over an input as `reef schema policy input`
//! describes. `eval` asks the daemon (`EvaluatePolicy`), which reads the
//! policy from `trunk()` unless told otherwise, never from the change.
//! `simulate` (R3.08) asks the daemon (`SimulatePolicy`) how a proposed
//! policy would have judged recent landings, page by page.

use std::io::{self, Write};
use std::path::{Path, PathBuf};

use reef_api::v1::{self, EvaluatePolicyRequest, ReadFileRequest};
use reef_policy::{
    Case, ConflictDecision, Decision, Diagnostic, Evaluation, InputSummary, Need, Policy, RuleKind,
    RuleResult, SignoffNeed, Status,
};
use schemars::JsonSchema;
use serde::Serialize;

use super::out::{Out, TextRender};
use super::scope::{self, ScopeArgs};
use super::{CliError, Ctx};
use reef_error::{Advice, ErrorKind};

/// `reef policy`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    #[command(subcommand)]
    command: PolicyCommand,
}

#[derive(Debug, clap::Subcommand)]
enum PolicyCommand {
    /// Check a policy file without a daemon: its keys, references, patterns and CEL; --input evaluates it.
    #[command(after_help = "Example: reef policy check reef-policy.toml --input in.json")]
    Check(CheckArgs),
    /// Evaluate the repository's policy over a change: the decision, and what is missing.
    #[command(after_help = "Example: reef policy eval -r @ --require-allow")]
    Eval(EvalArgs),
    /// Judge recent landings with a proposed policy: what it would have blocked, and why.
    #[command(
        after_help = "Example: reef policy simulate --policy-file reef-policy.toml --limit 50"
    )]
    Simulate(SimulateArgs),
    /// Write a starter reef-policy.toml here: this machine's executor must pass `test`.
    #[command(after_help = "Example: reef policy init --stdout")]
    Init(InitArgs),
}

/// `reef policy check`.
#[derive(Debug, clap::Args)]
struct CheckArgs {
    /// The policy file; reef-policy.toml in this directory when not given.
    #[arg(value_name = "FILE", conflicts_with = "revision")]
    file: Option<PathBuf>,
    /// Check the reef-policy.toml at this revision instead, read through the daemon.
    #[arg(short = 'r', long = "revision", value_name = "REV")]
    revision: Option<String>,
    /// Also evaluate it over this input (JSON; - for stdin), as `reef schema policy input` describes.
    #[arg(long, value_name = "FILE")]
    input: Option<PathBuf>,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef policy eval`.
#[derive(Debug, clap::Args)]
struct EvalArgs {
    /// The change judged [default: the workspace's change].
    #[arg(short = 'r', long = "revision", value_name = "REV")]
    revision: Option<String>,
    /// Read reef-policy.toml at this revision instead of `trunk()`.
    #[arg(long, value_name = "REV", conflicts_with = "policy_file")]
    policy_revision: Option<String>,
    /// Evaluate this file's text instead of a revision's policy.
    #[arg(long, value_name = "FILE")]
    policy_file: Option<PathBuf>,
    /// Print the input the policy saw, as JSON (for `reef policy check --input`).
    #[arg(long)]
    input_json: bool,
    /// Exit 5 unless the decision is allow.
    #[arg(long)]
    require_allow: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef policy simulate`.
#[derive(Debug, clap::Args)]
struct SimulateArgs {
    /// The proposed policy file; reef-policy.toml in this directory when neither this nor --policy-revision is given.
    #[arg(long, value_name = "FILE", conflicts_with = "policy_revision")]
    policy_file: Option<PathBuf>,
    /// Simulate the reef-policy.toml at this revision instead, such as a change that edits it.
    #[arg(long, value_name = "REV")]
    policy_revision: Option<String>,
    /// The landings, a revset read newest first; every non-merge ancestor of `trunk()` when not given.
    #[arg(short = 'r', long = "revisions", value_name = "REVSET")]
    revisions: Option<String>,
    /// The most landings to judge, newest first: 1 to 1000.
    #[arg(long, value_name = "N", default_value_t = 20, value_parser = clap::value_parser!(u32).range(1..=1000))]
    limit: u32,
    /// Compare with the reef-policy.toml at this revision instead of `trunk()`.
    #[arg(long, value_name = "REV", conflicts_with = "no_baseline")]
    baseline_revision: Option<String>,
    /// Judge with the proposed policy alone, with no baseline.
    #[arg(long)]
    no_baseline: bool,
    /// List every landing in text, not only those the proposed policy blocks.
    #[arg(long)]
    all: bool,
    /// Include each landing's full evaluations in JSON.
    #[arg(long)]
    evaluations: bool,
    #[command(flatten)]
    scope: ScopeArgs,
}

/// `reef policy init`.
#[derive(Debug, clap::Args)]
struct InitArgs {
    /// Print the starter file instead of writing it.
    #[arg(long)]
    stdout: bool,
}

impl Args {
    /// Whether the command needs no daemon: `check` of a file. (`init`
    /// asks one for the default identity's name, and names a placeholder
    /// when that call fails.)
    pub(crate) fn is_offline(&self) -> bool {
        matches!(&self.command, PolicyCommand::Check(args) if args.revision.is_none())
    }

    /// Runs an offline command.
    pub(crate) fn run_offline(self, out: &Out) -> Result<(), CliError> {
        match self.command {
            PolicyCommand::Check(args) => {
                let path = args
                    .file
                    .clone()
                    .unwrap_or_else(|| PathBuf::from(reef_policy::POLICY_PATH));
                let bytes = std::fs::read(&path).map_err(|source| CliError::Io {
                    op: "read",
                    path: path.clone(),
                    source,
                })?;
                check(
                    out,
                    &path.display().to_string(),
                    &bytes,
                    args.input.as_deref(),
                )
            }
            _ => Ok(()),
        }
    }
}

/// What `reef policy check` prints.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PolicyCheck {
    /// The file checked, as named, or `<rev>:reef-policy.toml`.
    pub(crate) path: String,
    /// The SHA-256 of its bytes.
    pub(crate) sha256: String,
    /// Its format version.
    pub(crate) version: u32,
    /// Its rules: the built-in ones first, then in file order.
    pub(crate) rules: Vec<String>,
    /// With `--input`, the evaluation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) evaluation: Option<Evaluation>,
    /// With a conflict `--input`, the tier.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) conflict: Option<ConflictDecision>,
}

/// Which policy an evaluation used.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub(crate) struct PolicySource {
    /// The policy revision as requested; empty for a file's text.
    pub(crate) revision: String,
    /// The commit it resolved to.
    pub(crate) commit_id: String,
    /// `reef-policy.toml`, or the file named.
    pub(crate) path: String,
    /// The file's blob id at that commit.
    pub(crate) blob_id: String,
    /// The SHA-256 of the bytes evaluated; empty for no file.
    pub(crate) sha256: String,
    /// The format version read.
    pub(crate) version: u32,
    /// The policy came from `--policy-file`.
    pub(crate) from_request: bool,
}

/// What `reef policy eval` prints.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PolicyEvaluated {
    /// Which policy.
    pub(crate) source: PolicySource,
    /// The change judged.
    pub(crate) change_id: String,
    /// Its commit.
    pub(crate) commit_id: String,
    /// Its tree; empty when conflicted.
    pub(crate) tree_id: String,
    /// The operation it was read at.
    pub(crate) at_operation: String,
    /// The evaluation.
    #[serde(flatten)]
    pub(crate) evaluation: Evaluation,
    /// With `--input-json`, the input the policy saw.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) input_json: Option<serde_json::Value>,
}

/// What `reef policy init` prints.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PolicyInit {
    /// The file written.
    pub(crate) path: String,
    /// The identity named as owner and attestor.
    pub(crate) owner: String,
    /// The owner is a placeholder: no daemon answered with a default
    /// identity.
    pub(crate) placeholder: bool,
}

/// The brief view of an evaluation: the decision, and each rule that
/// blocks with the evidence kinds it misses.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BriefEvaluation<'a> {
    decision: Decision,
    sha256: &'a str,
    blocking: Vec<BriefRule<'a>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct BriefRule<'a> {
    name: &'a str,
    status: Status,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    missing: Vec<&'a str>,
}

impl<'a> BriefEvaluation<'a> {
    pub(crate) fn of(evaluation: &'a Evaluation) -> BriefEvaluation<'a> {
        BriefEvaluation {
            decision: evaluation.decision,
            sha256: &evaluation.sha256,
            blocking: evaluation
                .rules
                .iter()
                .filter(|rule| matches!(rule.status, Status::Unsatisfied | Status::Error))
                .map(|rule| BriefRule {
                    name: &rule.name,
                    status: rule.status,
                    missing: rule.missing.iter().map(|need| need.kind.as_str()).collect(),
                })
                .collect(),
        }
    }
}

fn decision_word(decision: Decision) -> &'static str {
    match decision {
        Decision::Allow => "allow",
        Decision::Block => "block",
        Decision::NoPolicy => "no policy",
        Decision::InvalidPolicy => "invalid policy",
    }
}

fn status_word(status: Status) -> &'static str {
    match status {
        Status::Satisfied => "satisfied",
        Status::Unsatisfied => "unsatisfied",
        Status::NotApplicable => "not applicable",
        Status::Error => "error",
    }
}

/// The rules that are not satisfied, in text.
fn render_rules(w: &mut dyn Write, evaluation: &Evaluation) -> io::Result<()> {
    let mut rest = 0;
    for rule in &evaluation.rules {
        if !matches!(rule.status, Status::Unsatisfied | Status::Error) {
            rest += 1;
            continue;
        }
        writeln!(w, "{}: {}", rule.name, status_word(rule.status))?;
        if !rule.message.is_empty() {
            writeln!(w, "  {}", rule.message)?;
        }
        let need = |need: &Need| {
            let target = if need.target.is_empty() {
                String::new()
            } else {
                format!(" ({})", need.target)
            };
            format!("{}{target} from {}", need.kind, need.attestors.join(" or "))
        };
        for missing in &rule.missing {
            writeln!(w, "  missing: {}", need(missing))?;
        }
        for failed in &rule.failed {
            writeln!(
                w,
                "  failed: {} ({})",
                need(failed),
                failed.evidence_ids.join(", ")
            )?;
        }
        if let Some(signoff) = &rule.signoff
            && signoff.have < signoff.count
        {
            let mut who = signoff.groups.clone();
            who.extend(signoff.identities.iter().cloned());
            writeln!(
                w,
                "  sign-off: {} of {} from {}",
                signoff.have,
                signoff.count,
                who.join(", ")
            )?;
        }
        for path in &rule.paths {
            writeln!(w, "  path: {path}")?;
        }
        if rule.paths_total > i64::try_from(rule.paths.len()).unwrap_or(i64::MAX) {
            writeln!(w, "  ... {} paths in all", rule.paths_total)?;
        }
    }
    writeln!(
        w,
        "{rest} other rule{} satisfied or not applicable",
        if rest == 1 { "" } else { "s" }
    )?;
    for diagnostic in &evaluation.diagnostics {
        writeln!(w, "warning: {diagnostic}")?;
    }
    Ok(())
}

impl TextRender for PolicyCheck {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "{}: policy version {} loads", self.path, self.version)?;
        writeln!(w, "rules: {}", self.rules.join(", "))?;
        if let Some(evaluation) = &self.evaluation {
            writeln!(w, "decision: {}", decision_word(evaluation.decision))?;
            render_rules(w, evaluation)?;
        }
        if let Some(conflict) = &self.conflict {
            let by = conflict.rule.map_or_else(
                || "[conflicts]".to_owned(),
                |rule| format!("conflicts.rule[{rule}]"),
            );
            writeln!(
                w,
                "conflict tier: {} ({} attempts; {by})",
                serde_json::to_value(conflict.tier)
                    .ok()
                    .and_then(|v| v.as_str().map(str::to_owned))
                    .unwrap_or_default(),
                conflict.attempts
            )?;
        }
        Ok(())
    }
}

impl TextRender for PolicyEvaluated {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "decision: {}", decision_word(self.evaluation.decision))?;
        let source = &self.source;
        if source.from_request {
            writeln!(
                w,
                "policy: {} (sha256 {})",
                source.path,
                short(&source.sha256)
            )?;
        } else if source.blob_id.is_empty() {
            writeln!(
                w,
                "policy: none in {} at {}",
                reef_policy::POLICY_PATH,
                short(&source.commit_id)
            )?;
        } else {
            writeln!(
                w,
                "policy: {} at {} ({}, sha256 {})",
                source.path,
                source.revision,
                short(&source.commit_id),
                short(&source.sha256)
            )?;
        }
        writeln!(
            w,
            "change: {} at {}",
            short(&self.change_id),
            short(&self.commit_id)
        )?;
        match self.evaluation.decision {
            Decision::NoPolicy => writeln!(
                w,
                "nothing lands automatically without a policy; `reef policy init` writes a starter"
            )?,
            Decision::InvalidPolicy => {
                for diagnostic in &self.evaluation.diagnostics {
                    writeln!(w, "{}:{diagnostic}", reef_policy::POLICY_PATH)?;
                }
            }
            Decision::Allow | Decision::Block => render_rules(w, &self.evaluation)?,
        }
        if let Some(input) = &self.input_json {
            writeln!(w, "{input}")?;
        }
        Ok(())
    }
}

impl TextRender for PolicyInit {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "wrote {} (owner {})", self.path, self.owner)?;
        if self.placeholder {
            writeln!(
                w,
                "`{}` is a placeholder: no daemon answered; write your identity's name (`reef identity list`)",
                self.owner
            )?;
        }
        writeln!(w, "check it with `reef policy check`, then commit it")
    }
}

fn short(id: &str) -> &str {
    &id[..id.len().min(12)]
}

/// A proposed policy file's bytes. An empty file is refused here as one
/// that does not load: sent as empty `policy`, it would mean no text at all.
fn policy_text(path: &Path) -> Result<Vec<u8>, CliError> {
    let bytes = std::fs::read(path).map_err(|source| CliError::Io {
        op: "read",
        path: path.to_owned(),
        source,
    })?;
    if bytes.is_empty() {
        let diagnostics = Policy::load(&bytes).err().unwrap_or_default();
        return Err(invalid(&path.display().to_string(), &diagnostics));
    }
    Ok(bytes)
}

/// The error for a file that does not load: the first problem as the
/// message, every problem in the hint.
fn invalid(path: &str, diagnostics: &[Diagnostic]) -> CliError {
    let first = diagnostics.first().cloned().unwrap_or_default();
    let all: Vec<String> = diagnostics
        .iter()
        .map(|diagnostic| format!("{path}:{diagnostic}"))
        .collect();
    CliError::Precheck {
        kind: ErrorKind::InvalidArgument,
        reason: reef_api::error::INVALID_POLICY,
        message: format!(
            "{path}:{first}{}",
            if diagnostics.len() > 1 {
                format!(" (and {} more)", diagnostics.len() - 1)
            } else {
                String::new()
            }
        ),
        advice: Advice::hint(format!(
            "Fix every problem, then check again (docs/policy.md describes the format):\n{}",
            all.join("\n")
        )),
    }
}

/// Loads `bytes`, and with `input` evaluates them, and prints the check.
fn check(out: &Out, path: &str, bytes: &[u8], input: Option<&Path>) -> Result<(), CliError> {
    let policy = Policy::load(bytes).map_err(|diagnostics| invalid(path, &diagnostics))?;
    let mut checked = PolicyCheck {
        path: path.to_owned(),
        sha256: policy.sha256().to_owned(),
        version: reef_policy::VERSION,
        rules: policy.rule_names(),
        evaluation: None,
        conflict: None,
    };
    if let Some(input) = input {
        let text = if input.as_os_str() == "-" {
            let mut text = Vec::new();
            io::Read::read_to_end(&mut io::stdin().lock(), &mut text).map_err(|source| {
                CliError::Io {
                    op: "read",
                    path: input.to_owned(),
                    source,
                }
            })?;
            text
        } else {
            std::fs::read(input).map_err(|source| CliError::Io {
                op: "read",
                path: input.to_owned(),
                source,
            })?
        };
        match Case::from_json(&text)
            .map_err(|message| CliError::Usage(format!("--input {}: {message}", input.display())))?
        {
            Case::Change(input) => checked.evaluation = Some(policy.evaluate(&input)),
            Case::Conflict(conflict) => {
                checked.conflict =
                    Some(
                        policy
                            .conflict_tier(&conflict)
                            .map_err(|message| CliError::Refused {
                                kind: ErrorKind::InvalidArgument,
                                message,
                                advice: None,
                            })?,
                    );
            }
        }
    }
    out.object_with(&checked, |checked| checked)
}

/// Runs a command that talks to the daemon.
pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    match args.command {
        PolicyCommand::Check(args) => check_at(ctx, args).await,
        PolicyCommand::Eval(args) => eval(ctx, args).await,
        PolicyCommand::Simulate(args) => simulate(ctx, args).await,
        PolicyCommand::Init(args) => init(ctx, &args).await,
    }
}

async fn check_at(ctx: &Ctx, args: CheckArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let revision = args.revision.clone().unwrap_or_else(|| "@".to_owned());
    let read = ctx
        .call(ctx.client.read_file_to_end(ReadFileRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: revision.clone(),
            path: reef_policy::POLICY_PATH.to_owned(),
            ..ReadFileRequest::default()
        }))
        .await?;
    check(
        &ctx.out,
        &format!("{revision}:{}", reef_policy::POLICY_PATH),
        &read.content,
        args.input.as_deref(),
    )
}

fn need_of(need: &v1::PolicyEvidenceNeed) -> Need {
    Need {
        kind: need.kind.clone(),
        target: need.target.clone(),
        attestors: need.attestors.clone(),
        evidence_ids: need.evidence_ids.clone(),
    }
}

fn kind_of(kind: i32) -> RuleKind {
    match v1::PolicyRuleKind::try_from(kind).unwrap_or_default() {
        v1::PolicyRuleKind::Require => RuleKind::Require,
        v1::PolicyRuleKind::Limits => RuleKind::Limits,
        v1::PolicyRuleKind::Forbid => RuleKind::Forbid,
        v1::PolicyRuleKind::Signoff => RuleKind::Signoff,
        v1::PolicyRuleKind::Rule => RuleKind::Rule,
        v1::PolicyRuleKind::Builtin | v1::PolicyRuleKind::Unspecified => RuleKind::Builtin,
    }
}

fn decision_of(decision: i32) -> Decision {
    match v1::PolicyDecision::try_from(decision).unwrap_or_default() {
        v1::PolicyDecision::Allow => Decision::Allow,
        v1::PolicyDecision::NoPolicy => Decision::NoPolicy,
        v1::PolicyDecision::InvalidPolicy => Decision::InvalidPolicy,
        v1::PolicyDecision::Block | v1::PolicyDecision::Unspecified => Decision::Block,
    }
}

fn diagnostic_of(d: &v1::PolicyDiagnostic) -> Diagnostic {
    Diagnostic {
        rule: d.rule.clone(),
        field: d.field.clone(),
        line: d.line,
        column: d.column,
        message: d.message.clone(),
    }
}

/// A rule result message as `reef_policy`'s.
fn rule_of(rule: &v1::PolicyRuleResult) -> RuleResult {
    RuleResult {
        name: rule.name.clone(),
        kind: kind_of(rule.kind),
        status: match v1::PolicyRuleStatus::try_from(rule.status).unwrap_or_default() {
            v1::PolicyRuleStatus::Satisfied => Status::Satisfied,
            v1::PolicyRuleStatus::Unsatisfied => Status::Unsatisfied,
            v1::PolicyRuleStatus::NotApplicable => Status::NotApplicable,
            v1::PolicyRuleStatus::Error | v1::PolicyRuleStatus::Unspecified => Status::Error,
        },
        message: rule.message.clone(),
        paths: rule.paths.clone(),
        paths_total: i64::from(rule.paths_total),
        missing: rule.missing.iter().map(need_of).collect(),
        failed: rule.failed.iter().map(need_of).collect(),
        signoff: rule.signoff.as_ref().map(|s| SignoffNeed {
            groups: s.groups.clone(),
            identities: s.identities.clone(),
            count: i64::from(s.count),
            have: i64::from(s.have),
            signed_by: s.signed_by.clone(),
            presence: s.presence.clone(),
            kinds: s.kinds.clone(),
        }),
        evidence_ids: rule.evidence_ids.clone(),
    }
}

/// `message`'s evaluation as `reef_policy`'s, which `check --input` prints:
/// the same fields, so the two compare.
pub(crate) fn evaluation_of(message: &v1::PolicyEvaluation) -> Evaluation {
    let source = message.source.clone().unwrap_or_default();
    let input = message.input.unwrap_or_default();
    Evaluation {
        decision: decision_of(message.decision),
        sha256: source.sha256,
        version: source.version,
        diagnostics: message.diagnostics.iter().map(diagnostic_of).collect(),
        rules: message.rules.iter().map(rule_of).collect(),
        input: InputSummary {
            paths: i64::from(input.paths),
            lines_added: i64::try_from(input.lines_added).unwrap_or(i64::MAX),
            lines_removed: i64::try_from(input.lines_removed).unwrap_or(i64::MAX),
            evidence: i64::from(input.evidence),
            evidence_ignored: i64::from(input.evidence_ignored),
            provenance: i64::from(input.provenance),
            conflicted: input.conflicted,
        },
        cost: message.cost,
    }
}

async fn eval(ctx: &Ctx, args: EvalArgs) -> Result<(), CliError> {
    let scope = scope::resolve(ctx, &args.scope).await?;
    let policy = match &args.policy_file {
        Some(path) => policy_text(path)?,
        None => Vec::new(),
    };
    let response = ctx
        .call(ctx.client.evaluate_policy(EvaluatePolicyRequest {
            repo: scope.repo_id.clone(),
            workspace: scope.workspace_id(),
            revision: args.revision.clone().unwrap_or_default(),
            policy_revision: args.policy_revision.clone().unwrap_or_default(),
            policy,
            include_input: args.input_json,
            ..EvaluatePolicyRequest::default()
        }))
        .await?;
    let message = response.evaluation.unwrap_or_default();
    let source = message.source.clone().unwrap_or_default();
    let evaluated = PolicyEvaluated {
        source: PolicySource {
            revision: source.revision,
            commit_id: source.commit_id,
            path: match &args.policy_file {
                Some(path) => path.display().to_string(),
                None => source.path,
            },
            blob_id: source.blob_id,
            sha256: source.sha256,
            version: source.version,
            from_request: source.from_request,
        },
        change_id: message.change_id.clone(),
        commit_id: message.commit_id.clone(),
        tree_id: message.tree_id.clone(),
        at_operation: message.at_operation.clone(),
        evaluation: evaluation_of(&message),
        input_json: (!message.input_json.is_empty())
            .then(|| serde_json::from_str(&message.input_json).ok())
            .flatten(),
    };
    let refused = args.require_allow && evaluated.evaluation.decision != Decision::Allow;
    // Refused, JSON gets the error document alone, as every failure's; text
    // still shows what blocked.
    if !refused || !ctx.out.is_json() {
        ctx.out.object_with(&evaluated, |evaluated| {
            BriefEvaluation::of(&evaluated.evaluation)
        })?;
    }
    if refused {
        let rule = evaluated.evaluation.first_blocking();
        let revision = args.revision.unwrap_or_else(|| "@".to_owned());
        let hint = match (evaluated.evaluation.decision, rule) {
            (Decision::NoPolicy, _) => "The policy revision has no reef-policy.toml: `reef policy init` writes a starter to commit.".to_owned(),
            (Decision::InvalidPolicy, _) => match (&args.policy_file, &args.policy_revision) {
                (Some(file), _) => format!(
                    "The policy does not load: `reef policy check {}` lists its problems.",
                    file.display()
                ),
                (None, Some(revision)) => format!(
                    "The policy does not load: `reef policy check -r {revision}` lists its problems."
                ),
                (None, None) => {
                    "The policy does not load: `reef policy check -r 'trunk()'` lists its problems."
                        .to_owned()
                }
            },
            (_, Some(rule)) => format!("{}: {}", rule.name, rule.message),
            (_, None) => "A rule blocks the change.".to_owned(),
        };
        return Err(CliError::Precheck {
            kind: ErrorKind::Conflict,
            reason: reef_api::error::POLICY_BLOCKED,
            message: format!(
                "the policy's decision is {}, not allow",
                decision_word(evaluated.evaluation.decision)
            ),
            advice: Advice::run(hint, ["reef", "policy", "eval", "-r", revision.as_str()]),
        });
    }
    Ok(())
}

async fn init(ctx: &Ctx, args: &InitArgs) -> Result<(), CliError> {
    let identities = ctx.client.list_identities_all(false).await.ok();
    let default = identities.as_ref().and_then(|identities| {
        identities
            .iter()
            .find(|identity| identity.is_default)
            .map(|identity| identity.name.clone())
    });
    let placeholder = default.is_none();
    let owner = default.unwrap_or_else(|| "you".to_owned());
    let text = reef_policy::starter(&owner);
    if args.stdout {
        return ctx.out.raw(text.as_bytes());
    }
    let path = PathBuf::from(reef_policy::POLICY_PATH);
    let mut file = match std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&path)
    {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            return Err(CliError::Precheck {
                kind: ErrorKind::AlreadyExists,
                reason: reef_api::error::POLICY_EXISTS,
                message: format!("{} already exists", path.display()),
                advice: Advice::run(
                    "Check the file you have, or print the starter with --stdout and merge it by hand.",
                    ["reef", "policy", "check"],
                ),
            });
        }
        Err(source) => {
            return Err(CliError::Io {
                op: "create",
                path,
                source,
            });
        }
    };
    file.write_all(text.as_bytes())
        .map_err(|source| CliError::Io {
            op: "write",
            path: path.clone(),
            source,
        })?;
    ctx.out.object(&PolicyInit {
        path: path.display().to_string(),
        owner,
        placeholder,
    })
}

/// How a landing's decision under the proposed policy compares with the
/// baseline's (R3.08). Blocked means any decision but allow.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    /// The proposed policy blocks it; the baseline allows it.
    NewlyBlocked,
    /// Both block it.
    StillBlocked,
    /// The proposed policy allows it; the baseline blocks it.
    NewlyAllowed,
    /// Both allow it.
    StillAllowed,
    /// The proposed policy blocks it, with no baseline to compare with.
    Blocked,
    /// The proposed policy allows it, with no baseline to compare with.
    Allowed,
}

impl Outcome {
    fn of(outcome: i32) -> Outcome {
        use v1::PolicySimulationOutcome as O;
        match O::try_from(outcome).unwrap_or_default() {
            O::NewlyBlocked => Outcome::NewlyBlocked,
            O::StillBlocked => Outcome::StillBlocked,
            O::NewlyAllowed => Outcome::NewlyAllowed,
            O::StillAllowed => Outcome::StillAllowed,
            O::Allowed => Outcome::Allowed,
            O::Blocked | O::Unspecified => Outcome::Blocked,
        }
    }

    fn word(self) -> &'static str {
        match self {
            Outcome::NewlyBlocked => "newly blocked",
            Outcome::StillBlocked => "still blocked",
            Outcome::NewlyAllowed => "newly allowed",
            Outcome::StillAllowed => "still allowed",
            Outcome::Blocked => "blocked",
            Outcome::Allowed => "allowed",
        }
    }
}

/// One landing, judged by the proposed policy and the baseline.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SimulatedLanding {
    /// Its change.
    pub(crate) change_id: String,
    /// Its commit.
    pub(crate) commit_id: String,
    /// Its tree; empty when conflicted.
    pub(crate) tree_id: String,
    /// The first line of its description.
    pub(crate) subject: String,
    /// Its author, as `name <email>`.
    pub(crate) author: String,
    /// When it was committed.
    pub(crate) committed_at: Option<String>,
    /// The proposed policy's decision.
    pub(crate) decision: Decision,
    /// The baseline's decision; absent with no baseline.
    pub(crate) baseline_decision: Option<Decision>,
    /// How the two compare.
    pub(crate) outcome: Outcome,
    /// The proposed policy's rules that are unsatisfied or errored.
    pub(crate) unmet: Vec<RuleResult>,
    /// The names of the baseline's rules that are unsatisfied or errored.
    pub(crate) baseline_unmet: Vec<String>,
    /// Blocked only for evidence its tree does not have.
    pub(crate) missing_evidence_only: bool,
    /// The cost of both evaluations.
    pub(crate) cost: u64,
    /// With --evaluations, the proposed policy's full evaluation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) evaluation: Option<Evaluation>,
    /// With --evaluations, the baseline's full evaluation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub(crate) baseline_evaluation: Option<Evaluation>,
    /// Too large for a page alone: its evaluations, and perhaps its unmet
    /// rules' paths, were left out (`reef policy eval -r` has them).
    pub(crate) trimmed: bool,
}

/// How many landings one rule of the proposed policy blocked.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct SimulatedRule {
    /// The rule's name.
    pub(crate) name: String,
    /// Its kind.
    pub(crate) kind: RuleKind,
    /// The landings it blocked.
    pub(crate) landings: u32,
}

/// Counts over every landing judged.
#[derive(Debug, Default, Serialize, JsonSchema)]
pub(crate) struct SimulationSummary {
    /// Landings judged.
    pub(crate) landings: u32,
    /// Landings the proposed policy blocks.
    pub(crate) blocked: u32,
    /// Of those, the ones blocked only for missing evidence.
    pub(crate) missing_evidence_only: u32,
    /// Blocked by the proposed policy, allowed by the baseline.
    pub(crate) newly_blocked: u32,
    /// Blocked by both.
    pub(crate) still_blocked: u32,
    /// Allowed by the proposed policy, blocked by the baseline.
    pub(crate) newly_allowed: u32,
    /// Allowed by both.
    pub(crate) still_allowed: u32,
    /// Every rule of the proposed policy, in its order, with the landings
    /// it blocked.
    pub(crate) rules: Vec<SimulatedRule>,
}

impl SimulationSummary {
    fn add(&mut self, page: &v1::PolicySimulationSummary) {
        self.landings += page.landings;
        self.blocked += page.blocked;
        self.missing_evidence_only += page.missing_evidence_only;
        self.newly_blocked += page.newly_blocked;
        self.still_blocked += page.still_blocked;
        self.newly_allowed += page.newly_allowed;
        self.still_allowed += page.still_allowed;
        for rule in &page.rules {
            match self.rules.iter_mut().find(|have| have.name == rule.name) {
                Some(have) => have.landings += rule.landings,
                None => self.rules.push(SimulatedRule {
                    name: rule.name.clone(),
                    kind: kind_of(rule.kind),
                    landings: rule.landings,
                }),
            }
        }
    }
}

/// The baseline's state.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub(crate) enum BaselineState {
    /// The baseline policy loads.
    Loads,
    /// Its revision has no reef-policy.toml: nothing to compare with.
    NoPolicy,
    /// It does not load: it blocks every landing.
    InvalidPolicy,
}

/// What `reef policy simulate` prints.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct PolicySimulation {
    /// The proposed policy.
    pub(crate) proposed: PolicySource,
    /// The baseline; absent with --no-baseline.
    pub(crate) baseline: Option<PolicySource>,
    /// The baseline's state; absent with --no-baseline.
    pub(crate) baseline_state: Option<BaselineState>,
    /// The operation the landings were read at.
    pub(crate) at_operation: String,
    /// The landings judged, newest first; in text, only those blocked
    /// unless --all.
    pub(crate) landings: Vec<SimulatedLanding>,
    /// Counts over them.
    pub(crate) summary: SimulationSummary,
    /// The evaluations' cost, summed.
    pub(crate) cost: u64,
    /// --limit stopped before the window's end.
    pub(crate) truncated: bool,
    /// Text only: list every landing.
    #[serde(skip)]
    pub(crate) all: bool,
}

/// The brief view of a simulation: the counts, and each blocked landing
/// with its unmet rules' names.
#[derive(Debug, Serialize, JsonSchema)]
pub(crate) struct BriefSimulation<'a> {
    landings: u32,
    blocked: u32,
    newly_blocked: u32,
    still_blocked: u32,
    newly_allowed: u32,
    missing_evidence_only: u32,
    truncated: bool,
    blocking: Vec<BriefLanding<'a>>,
}

#[derive(Debug, Serialize, JsonSchema)]
struct BriefLanding<'a> {
    commit: &'a str,
    outcome: Outcome,
    rules: Vec<&'a str>,
}

impl<'a> BriefSimulation<'a> {
    pub(crate) fn of(simulation: &'a PolicySimulation) -> BriefSimulation<'a> {
        let summary = &simulation.summary;
        BriefSimulation {
            landings: summary.landings,
            blocked: summary.blocked,
            newly_blocked: summary.newly_blocked,
            still_blocked: summary.still_blocked,
            newly_allowed: summary.newly_allowed,
            missing_evidence_only: summary.missing_evidence_only,
            truncated: simulation.truncated,
            blocking: simulation
                .landings
                .iter()
                .filter(|landing| landing.decision != Decision::Allow)
                .map(|landing| BriefLanding {
                    commit: short(&landing.commit_id),
                    outcome: landing.outcome,
                    rules: landing
                        .unmet
                        .iter()
                        .map(|rule| rule.name.as_str())
                        .collect(),
                })
                .collect(),
        }
    }
}

fn plural(n: u32, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

/// A policy source in one line.
fn source_line(source: &PolicySource) -> String {
    if source.from_request {
        format!("{} (sha256 {})", source.path, short(&source.sha256))
    } else if source.blob_id.is_empty() {
        format!(
            "none in {} at {} ({})",
            reef_policy::POLICY_PATH,
            source.revision,
            short(&source.commit_id)
        )
    } else {
        format!(
            "{} at {} ({}, sha256 {})",
            source.path,
            source.revision,
            short(&source.commit_id),
            short(&source.sha256)
        )
    }
}

/// A short reason for an unmet rule.
fn reason(rule: &RuleResult) -> String {
    if let Some(missing) = rule.missing.first() {
        let more = if rule.missing.len() > 1 {
            format!(" and {} more", rule.missing.len() - 1)
        } else {
            String::new()
        };
        return format!(
            "missing {} from {}{more}",
            missing.kind,
            missing.attestors.join(" or ")
        );
    }
    if !rule.message.is_empty() {
        return rule.message.clone();
    }
    status_word(rule.status).to_owned()
}

impl TextRender for PolicySimulation {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        writeln!(w, "proposed: {}", source_line(&self.proposed))?;
        match (&self.baseline, self.baseline_state) {
            (Some(source), Some(BaselineState::NoPolicy)) => writeln!(
                w,
                "baseline: {}; nothing to compare with",
                source_line(source)
            )?,
            (Some(source), Some(BaselineState::InvalidPolicy)) => writeln!(
                w,
                "baseline: {} does not load; it blocks every landing",
                source_line(source)
            )?,
            (Some(source), _) => writeln!(w, "baseline: {}", source_line(source))?,
            (None, _) => writeln!(w, "baseline: none")?,
        }
        for landing in &self.landings {
            if !self.all && landing.decision == Decision::Allow {
                continue;
            }
            writeln!(
                w,
                "{:<13}  {}  {}",
                landing.outcome.word(),
                short(&landing.commit_id),
                landing.subject
            )?;
            for rule in &landing.unmet {
                writeln!(w, "  {}: {}", rule.name, reason(rule))?;
            }
        }
        let summary = &self.summary;
        let allowed = summary.landings - summary.blocked;
        let mut detail = Vec::new();
        if self.baseline_state == Some(BaselineState::Loads)
            || self.baseline_state == Some(BaselineState::InvalidPolicy)
        {
            detail.push(format!(
                "{} newly, {} also by the baseline",
                summary.newly_blocked, summary.still_blocked
            ));
        }
        if summary.missing_evidence_only > 0 {
            detail.push(format!(
                "{} only for missing evidence",
                summary.missing_evidence_only
            ));
        }
        let detail = if detail.is_empty() {
            String::new()
        } else {
            format!(" ({})", detail.join("; "))
        };
        writeln!(
            w,
            "{}: {} would have been blocked{detail}; {} allowed{}",
            plural(summary.landings, "landing", "landings"),
            summary.blocked,
            allowed,
            if summary.newly_allowed > 0 {
                format!(" ({} newly)", summary.newly_allowed)
            } else {
                String::new()
            }
        )?;
        let blocking: Vec<String> = summary
            .rules
            .iter()
            .filter(|rule| rule.landings > 0)
            .map(|rule| format!("{} {}", rule.name, rule.landings))
            .collect();
        if !blocking.is_empty() {
            writeln!(w, "blocked by: {}", blocking.join(", "))?;
        }
        if self.truncated {
            writeln!(w, "more landings follow: raise --limit (at most 1000)")?;
        }
        Ok(())
    }
}

/// A landing message as the CLI's.
fn landing_of(landing: &v1::PolicySimulatedLanding) -> SimulatedLanding {
    SimulatedLanding {
        change_id: landing.change_id.clone(),
        commit_id: landing.commit_id.clone(),
        tree_id: landing.tree_id.clone(),
        subject: landing.subject.clone(),
        author: landing.author.clone(),
        committed_at: landing
            .committed_at
            .as_ref()
            .map(reef_client::json::rfc3339),
        decision: decision_of(landing.decision),
        baseline_decision: (landing.baseline_decision != 0)
            .then(|| decision_of(landing.baseline_decision)),
        outcome: Outcome::of(landing.outcome),
        unmet: landing.unmet.iter().map(rule_of).collect(),
        baseline_unmet: landing.baseline_unmet.clone(),
        missing_evidence_only: landing.missing_evidence_only,
        cost: landing.cost,
        evaluation: landing.evaluation.as_ref().map(evaluation_of),
        baseline_evaluation: landing.baseline_evaluation.as_ref().map(evaluation_of),
        trimmed: landing.trimmed,
    }
}

fn source_of(source: v1::PolicySource, path: Option<&Path>) -> PolicySource {
    PolicySource {
        revision: source.revision,
        commit_id: source.commit_id,
        path: match path {
            Some(path) => path.display().to_string(),
            None => source.path,
        },
        blob_id: source.blob_id,
        sha256: source.sha256,
        version: source.version,
        from_request: source.from_request,
    }
}

#[allow(clippy::too_many_lines)]
async fn simulate(ctx: &Ctx, args: SimulateArgs) -> Result<(), CliError> {
    let file = match (&args.policy_file, &args.policy_revision) {
        (Some(path), _) => Some(path.clone()),
        (None, Some(_)) => None,
        (None, None) => Some(PathBuf::from(reef_policy::POLICY_PATH)),
    };
    let policy = match &file {
        Some(path) if args.policy_file.is_none() && !path.exists() => {
            return Err(CliError::Precheck {
                kind: ErrorKind::InvalidArgument,
                reason: reef_api::error::POLICY_SOURCE_MISSING,
                message: format!("no {} in this directory to simulate", path.display()),
                advice: Advice::hint(
                    "Name the proposed policy with --policy-file FILE, or a revision whose reef-policy.toml to simulate with --policy-revision REV.",
                ),
            });
        }
        Some(path) => policy_text(path)?,
        None => Vec::new(),
    };
    let scope = scope::resolve(ctx, &args.scope).await?;
    let request = v1::SimulatePolicyRequest {
        repo: scope.repo_id.clone(),
        workspace: scope.workspace_id(),
        revisions: args.revisions.clone().unwrap_or_default(),
        policy,
        policy_revision: args.policy_revision.clone().unwrap_or_default(),
        baseline_revision: args.baseline_revision.clone().unwrap_or_default(),
        no_baseline: args.no_baseline,
        include_evaluations: args.evaluations,
        ..v1::SimulatePolicyRequest::default()
    };
    let limit = usize::try_from(args.limit).unwrap_or(usize::MAX);
    let mut landings = Vec::new();
    let mut summary = SimulationSummary::default();
    let mut cost = 0u64;
    let mut token = String::new();
    let mut first: Option<v1::SimulatePolicyResponse> = None;
    let truncated = loop {
        let remaining = limit - landings.len();
        let page = ctx
            .call(ctx.client.simulate_policy(v1::SimulatePolicyRequest {
                page_size: i32::try_from(remaining.min(100)).unwrap_or(100),
                page_token: token.clone(),
                ..request.clone()
            }))
            .await?;
        if page.proposed_decision() != v1::PolicyDecision::Allow {
            let path = match (&file, &args.policy_revision) {
                (Some(path), _) => path.display().to_string(),
                (None, Some(revision)) => format!("{revision}:{}", reef_policy::POLICY_PATH),
                (None, None) => reef_policy::POLICY_PATH.to_owned(),
            };
            if page.proposed_decision() == v1::PolicyDecision::NoPolicy {
                return Err(CliError::Precheck {
                    kind: ErrorKind::InvalidArgument,
                    reason: reef_api::error::INVALID_POLICY,
                    message: format!("there is no {path} to simulate"),
                    advice: Advice::hint(
                        "Name a revision that has a reef-policy.toml, or a file with --policy-file.",
                    ),
                });
            }
            let diagnostics: Vec<Diagnostic> = page.diagnostics.iter().map(diagnostic_of).collect();
            return Err(invalid(&path, &diagnostics));
        }
        cost = cost.saturating_add(page.cost);
        if let Some(page_summary) = &page.summary {
            summary.add(page_summary);
        }
        landings.extend(page.landings.iter().map(landing_of));
        let next = page.next_page_token.clone();
        if first.is_none() {
            first = Some(page);
        }
        if next.is_empty() {
            break false;
        }
        if landings.len() >= limit {
            break true;
        }
        token = next;
    };
    let first = first.unwrap_or_default();
    let simulation = PolicySimulation {
        proposed: source_of(first.proposed.clone().unwrap_or_default(), file.as_deref()),
        baseline: first.baseline.clone().map(|source| source_of(source, None)),
        baseline_state: match first.baseline_state() {
            v1::PolicyDecision::Unspecified => None,
            v1::PolicyDecision::NoPolicy => Some(BaselineState::NoPolicy),
            v1::PolicyDecision::InvalidPolicy => Some(BaselineState::InvalidPolicy),
            v1::PolicyDecision::Allow | v1::PolicyDecision::Block => Some(BaselineState::Loads),
        },
        at_operation: first.at_operation.clone(),
        landings,
        summary,
        cost,
        truncated,
        all: args.all,
    };
    ctx.out.object_with(&simulation, BriefSimulation::of)
}
