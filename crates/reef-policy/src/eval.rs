//! Evaluating a policy over one input: attestor sets, the declared tables,
//! the built-in rules and the free rules, into an [`Evaluation`].
//!
//! Attestor matching and the tables are plain Rust over the input, run
//! before any CEL, so their results carry data (what is missing, who must
//! sign) and their cost is linear. Every rule is evaluated and reported;
//! one that is unsatisfied or errors blocks.

use std::collections::{BTreeMap, BTreeSet};

use cel::Context;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::check::shapes;
use crate::cost::{self, MAX_EVALUATION_COST, MAX_EXPRESSION_COST, Sizes};
use crate::env;
use crate::format::{Attestor, Diagnostic, Entry, Expression, Policy, Presence, Require, Signoff};
use crate::input::{Evidence, Input};

/// What a policy decides about a change.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "policy.Decision")]
pub enum Decision {
    /// Every rule is satisfied or does not apply.
    Allow,
    /// Some rule is unsatisfied or errored.
    Block,
    /// There is no policy file: nothing lands automatically.
    NoPolicy,
    /// The policy file does not load: nothing lands automatically.
    InvalidPolicy,
}

/// Which kind of rule a result is for.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "policy.RuleKind")]
pub enum RuleKind {
    /// `builtin:input`, `builtin:conflicts`, `builtin:policy-file` or
    /// `builtin:owners`.
    Builtin,
    /// A `[[require]]` entry.
    Require,
    /// `[limits]`.
    Limits,
    /// A `[[forbid]]` entry.
    Forbid,
    /// A `[[signoff]]` entry.
    Signoff,
    /// A `[[rule]]` entry.
    Rule,
}

/// A rule's outcome.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
#[schemars(rename = "policy.Status")]
pub enum Status {
    /// The rule holds.
    Satisfied,
    /// The rule does not hold: it blocks.
    Unsatisfied,
    /// The rule does not apply to this change.
    NotApplicable,
    /// The rule could not be evaluated: it blocks.
    Error,
}

/// Evidence a requirement needs, by kind and target, from some of its
/// attestor sets.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Need")]
pub struct Need {
    /// The evidence kind.
    pub kind: String,
    /// The target; empty for the whole check.
    pub target: String,
    /// The attestor sets whose evidence counts.
    pub attestors: Vec<String>,
    /// For a failed need, the failing object.
    pub evidence_ids: Vec<String>,
    /// For a missing need, the objects of its kind and target that were on
    /// the tree and did not count, each with why (R3.13), at most
    /// [`crate::MAX_REJECTED`]; the daemon adds those whose signature does
    /// not verify, which never reach the evaluator.
    pub rejected: Vec<Rejected>,
    /// How many objects were rejected; more than `rejected`'s length when
    /// it was cut.
    pub rejected_total: i64,
}

/// An evidence object that did not count for a need, and why (R3.13).
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.Rejected")]
pub struct Rejected {
    /// The evidence id.
    pub evidence_id: String,
    /// Why it did not count: `error` (no verdict), `key_unbound`,
    /// `key_expired`, `key_revoked` or `key_invalid` (its key's status at
    /// witness time), `self` (from the change's own session or agent),
    /// `presence` (its key's presence is below the set's), `condition` (a
    /// set's `when` was false), or `untrusted` (no attestor set of the rule
    /// accepts its signer). The daemon adds `signature_invalid`.
    pub why: String,
}

/// A sign-off a rule needs, and what it has.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.SignoffNeed")]
pub struct SignoffNeed {
    /// The groups whose members may sign.
    pub groups: Vec<String>,
    /// The identities that may sign.
    pub identities: Vec<String>,
    /// How many distinct identities must.
    pub count: i64,
    /// How many have.
    pub have: i64,
    /// Who has, by identity name (or id when the name is not known).
    pub signed_by: Vec<String>,
    /// The least presence a sign-off's key needs.
    pub presence: String,
    /// The evidence kinds that count as a sign-off.
    pub kinds: Vec<String>,
}

/// One rule's result.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.RuleResult")]
pub struct RuleResult {
    /// The rule's name.
    pub name: String,
    /// Its kind.
    pub kind: RuleKind,
    /// Its outcome.
    pub status: Status,
    /// Why, for a rule that is not satisfied; the rule's own message when it
    /// has one.
    #[serde(default)]
    pub message: String,
    /// The touched paths the rule is about, at most
    /// [`crate::MAX_REPORTED_PATHS`].
    #[serde(default)]
    pub paths: Vec<String>,
    /// How many there are.
    #[serde(default)]
    pub paths_total: i64,
    /// Evidence missing.
    #[serde(default)]
    pub missing: Vec<Need>,
    /// Evidence whose newest trusted verdict is a failure.
    #[serde(default)]
    pub failed: Vec<Need>,
    /// For a sign-off rule, what it needs and has.
    #[serde(default)]
    pub signoff: Option<SignoffNeed>,
    /// The evidence that satisfied it.
    #[serde(default)]
    pub evidence_ids: Vec<String>,
}

impl RuleResult {
    fn new(name: &str, kind: RuleKind, status: Status) -> RuleResult {
        RuleResult {
            name: name.to_owned(),
            kind,
            status,
            message: String::new(),
            paths: Vec::new(),
            paths_total: 0,
            missing: Vec::new(),
            failed: Vec::new(),
            signoff: None,
            evidence_ids: Vec::new(),
        }
    }

    fn with_message(mut self, message: impl Into<String>) -> RuleResult {
        self.message = message.into();
        self
    }

    fn with_paths<P: AsRef<str>>(mut self, paths: &[P]) -> RuleResult {
        self.paths_total = i64::try_from(paths.len()).unwrap_or(i64::MAX);
        self.paths = paths
            .iter()
            .take(crate::MAX_REPORTED_PATHS)
            .map(|path| path.as_ref().to_owned())
            .collect();
        self
    }
}

/// What the evaluation saw, in counts.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(default, deny_unknown_fields)]
#[schemars(rename = "policy.InputSummary")]
pub struct InputSummary {
    /// Touched paths.
    pub paths: i64,
    /// Lines added.
    pub lines_added: i64,
    /// Lines removed.
    pub lines_removed: i64,
    /// Evidence objects the policy saw.
    pub evidence: i64,
    /// Evidence objects left out: not verified, or untrusted over the cap.
    pub evidence_ignored: i64,
    /// Provenance records.
    pub provenance: i64,
    /// Whether the change is conflicted.
    pub conflicted: bool,
}

impl InputSummary {
    /// The counts of `input`, as it is.
    pub fn of(input: &Input) -> InputSummary {
        InputSummary {
            paths: len(input.change.paths.len()),
            lines_added: input.change.lines_added,
            lines_removed: input.change.lines_removed,
            evidence: len(input.evidence.len()),
            evidence_ignored: input.evidence_ignored,
            provenance: len(input.provenance.len()),
            conflicted: input.change.conflicted,
        }
    }
}

fn len(n: usize) -> i64 {
    i64::try_from(n).unwrap_or(i64::MAX)
}

/// A policy's decision about one change, with a result per rule.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
#[schemars(rename = "policy.Evaluation")]
pub struct Evaluation {
    /// The decision.
    pub decision: Decision,
    /// The SHA-256 of the policy bytes evaluated; empty for no policy.
    #[serde(default)]
    pub sha256: String,
    /// The policy format version.
    #[serde(default)]
    pub version: u32,
    /// For `invalid_policy`, every load error; otherwise warnings, such as an
    /// attestor set's `when` that errored on an object.
    #[serde(default)]
    pub diagnostics: Vec<Diagnostic>,
    /// Every rule: the built-in ones first, then in file order.
    #[serde(default)]
    pub rules: Vec<RuleResult>,
    /// What the evaluation saw.
    #[serde(default)]
    pub input: InputSummary,
    /// The evaluation's cost bound, summed over the expressions it ran.
    #[serde(default)]
    pub cost: u64,
}

impl Evaluation {
    /// The decision when there is no policy file.
    pub fn no_policy(input: &Input) -> Evaluation {
        Evaluation {
            decision: Decision::NoPolicy,
            sha256: String::new(),
            version: 0,
            diagnostics: Vec::new(),
            rules: Vec::new(),
            input: InputSummary::of(input),
            cost: 0,
        }
    }

    /// The decision when the policy file at `bytes` does not load.
    pub fn invalid(bytes: &[u8], diagnostics: Vec<Diagnostic>, input: &Input) -> Evaluation {
        Evaluation {
            decision: Decision::InvalidPolicy,
            sha256: crate::sha256_hex(bytes),
            version: crate::VERSION,
            diagnostics,
            rules: Vec::new(),
            input: InputSummary::of(input),
            cost: 0,
        }
    }

    /// Loads `bytes` (none for no policy file) and evaluates it over
    /// `input`.
    pub fn of(bytes: Option<&[u8]>, input: &Input) -> Evaluation {
        match bytes {
            None => Evaluation::no_policy(input),
            Some(bytes) => match Policy::load(bytes) {
                Ok(policy) => policy.evaluate(input),
                Err(diagnostics) => Evaluation::invalid(bytes, diagnostics, input),
            },
        }
    }

    /// The first rule that blocks, if any.
    pub fn first_blocking(&self) -> Option<&RuleResult> {
        self.rules
            .iter()
            .find(|rule| matches!(rule.status, Status::Unsatisfied | Status::Error))
    }
}

/// `n` as `3.2e9`, for cost messages.
pub(crate) fn scientific(n: u64) -> String {
    if n < 1_000 {
        return n.to_string();
    }
    #[allow(clippy::cast_precision_loss)]
    let value = n as f64;
    let exponent = value.log10().floor();
    let mantissa = value / 10f64.powf(exponent);
    let mantissa = (mantissa * 10.0).round() / 10.0;
    if (mantissa - mantissa.trunc()).abs() < f64::EPSILON {
        format!("{mantissa:.0}e{exponent}")
    } else {
        format!("{mantissa:.1}e{exponent}")
    }
}

struct Evaluator {
    input: Input,
    sizes: Sizes,
    context: Context<'static, 'static>,
    total: u64,
    diagnostics: Vec<Diagnostic>,
    input_problem: Option<String>,
}

impl Evaluator {
    /// Runs `expr`, charging its cost bound first.
    fn run(
        &mut self,
        expr: &Expression,
        context: Option<&Context<'_, '_>>,
        sizes: Option<&Sizes>,
    ) -> Result<bool, String> {
        let cost = cost::cost(&expr.parsed.ast, expr.scope, sizes.unwrap_or(&self.sizes));
        self.charge(cost)?;
        env::boolean(context.unwrap_or(&self.context), &expr.parsed)
    }

    fn charge(&mut self, cost: u64) -> Result<(), String> {
        if cost > MAX_EXPRESSION_COST {
            return Err(format!(
                "cost {} over {}: not run (see docs/policy.md, \"The cost bound\")",
                scientific(cost),
                scientific(MAX_EXPRESSION_COST)
            ));
        }
        if self.total.saturating_add(cost) > MAX_EVALUATION_COST {
            return Err(format!(
                "the evaluation's cost would pass {}: not run",
                scientific(MAX_EVALUATION_COST)
            ));
        }
        self.total += cost;
        Ok(())
    }

    /// An optional condition: true when absent.
    fn condition(&mut self, when: Option<&Expression>) -> Result<bool, String> {
        match when {
            None => Ok(true),
            Some(expr) => self.run(expr, None, None),
        }
    }
}

/// Whether `e`'s signer is in `set` by its declared keys (everything but
/// `when`).
fn declared_match(set: &Attestor, e: &Evidence) -> bool {
    e.key.status == "bound"
        && set.members.contains(&e.key.identity_id, &e.key.identity)
        && (set.identity_kinds.is_empty() || set.identity_kinds.contains(&e.key.identity_kind))
        && set.actors.contains(&e.key.actor)
        && (set.authorities.is_empty() || set.authorities.contains(&e.key.authority))
        && (set.issuer_kinds.is_empty() || set.issuer_kinds.contains(&e.key.issuer_kind))
        && Presence::of_key(&e.key.presence).is_some_and(|presence| presence >= set.presence)
        && (set.allow_self || !e.from_self)
}

/// Whether `e` is a sign-off `spec` counts.
fn is_signoff(spec: &Signoff, e: &Evidence) -> bool {
    spec.kinds.contains(&e.kind)
        && e.result == "pass"
        && e.key.status == "bound"
        && e.key.actor == "human"
        && e.key.identity_kind == "person"
        && spec.members.contains(&e.key.identity_id, &e.key.identity)
        && Presence::of_key(&e.key.presence).is_some_and(|presence| presence >= spec.presence)
}

/// Fills each object's `self`, from the change's provenance sessions and
/// agents.
fn mark_self(input: &mut Input) {
    let sessions: BTreeSet<&str> = input
        .provenance
        .iter()
        .flat_map(|record| [record.session.as_str(), record.key.session.as_str()])
        .filter(|session| !session.is_empty())
        .collect();
    let agents: Vec<(&str, &str)> = input
        .provenance
        .iter()
        .filter(|record| !record.agent.is_empty())
        .map(|record| (record.agent.as_str(), record.key.identity_id.as_str()))
        .collect();
    let marks: Vec<bool> = input
        .evidence
        .iter()
        .map(|e| {
            (!e.key.session.is_empty() && sessions.contains(e.key.session.as_str()))
                || (!e.key.agent.is_empty()
                    && agents.iter().any(|(agent, identity)| {
                        *agent == e.key.agent && *identity == e.key.identity_id
                    }))
                || (!e.added_by_agent.is_empty()
                    && agents.iter().any(|(agent, _)| *agent == e.added_by_agent))
        })
        .collect();
    for (e, mark) in input.evidence.iter_mut().zip(marks) {
        e.from_self = mark;
    }
}

impl Policy {
    /// The SHA-256 of the bytes this policy was loaded from.
    pub fn sha256(&self) -> &str {
        &self.sha256
    }

    /// The names of the policy's rules, the built-in ones first, then in
    /// file order.
    pub fn rule_names(&self) -> Vec<String> {
        let mut names: Vec<String> = crate::BUILTIN_RULES.iter().map(|&n| n.to_owned()).collect();
        names.extend(self.entries.iter().map(|entry| entry.name().to_owned()));
        names
    }

    /// Evaluates the policy over `input`, on a thread of its own
    /// ([`crate::env::STACK_BYTES`]).
    pub fn evaluate(&self, input: &Input) -> Evaluation {
        env::on_policy_stack(|| self.evaluate_here(input)).unwrap_or_else(|| Evaluation {
            decision: Decision::Block,
            sha256: self.sha256.clone(),
            version: crate::VERSION,
            diagnostics: Vec::new(),
            rules: vec![
                RuleResult::new("builtin:input", RuleKind::Builtin, Status::Error)
                    .with_message("could not start a thread to evaluate the policy"),
            ],
            input: InputSummary::of(input),
            cost: 0,
        })
    }

    /// `builtin:description` (R3.16): on a branch whose `[history]`
    /// summarizes landed changes, a change needs a description, since the
    /// summary is its description; and on a branch that is pushed to a git
    /// remote (`[landing] push`, R3.17), whose landed commits reach the host
    /// as they are.
    fn description(&self, input: &Input) -> RuleResult {
        const NAME: &str = "builtin:description";
        if !self.history.summarized() && self.landing.push.is_none() {
            return RuleResult::new(NAME, RuleKind::Builtin, Status::NotApplicable);
        }
        // What the summary keeps (reef_lib::jj::projection::normalize):
        // anything but whitespace, control characters and invisible
        // formatting characters.
        let visible = |c: char| {
            !c.is_whitespace()
                && !c.is_control()
                && !matches!(
                    c,
                    '\u{200B}'..='\u{200F}'
                        | '\u{202A}'..='\u{202E}'
                        | '\u{2060}'..='\u{2069}'
                        | '\u{FEFF}'
                )
        };
        if input.change.description.chars().any(visible) {
            return RuleResult::new(NAME, RuleKind::Builtin, Status::Satisfied);
        }
        let branch = match input.landing.destination.as_str() {
            "" => "this branch".to_owned(),
            destination => destination.to_owned(),
        };
        let id = match input.change.id.as_str() {
            "" => "<change>",
            id => id,
        };
        RuleResult::new(NAME, RuleKind::Builtin, Status::Unsatisfied).with_message(format!(
            "a change landing on {branch} needs a description, which is its summary in the landed history: `reef describe {id} -m \"<summary>\"`, then propose it again"
        ))
    }

    #[allow(clippy::too_many_lines)]
    fn evaluate_here(&self, input: &Input) -> Evaluation {
        let mut evaluator = self.prepare(input);
        let mut rules = Vec::new();
        rules.push(match &evaluator.input_problem {
            None => RuleResult::new("builtin:input", RuleKind::Builtin, Status::Satisfied),
            Some(problem) => RuleResult::new("builtin:input", RuleKind::Builtin, Status::Error)
                .with_message(problem.clone()),
        });
        rules.push(if evaluator.input.change.conflicted {
            RuleResult::new("builtin:conflicts", RuleKind::Builtin, Status::Unsatisfied)
                .with_message("the change's tree has a conflict; resolve it first")
        } else {
            RuleResult::new("builtin:conflicts", RuleKind::Builtin, Status::Satisfied)
        });
        rules.push(self.description(&evaluator.input));
        rules.push(policy_file(&evaluator.input));
        rules.push(self.signoff(&mut evaluator, &self.owners, RuleKind::Builtin));
        for entry in &self.entries {
            let result = match entry {
                Entry::Require(require) => self.require(&mut evaluator, require),
                Entry::Limits(limits) => limits_result(&evaluator.input, limits),
                Entry::Forbid(forbid) => {
                    let covered: Vec<String> = forbid
                        .filter
                        .covered(&evaluator.input.change.paths)
                        .into_iter()
                        .cloned()
                        .collect();
                    match evaluator.condition(forbid.when.as_ref()) {
                        Err(error) => {
                            RuleResult::new(&forbid.name, RuleKind::Forbid, Status::Error)
                                .with_message(format!("when: {error}"))
                        }
                        Ok(false) => {
                            RuleResult::new(&forbid.name, RuleKind::Forbid, Status::NotApplicable)
                        }
                        Ok(true) if covered.is_empty() => {
                            RuleResult::new(&forbid.name, RuleKind::Forbid, Status::Satisfied)
                        }
                        Ok(true) => {
                            let message = if forbid.message.is_empty() {
                                format!(
                                    "touches {} forbidden path{}",
                                    covered.len(),
                                    if covered.len() == 1 { "" } else { "s" }
                                )
                            } else {
                                forbid.message.clone()
                            };
                            RuleResult::new(&forbid.name, RuleKind::Forbid, Status::Unsatisfied)
                                .with_message(message)
                                .with_paths(&covered)
                        }
                    }
                }
                Entry::Signoff(signoff) => self.signoff(&mut evaluator, signoff, RuleKind::Signoff),
                Entry::Rule(rule) => match evaluator.condition(rule.when.as_ref()) {
                    Err(error) => RuleResult::new(&rule.name, RuleKind::Rule, Status::Error)
                        .with_message(format!("when: {error}")),
                    Ok(false) => RuleResult::new(&rule.name, RuleKind::Rule, Status::NotApplicable),
                    Ok(true) => match evaluator.run(&rule.expr, None, None) {
                        Ok(true) => RuleResult::new(&rule.name, RuleKind::Rule, Status::Satisfied),
                        Ok(false) => {
                            let message = if rule.message.is_empty() {
                                format!("`{}` is false", rule.expr.source)
                            } else {
                                rule.message.clone()
                            };
                            RuleResult::new(&rule.name, RuleKind::Rule, Status::Unsatisfied)
                                .with_message(message)
                        }
                        Err(error) => RuleResult::new(&rule.name, RuleKind::Rule, Status::Error)
                            .with_message(error),
                    },
                },
            };
            rules.push(result);
        }
        let decision = if rules
            .iter()
            .any(|rule| matches!(rule.status, Status::Unsatisfied | Status::Error))
        {
            Decision::Block
        } else {
            Decision::Allow
        };
        Evaluation {
            decision,
            sha256: self.sha256.clone(),
            version: crate::VERSION,
            diagnostics: evaluator.diagnostics,
            rules,
            input: InputSummary::of(&evaluator.input),
            cost: evaluator.total,
        }
    }

    /// Sorts the input, marks `self` and attestor sets, keeps what the policy
    /// can use, and binds the CEL variables.
    #[allow(clippy::too_many_lines)]
    fn prepare(&self, input: &Input) -> Evaluator {
        let mut input = input.clone();
        let mut problems = Vec::new();
        input.change.paths.sort();
        input.change.paths.dedup();
        input
            .change
            .files
            .sort_by(|a, b| (&a.path, &a.old_path).cmp(&(&b.path, &b.old_path)));
        input.change.parents.sort();
        input.landing.overlap.sort();
        input.landing.overlap.dedup();
        let overlap = i64::try_from(input.landing.overlap.len()).unwrap_or(i64::MAX);
        input.landing.overlap_total = input.landing.overlap_total.max(overlap);
        input.landing.overlap.truncate(crate::MAX_OVERLAP_PATHS);
        input
            .evidence
            .sort_by(|a, b| (a.added_at, &a.id).cmp(&(b.added_at, &b.id)));
        input.provenance.sort_by(|a, b| {
            (a.recorded_at, a.sequence, &a.id).cmp(&(b.recorded_at, b.sequence, &b.id))
        });
        for e in &mut input.evidence {
            e.attestors.clear();
            e.artifacts
                .sort_by(|a, b| (&a.name, &a.sha256).cmp(&(&b.name, &b.sha256)));
        }
        if input.change.paths.len() > crate::MAX_PATHS
            || input.change.files.len() > crate::MAX_PATHS
        {
            problems.push(format!(
                "the change touches {} paths; a policy judges at most {}",
                input.change.paths.len().max(input.change.files.len()),
                crate::MAX_PATHS
            ));
            input.change.paths.truncate(crate::MAX_PATHS);
            input.change.files.truncate(crate::MAX_PATHS);
        }
        if input.provenance_ignored > 0 {
            problems.push(format!(
                "{} provenance records of the change were not read",
                input.provenance_ignored
            ));
        }
        mark_self(&mut input);

        let groups_value = env::to_value(&self.groups);
        let groups_json = serde_json::to_value(&self.groups).unwrap_or_default();
        let mut evaluator = Evaluator {
            sizes: Sizes::default(),
            context: env::context(Vec::new()),
            total: 0,
            diagnostics: Vec::new(),
            input_problem: None,
            input,
        };

        // Attestor sets: the declared keys first, then each set's `when`,
        // per object, over the evidence before it is capped.
        let declared: Vec<Vec<&String>> = evaluator
            .input
            .evidence
            .iter()
            .map(|e| {
                self.attestors
                    .iter()
                    .filter(|(_, set)| declared_match(set, e))
                    .map(|(name, _)| name)
                    .collect()
            })
            .collect();
        let mut when_context = env::context(vec![("groups", groups_value.clone())]);
        let mut reported: BTreeSet<&String> = BTreeSet::new();
        let mut matched: Vec<Vec<String>> = Vec::with_capacity(declared.len());
        for (index, names) in declared.iter().enumerate() {
            let mut sets = Vec::new();
            for name in names {
                let set = &self.attestors[*name];
                let Some(when) = &set.when else {
                    sets.push((*name).clone());
                    continue;
                };
                // The cost bound over this one object, exactly: junk elsewhere
                // on the tree cannot inflate it (review finding 2).
                let object = &evaluator.input.evidence[index];
                let when_sizes = {
                    let json = serde_json::to_value(object).unwrap_or_default();
                    Sizes::of(&[
                        ("evidence.*", &json, &shapes().e),
                        ("groups", &groups_json, &shapes().groups),
                    ])
                };
                when_context.add_variable_from_value("e", env::to_value(object));
                match evaluator.run(when, Some(&when_context), Some(&when_sizes)) {
                    Ok(true) => sets.push((*name).clone()),
                    Ok(false) => {}
                    Err(error) => {
                        // An object whose keys the set accepts, and whose
                        // condition could not be decided, may be the newest
                        // verdict: leaving it out could let an older one
                        // decide. So the evaluation blocks (review finding 2).
                        if reported.insert(*name) {
                            let id = &evaluator.input.evidence[index].id;
                            problems.push(format!(
                                "attestors.{name}.when failed on evidence {id}: {error}"
                            ));
                            evaluator.diagnostics.push(Diagnostic {
                                rule: format!("attestors.{name}"),
                                field: "when".to_owned(),
                                message: format!("evidence {id}: {error}"),
                                ..Diagnostic::default()
                            });
                        }
                    }
                }
            }
            matched.push(sets);
        }
        for (e, sets) in evaluator.input.evidence.iter_mut().zip(matched) {
            e.attestors = sets;
        }

        // Keep what can count: every object some set or sign-off rule
        // accepts, and the newest of the rest, so junk cannot push trusted
        // evidence out of view (S-94).
        let signoffs: Vec<&Signoff> = std::iter::once(&self.owners)
            .chain(self.entries.iter().filter_map(|entry| match entry {
                Entry::Signoff(signoff) => Some(signoff),
                _ => None,
            }))
            .collect();
        let counts = |e: &Evidence| {
            !e.attestors.is_empty() || signoffs.iter().any(|spec| is_signoff(spec, e))
        };
        let trusted = evaluator
            .input
            .evidence
            .iter()
            .filter(|e| counts(e))
            .count();
        if trusted > crate::EVIDENCE_TRUSTED_MAX {
            problems.push(format!(
                "{trusted} evidence objects on the tree count under this policy; it reads at most {}",
                crate::EVIDENCE_TRUSTED_MAX
            ));
        }
        let untrusted = evaluator.input.evidence.len() - trusted;
        let mut drop = untrusted.saturating_sub(crate::EVIDENCE_UNTRUSTED_MAX);
        let mut trusted_left = crate::EVIDENCE_TRUSTED_MAX;
        let dropped = i64::try_from(drop).unwrap_or(i64::MAX)
            + i64::try_from(trusted.saturating_sub(crate::EVIDENCE_TRUSTED_MAX)).unwrap_or(0);
        // Oldest first: drop the oldest untrusted, and trusted past the cap.
        evaluator.input.evidence.retain(|e| {
            if counts(e) {
                if trusted_left == 0 {
                    return false;
                }
                trusted_left -= 1;
                true
            } else if drop > 0 {
                drop -= 1;
                false
            } else {
                true
            }
        });
        evaluator.input.evidence_ignored = evaluator.input.evidence_ignored.saturating_add(dropped);

        // The CEL variables and their sizes.
        let change = serde_json::to_value(&evaluator.input.change).unwrap_or_default();
        let evidence = serde_json::to_value(&evaluator.input.evidence).unwrap_or_default();
        let provenance = serde_json::to_value(&evaluator.input.provenance).unwrap_or_default();
        let landing = serde_json::to_value(&evaluator.input.landing).unwrap_or_default();
        let batch = serde_json::to_value(&evaluator.input.batch).unwrap_or_default();
        let shapes = shapes();
        evaluator.sizes = Sizes::of(&[
            ("change", &change, &shapes.change),
            ("evidence", &evidence, &shapes.evidence),
            ("provenance", &provenance, &shapes.provenance),
            ("landing", &landing, &shapes.landing),
            ("batch", &batch, &shapes.batch),
            ("groups", &groups_json, &shapes.groups),
        ]);
        evaluator.context = env::context(vec![
            ("change", env::to_value(&evaluator.input.change)),
            ("evidence", env::to_value(&evaluator.input.evidence)),
            ("provenance", env::to_value(&evaluator.input.provenance)),
            ("landing", env::to_value(&evaluator.input.landing)),
            ("batch", env::to_value(&evaluator.input.batch)),
            ("groups", groups_value),
        ]);
        if !problems.is_empty() {
            evaluator.input_problem = Some(problems.join("; "));
        }
        evaluator
    }

    /// Why `e`, of a missing need's kind and target, did not count for
    /// `require`: the first reason that applies, over the rule's attestor
    /// sets (R3.13).
    fn rejection(&self, require: &Require, e: &Evidence) -> &'static str {
        if e.result != "pass" && e.result != "fail" {
            return "error";
        }
        match e.key.status.as_str() {
            "bound" => {}
            "unbound" => return "key_unbound",
            "expired" => return "key_expired",
            "revoked" => return "key_revoked",
            _ => return "key_invalid",
        }
        let sets = || {
            require
                .attestors
                .iter()
                .filter_map(|name| self.attestors.get(name))
        };
        if sets().any(|set| declared_match(set, e)) {
            return "condition";
        }
        let trusted_but = |set: &Attestor, presence: bool, self_ok: bool| {
            set.members.contains(&e.key.identity_id, &e.key.identity)
                && (set.identity_kinds.is_empty()
                    || set.identity_kinds.contains(&e.key.identity_kind))
                && set.actors.contains(&e.key.actor)
                && (set.authorities.is_empty() || set.authorities.contains(&e.key.authority))
                && (presence
                    || Presence::of_key(&e.key.presence)
                        .is_some_and(|presence| presence >= set.presence))
                && (self_ok || set.allow_self || !e.from_self)
        };
        if sets().any(|set| trusted_but(set, false, true)) {
            return "self";
        }
        if sets().any(|set| trusted_but(set, true, false)) {
            return "presence";
        }
        "untrusted"
    }

    #[expect(
        clippy::too_many_lines,
        reason = "each pair's newest verdict, then the message"
    )]
    fn require(&self, evaluator: &mut Evaluator, require: &Require) -> RuleResult {
        let covered: Vec<String> = require
            .filter
            .covered(&evaluator.input.change.paths)
            .into_iter()
            .cloned()
            .collect();
        if !require.filter.applies(&evaluator.input.change.paths) {
            return RuleResult::new(&require.name, RuleKind::Require, Status::NotApplicable);
        }
        match evaluator.condition(require.when.as_ref()) {
            Err(error) => {
                return RuleResult::new(&require.name, RuleKind::Require, Status::Error)
                    .with_message(format!("when: {error}"));
            }
            Ok(false) => {
                return RuleResult::new(&require.name, RuleKind::Require, Status::NotApplicable);
            }
            Ok(true) => {}
        }
        let targets: Vec<&str> = if require.targets.is_empty() {
            vec![""]
        } else {
            require.targets.iter().map(String::as_str).collect()
        };
        let mut result = RuleResult::new(&require.name, RuleKind::Require, Status::Satisfied)
            .with_paths(&covered);
        for kind in &require.kinds {
            for target in &targets {
                // The newest by witness time; at the same millisecond, a
                // failure wins (review finding 12).
                let newest = evaluator
                    .input
                    .evidence
                    .iter()
                    .filter(|e| {
                        e.kind == *kind
                            && e.target == *target
                            && (e.result == "pass" || e.result == "fail")
                            && e.attestors
                                .iter()
                                .any(|set| require.attestors.contains(set))
                    })
                    .max_by_key(|e| (e.added_at, e.result == "fail"));
                let need = |ids: Vec<String>| Need {
                    kind: kind.clone(),
                    target: (*target).to_owned(),
                    attestors: require.attestors.clone(),
                    evidence_ids: ids,
                    rejected: Vec::new(),
                    rejected_total: 0,
                };
                match newest {
                    None => {
                        let mut missing = need(Vec::new());
                        let rejected: Vec<Rejected> = evaluator
                            .input
                            .evidence
                            .iter()
                            .rev()
                            .filter(|e| e.kind == *kind && e.target == *target)
                            .map(|e| Rejected {
                                evidence_id: e.id.clone(),
                                why: self.rejection(require, e).to_owned(),
                            })
                            .collect();
                        missing.rejected_total = i64::try_from(rejected.len()).unwrap_or(i64::MAX);
                        missing.rejected = rejected.into_iter().take(crate::MAX_REJECTED).collect();
                        result.missing.push(missing);
                    }
                    Some(e) if e.result == "fail" => result.failed.push(need(vec![e.id.clone()])),
                    Some(e) => result.evidence_ids.push(e.id.clone()),
                }
            }
        }
        if !result.missing.is_empty() || !result.failed.is_empty() {
            result.status = Status::Unsatisfied;
            result.message = if require.message.is_empty() {
                let describe = |needs: &[Need]| {
                    needs
                        .iter()
                        .map(|need| {
                            if need.target.is_empty() {
                                need.kind.clone()
                            } else {
                                format!("{} ({})", need.kind, need.target)
                            }
                        })
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut parts = Vec::new();
                if !result.missing.is_empty() {
                    parts.push(format!(
                        "needs passing {} from {}",
                        describe(&result.missing),
                        require.attestors.join(" or ")
                    ));
                }
                if !result.failed.is_empty() {
                    parts.push(format!("{} failed", describe(&result.failed)));
                }
                parts.join("; ")
            } else {
                require.message.clone()
            };
        }
        result
    }

    #[allow(clippy::unused_self)]
    fn signoff(&self, evaluator: &mut Evaluator, spec: &Signoff, kind: RuleKind) -> RuleResult {
        let covered: Vec<String> = spec
            .filter
            .covered(&evaluator.input.change.paths)
            .into_iter()
            .cloned()
            .collect();
        let applies = if kind == RuleKind::Builtin {
            !covered.is_empty()
        } else {
            spec.filter.applies(&evaluator.input.change.paths)
        };
        if !applies {
            return RuleResult::new(&spec.name, kind, Status::NotApplicable);
        }
        match evaluator.condition(spec.when.as_ref()) {
            Err(error) => {
                return RuleResult::new(&spec.name, kind, Status::Error)
                    .with_message(format!("when: {error}"));
            }
            Ok(false) => return RuleResult::new(&spec.name, kind, Status::NotApplicable),
            Ok(true) => {}
        }
        let mut signers: BTreeMap<String, (String, String)> = BTreeMap::new();
        for e in &evaluator.input.evidence {
            if is_signoff(spec, e) {
                let key = if e.key.identity_id.is_empty() {
                    e.key.identity.clone()
                } else {
                    e.key.identity_id.clone()
                };
                let shown = if e.key.identity.is_empty() {
                    e.key.identity_id.clone()
                } else {
                    e.key.identity.clone()
                };
                signers.entry(key).or_insert((shown, e.id.clone()));
            }
        }
        let have = i64::try_from(signers.len()).unwrap_or(i64::MAX);
        let mut signed_by: Vec<String> = signers.values().map(|(name, _)| name.clone()).collect();
        signed_by.sort();
        let mut result = RuleResult::new(
            &spec.name,
            kind,
            if have >= spec.count {
                Status::Satisfied
            } else {
                Status::Unsatisfied
            },
        )
        .with_paths(&covered);
        result.evidence_ids = signers.into_values().map(|(_, id)| id).collect();
        result.evidence_ids.sort();
        result.signoff = Some(SignoffNeed {
            groups: spec.members.groups.clone(),
            identities: spec.members.identities.clone(),
            count: spec.count,
            have,
            signed_by,
            presence: spec.presence.as_str().to_owned(),
            kinds: spec.kinds.clone(),
        });
        if result.status == Status::Unsatisfied {
            result.message = if spec.message.is_empty() {
                let mut who: Vec<String> = spec.members.groups.clone();
                who.extend(spec.members.identities.iter().cloned());
                let what = if kind == RuleKind::Builtin {
                    "a change to reef-policy.toml or reef-checks.toml needs"
                } else {
                    "needs"
                };
                format!(
                    "{what} {} sign-off{} from {} ({} so far)",
                    spec.count,
                    if spec.count == 1 { "" } else { "s" },
                    who.join(", "),
                    have
                )
            } else {
                spec.message.clone()
            };
        }
        result
    }
}

/// `builtin:policy-file`: a change's own new policy must load.
fn policy_file(input: &Input) -> RuleResult {
    let touched = input
        .change
        .paths
        .iter()
        .any(|path| path.eq_ignore_ascii_case(crate::POLICY_PATH));
    let name = "builtin:policy-file";
    if !touched {
        return RuleResult::new(name, RuleKind::Builtin, Status::NotApplicable);
    }
    if !input.proposed_policy.present {
        return RuleResult::new(name, RuleKind::Builtin, Status::NotApplicable)
            .with_message("the change's tree has no reef-policy.toml (deleted, or only another case of the name); builtin:owners applies");
    }
    match input.proposed_policy.diagnostics.first() {
        None => RuleResult::new(name, RuleKind::Builtin, Status::Satisfied),
        Some(first) => {
            RuleResult::new(name, RuleKind::Builtin, Status::Unsatisfied).with_message(format!(
                "the change's reef-policy.toml does not load ({} problem{}): {}:{first}",
                input.proposed_policy.diagnostics.len(),
                if input.proposed_policy.diagnostics.len() == 1 {
                    ""
                } else {
                    "s"
                },
                crate::POLICY_PATH
            ))
        }
    }
}

fn limits_result(input: &Input, limits: &crate::format::Limits) -> RuleResult {
    let counted: Vec<&String> = input
        .change
        .paths
        .iter()
        .filter(|path| !limits.except.matches(path))
        .collect();
    let files: Vec<&crate::input::File> = input
        .change
        .files
        .iter()
        .filter(|file| !limits.except.matches(&file.path))
        .collect();
    let lines: i64 = files
        .iter()
        .map(|file| file.lines_added.saturating_add(file.lines_removed))
        .fold(0, i64::saturating_add);
    let mut problems = Vec::new();
    let mut large: Vec<&String> = Vec::new();
    if let Some(max) = limits.max_files
        && len(counted.len()) > max
    {
        problems.push(format!("{} files, over max_files {max}", counted.len()));
    }
    if let Some(max) = limits.max_lines
        && lines > max
    {
        problems.push(format!(
            "{lines} lines added and removed, over max_lines {max}"
        ));
    }
    if let Some(max) = limits.max_file_size {
        large = files
            .iter()
            .filter(|file| file.status != "deleted" && file.size > max)
            .map(|file| &file.path)
            .collect();
        if !large.is_empty() {
            problems.push(format!(
                "{} file{} over max_file_size {max} bytes",
                large.len(),
                if large.len() == 1 { "" } else { "s" }
            ));
        }
    }
    if problems.is_empty() {
        RuleResult::new("limits", RuleKind::Limits, Status::Satisfied)
    } else {
        RuleResult::new("limits", RuleKind::Limits, Status::Unsatisfied)
            .with_message(problems.join("; "))
            .with_paths(&large)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn costs_print_in_scientific_form() {
        assert_eq!(scientific(10_000_000), "1e7");
        assert_eq!(scientific(3_200_000_000), "3.2e9");
        assert_eq!(scientific(500), "500");
    }
}
