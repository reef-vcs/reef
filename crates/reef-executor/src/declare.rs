//! Matching a job's pairs to declared checks (ADR 0060 decision 2).
//!
//! For each requested pair (a kind, and a target or the whole check), the
//! executor's own checks of that kind are tried first; only when none
//! matches, and `[repository] checks = "allow"`, the tree's. A pair nothing
//! matches is planned as `error` with a note.

use std::sync::Arc;

use serde::Serialize;

use crate::config::{Check, Config, Network, RepoChecks, RepositoryChecks, Source, target_ok};

/// A requested pair.
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Serialize)]
pub struct Pair {
    /// The evidence kind.
    pub kind: String,
    /// The target; `None` for the whole check.
    pub target: Option<String>,
}

impl Pair {
    /// `kind target`, or `kind` for the whole check.
    pub fn label(&self) -> String {
        match &self.target {
            Some(target) => format!("{} {target}", self.kind),
            None => self.kind.clone(),
        }
    }
}

/// The pairs of a job's spec: each kind with each target, or each kind
/// alone when no target is named, in the spec's order.
pub fn pairs(kinds: &[String], targets: &[String]) -> Vec<Pair> {
    kinds
        .iter()
        .flat_map(|kind| {
            if targets.is_empty() {
                vec![Pair {
                    kind: kind.clone(),
                    target: None,
                }]
            } else {
                targets
                    .iter()
                    .map(|target| Pair {
                        kind: kind.clone(),
                        target: Some(target.clone()),
                    })
                    .collect()
            }
        })
        .collect()
}

/// One check planned for a pair.
#[derive(Clone, Debug)]
pub struct Planned {
    /// The check.
    pub check: Arc<Check>,
    /// The argv, `{target}` substituted.
    pub argv: Vec<String>,
    /// The network it gets, after the repository cap.
    pub network: Network,
    /// The level it asked for, when the cap lowered it.
    pub lowered_from: Option<Network>,
}

/// What runs for a pair: its checks, or why none does.
#[derive(Clone, Debug)]
pub struct PairPlan {
    /// The pair.
    pub pair: Pair,
    /// The checks, executor's first; empty when nothing matched.
    pub checks: Vec<Planned>,
    /// Why the pair is `error` when nothing runs; a note on a lowered
    /// network otherwise.
    pub note: Option<String>,
}

/// The tree's declarations as the executor read them.
#[derive(Clone, Debug)]
pub enum TreeChecks {
    /// Not read: `[repository] checks = "deny"`.
    Denied,
    /// The tree has no `reef-checks.toml`.
    Absent,
    /// It did not parse; the message names why.
    Invalid(String),
    /// Its checks.
    Parsed(RepoChecks),
}

impl TreeChecks {
    /// The file's digest, when it parsed.
    pub fn digest(&self) -> Option<&str> {
        match self {
            TreeChecks::Parsed(checks) => Some(&checks.digest),
            _ => None,
        }
    }
}

/// Plans every pair.
pub fn plan(config: &Config, tree: &TreeChecks, pairs: &[Pair]) -> Vec<PairPlan> {
    let executor: Vec<Arc<Check>> = config.checks.iter().cloned().map(Arc::new).collect();
    let repository: Vec<Arc<Check>> = match tree {
        TreeChecks::Parsed(checks) => checks.checks.iter().cloned().map(Arc::new).collect(),
        _ => Vec::new(),
    };
    pairs
        .iter()
        .map(|pair| plan_pair(config, tree, &executor, &repository, pair))
        .collect()
}

fn plan_pair(
    config: &Config,
    tree: &TreeChecks,
    executor: &[Arc<Check>],
    repository: &[Arc<Check>],
    pair: &Pair,
) -> PairPlan {
    let target = pair.target.as_deref();
    let matching = |checks: &[Arc<Check>]| -> Vec<Arc<Check>> {
        checks
            .iter()
            .filter(|c| c.kind == pair.kind && c.accepts(target, &config.target_pattern))
            .cloned()
            .collect()
    };
    let error = |note: String| PairPlan {
        pair: pair.clone(),
        checks: Vec::new(),
        note: Some(note),
    };
    if let Some(target) = target
        && !target_ok(target, &config.target_pattern)
    {
        return error(format!(
            "target {target:?} does not match this executor's target pattern ({})",
            config.target_pattern.as_str()
        ));
    }
    let mut chosen = matching(executor);
    if chosen.is_empty() && config.repository.checks == RepositoryChecks::Allow {
        if let TreeChecks::Invalid(message) = tree {
            return error(format!("the tree's checks do not parse: {message}"));
        }
        chosen = matching(repository);
    }
    if chosen.is_empty() {
        let what = match target {
            Some(target) => format!("kind {} and target {target}", pair.kind),
            None => format!("kind {} as a whole (a check with `run`)", pair.kind),
        };
        let wheres = match (config.repository.checks, tree) {
            (RepositoryChecks::Deny, _) | (_, TreeChecks::Denied) => {
                "this executor's configuration (repository checks are denied)"
            }
            (_, TreeChecks::Absent) => {
                "this executor's configuration, and the tree has no reef-checks.toml"
            }
            _ => "this executor's configuration or the tree's reef-checks.toml",
        };
        return error(format!("no check for {what} in {wheres}"));
    }
    let mut notes = Vec::new();
    let checks = chosen
        .into_iter()
        .filter_map(|check| {
            let argv = check.argv(target)?;
            let (network, lowered_from) = if check.source == Source::Repository
                && check.network > config.sandbox.max_repository_network
            {
                notes.push(format!(
                    "check {} asked for network {} and got {} (max_repository_network)",
                    check.name,
                    check.network.as_str(),
                    config.sandbox.max_repository_network.as_str()
                ));
                (config.sandbox.max_repository_network, Some(check.network))
            } else {
                (check.network, None)
            };
            Some(Planned {
                check,
                argv,
                network,
                lowered_from,
            })
        })
        .collect();
    PairPlan {
        pair: pair.clone(),
        checks,
        note: (!notes.is_empty()).then(|| notes.join("; ")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config(text: &str) -> Config {
        Config::parse(text, None).unwrap()
    }

    fn tree(text: &str) -> TreeChecks {
        TreeChecks::Parsed(RepoChecks::parse(text.as_bytes()).unwrap())
    }

    fn pair(kind: &str, target: Option<&str>) -> Pair {
        Pair {
            kind: kind.into(),
            target: target.map(str::to_owned),
        }
    }

    const EXECUTOR: &str = r#"
[[check]]
name = "exec-test"
kind = "test"
targets = ["crates/*"]
run_target = ["exec", "{target}"]
"#;

    const REPO: &str = r#"
[[check]]
name = "repo-test"
kind = "test"
targets = ["crates/*", "web"]
run_target = ["repo", "{target}"]
network = "full"

[[check]]
name = "fmt"
kind = "lint"
run = ["fmt"]

[[check]]
name = "clippy"
kind = "lint"
run = ["clippy"]
"#;

    #[test]
    fn executor_checks_shadow_the_repository_s() {
        let plans = plan(
            &config(EXECUTOR),
            &tree(REPO),
            &[
                pair("test", Some("crates/a")),
                pair("test", Some("web")),
                pair("lint", None),
            ],
        );
        assert_eq!(plans[0].checks.len(), 1);
        assert_eq!(plans[0].checks[0].check.name, "exec-test");
        assert_eq!(plans[0].checks[0].argv, vec!["exec", "crates/a"]);
        assert_eq!(plans[1].checks[0].check.name, "repo-test");
        assert_eq!(plans[1].checks[0].check.source, Source::Repository);
        let lint: Vec<&str> = plans[2]
            .checks
            .iter()
            .map(|p| p.check.name.as_str())
            .collect();
        assert_eq!(lint, vec!["fmt", "clippy"]);
    }

    #[test]
    fn deny_ignores_the_repository_file() {
        let text = format!("[repository]\nchecks = \"deny\"\n{EXECUTOR}");
        let plans = plan(&config(&text), &tree(REPO), &[pair("test", Some("web"))]);
        assert!(plans[0].checks.is_empty());
        assert!(plans[0].note.as_deref().unwrap().contains("denied"));
        assert_eq!(config(&text).kinds(), vec!["test"]);
    }

    #[test]
    fn unmatched_and_unparsed_pairs_are_errors_with_a_note() {
        let plans = plan(
            &config(""),
            &TreeChecks::Absent,
            &[pair("build", None), pair("test", Some("$(id)"))],
        );
        assert!(
            plans[0]
                .note
                .as_deref()
                .unwrap()
                .contains("no reef-checks.toml")
        );
        assert!(plans[1].note.as_deref().unwrap().contains("target pattern"));
        let plans = plan(
            &config(""),
            &TreeChecks::Invalid("bad line 3".into()),
            &[pair("lint", None)],
        );
        assert!(plans[0].note.as_deref().unwrap().contains("bad line 3"));
    }

    #[test]
    fn a_repository_s_network_is_capped() {
        let plans = plan(&config(""), &tree(REPO), &[pair("test", Some("web"))]);
        assert_eq!(plans[0].checks[0].network, Network::None);
        assert_eq!(plans[0].checks[0].lowered_from, Some(Network::Full));
        assert!(
            plans[0]
                .note
                .as_deref()
                .unwrap()
                .contains("max_repository_network")
        );
        let loose = config("[sandbox]\nmax_repository_network = \"full\"\n");
        let plans = plan(&loose, &tree(REPO), &[pair("test", Some("web"))]);
        assert_eq!(plans[0].checks[0].network, Network::Full);
    }

    #[test]
    fn pairs_follow_the_spec() {
        let kinds = vec!["lint".to_owned(), "test".to_owned()];
        assert_eq!(pairs(&kinds, &[]).len(), 2);
        let both = pairs(&kinds, &["a".into(), "b".into()]);
        assert_eq!(both.len(), 4);
        assert_eq!(both[1].label(), "lint b");
    }
}
