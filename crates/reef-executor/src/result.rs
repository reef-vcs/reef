//! From a check's end to evidence (ADR 0060 decision 7).
//!
//! | How the check ended | Its result |
//! | --- | --- |
//! | exit 0 | `pass` |
//! | an exit code in `error_exit` | `error` |
//! | any other exit, or a signal the executor did not send | `fail` |
//! | stopped by the executor (timeout, budget, cancel, shutdown) | `error` |
//! | never started, or nothing matched the pair | `error` |
//!
//! Each requested pair gets one summary object with its kind and target:
//! `fail` if any of its checks failed, else `error` if any errored, else
//! `pass`. With more than one check, each also gets a finer object whose
//! target is `<target>#<check>` (`#<check>` for the whole check).

use std::time::SystemTime;

use reef_evidence::{Artifact, Claim, Outcome};
use serde_json::{Map, Value, json};

use crate::declare::{Pair, Planned};
use crate::run::{Ended, Ran};

/// The longest note kept in an annotation.
pub const MAX_NOTE: usize = 512;

/// One check that ran for a pair, and what it left.
#[derive(Clone, Debug)]
pub struct CheckRun {
    /// What was planned.
    pub planned: Planned,
    /// How it ran.
    pub ran: Ran,
    /// When it started and ended.
    pub started: SystemTime,
    /// When it ended.
    pub finished: SystemTime,
    /// Its log and declared artifacts, as uploaded (or not).
    pub artifacts: Vec<Artifact>,
    /// What went wrong with its artifacts, if anything.
    pub notes: Vec<String>,
}

impl CheckRun {
    /// Its result, by the table above.
    pub fn result(&self) -> Outcome {
        outcome_of(&self.ran.ended, &self.planned.check.error_exit)
    }
}

/// A check's result from how it ended.
pub fn outcome_of(ended: &Ended, error_exit: &[i32]) -> Outcome {
    match ended {
        Ended::Exited(0) => Outcome::Pass,
        Ended::Exited(code) if error_exit.contains(code) => Outcome::Error,
        Ended::Exited(_) | Ended::Signaled(_) => Outcome::Fail,
        Ended::Stopped(_) | Ended::NotStarted(_) => Outcome::Error,
    }
}

/// `fail` over `error` over `pass`; `error` for none.
pub fn combine(results: impl IntoIterator<Item = Outcome>) -> Outcome {
    let mut any = false;
    let mut error = false;
    for result in results {
        any = true;
        match result {
            Outcome::Fail => return Outcome::Fail,
            Outcome::Error => error = true,
            Outcome::Pass => {}
        }
    }
    if error || !any {
        Outcome::Error
    } else {
        Outcome::Pass
    }
}

/// What every object of a job's annotations carries.
#[derive(Clone, Debug)]
pub struct Context {
    /// The executor's name.
    pub executor: String,
    /// The job's id.
    pub job: String,
    /// The attempt.
    pub attempt: u32,
    /// The sandbox kind.
    pub sandbox: &'static str,
    /// Which hash checks ran.
    pub verify: &'static str,
    /// The tree's `reef-checks.toml` digest, when it was read.
    pub repository_checks: Option<String>,
}

/// The objects for one pair: its summary first, then finer objects when
/// more than one check ran. `note` is the plan's.
pub fn claims(
    context: &Context,
    pair: &Pair,
    runs: &[CheckRun],
    note: Option<&str>,
    at: SystemTime,
) -> Vec<Claim> {
    let started = runs.iter().map(|r| r.started).min().unwrap_or(at);
    let finished = runs.iter().map(|r| r.finished).max().unwrap_or(at);
    let mut notes: Vec<String> = note.into_iter().map(str::to_owned).collect();
    for run in runs {
        if let Ended::NotStarted(why) = &run.ran.ended {
            notes.push(format!(
                "check {} did not start: {why}",
                run.planned.check.name
            ));
        }
        notes.extend(run.notes.iter().cloned());
    }
    let mut artifacts: Vec<Artifact> = runs
        .iter()
        .flat_map(|r| r.artifacts.iter().cloned())
        .collect();
    artifacts.truncate(reef_evidence::MAX_ARTIFACTS);
    let summary = Claim {
        kind: pair.kind.clone(),
        result: combine(runs.iter().map(CheckRun::result)),
        target: pair.target.clone(),
        started_on: started,
        finished_on: finished.max(started),
        artifacts,
        annotations: annotations(context, runs, &notes),
    };
    let mut out = vec![summary];
    if runs.len() > 1 {
        for run in runs {
            let target = format!(
                "{}#{}",
                pair.target.as_deref().unwrap_or_default(),
                run.planned.check.name
            );
            if target.len() > reef_evidence::MAX_TARGET_BYTES {
                continue;
            }
            let mut artifacts = run.artifacts.clone();
            artifacts.truncate(reef_evidence::MAX_ARTIFACTS);
            out.push(Claim {
                kind: pair.kind.clone(),
                result: run.result(),
                target: Some(target),
                started_on: run.started,
                finished_on: run.finished.max(run.started),
                artifacts,
                annotations: annotations(context, std::slice::from_ref(run), &run.notes),
            });
        }
    }
    out
}

fn annotations(context: &Context, runs: &[CheckRun], notes: &[String]) -> Map<String, Value> {
    let mut map = Map::new();
    map.insert(
        "executor".into(),
        json!({ "name": context.executor, "version": crate::VERSION }),
    );
    map.insert("job".into(), json!(context.job));
    map.insert("attempt".into(), json!(context.attempt));
    map.insert("sandbox".into(), json!({ "kind": context.sandbox }));
    map.insert("verify".into(), json!(context.verify));
    if let Some(digest) = &context.repository_checks
        && runs
            .iter()
            .any(|r| r.planned.check.source == crate::config::Source::Repository)
    {
        map.insert("repository_checks".into(), json!(digest));
    }
    let checks: Vec<Value> = runs
        .iter()
        .map(|run| {
            let check = &run.planned.check;
            let mut entry = Map::new();
            entry.insert("name".into(), json!(check.name));
            entry.insert("source".into(), json!(check.source.as_str()));
            entry.insert("declaration".into(), json!(check.declaration));
            entry.insert("network".into(), json!(run.planned.network.as_str()));
            match &run.ran.ended {
                Ended::Exited(code) => {
                    entry.insert("exit".into(), json!(code));
                }
                Ended::Signaled(signal) => {
                    entry.insert("signal".into(), json!(signal));
                }
                Ended::Stopped(why) => {
                    entry.insert("stopped".into(), json!(why.as_str()));
                }
                Ended::NotStarted(_) => {
                    entry.insert("started".into(), json!(false));
                }
            }
            entry.insert(
                "wall_ms".into(),
                json!(u64::try_from(run.ran.wall.as_millis()).unwrap_or(u64::MAX)),
            );
            entry.insert(
                "cpu_ms".into(),
                json!(u64::try_from(run.ran.cpu.as_millis()).unwrap_or(u64::MAX)),
            );
            if check.cache {
                entry.insert("cache".into(), json!(true));
            }
            Value::Object(entry)
        })
        .collect();
    map.insert("checks".into(), Value::Array(checks));
    if !notes.is_empty() {
        map.insert("note".into(), json!(truncate(&notes.join("; "), MAX_NOTE)));
    }
    map
}

/// `text` cut to at most `max` bytes on a character boundary.
pub fn truncate(text: &str, max: usize) -> String {
    if text.len() <= max {
        return text.to_owned();
    }
    let mut end = max;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    text[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use super::*;
    use crate::config::{Config, Network};
    use crate::run::StopReason;

    fn run_of(name: &str, ended: Ended) -> CheckRun {
        let config = Config::parse(
            &format!(
                "[[check]]\nname = \"{name}\"\nkind = \"lint\"\nrun = [\"x\"]\nerror_exit = [75]\n"
            ),
            None,
        )
        .unwrap();
        let now = SystemTime::now();
        CheckRun {
            planned: Planned {
                check: Arc::new(config.checks[0].clone()),
                argv: vec!["x".into()],
                network: Network::None,
                lowered_from: None,
            },
            ran: Ran {
                ended,
                wall: Duration::from_millis(5),
                cpu: Duration::from_millis(1),
                logged: 0,
                dropped: 0,
            },
            started: now,
            finished: now,
            artifacts: Vec::new(),
            notes: Vec::new(),
        }
    }

    fn context() -> Context {
        Context {
            executor: "e".into(),
            job: "job_1".into(),
            attempt: 1,
            sandbox: "none",
            verify: "disk",
            repository_checks: None,
        }
    }

    #[test]
    fn the_table() {
        assert_eq!(outcome_of(&Ended::Exited(0), &[]), Outcome::Pass);
        assert_eq!(outcome_of(&Ended::Exited(1), &[]), Outcome::Fail);
        assert_eq!(outcome_of(&Ended::Exited(75), &[75]), Outcome::Error);
        assert_eq!(outcome_of(&Ended::Signaled(11), &[]), Outcome::Fail);
        assert_eq!(
            outcome_of(&Ended::Stopped(StopReason::Timeout), &[]),
            Outcome::Error
        );
        assert_eq!(
            outcome_of(&Ended::NotStarted("x".into()), &[]),
            Outcome::Error
        );
    }

    #[test]
    fn fail_over_error_over_pass() {
        use Outcome::{Error, Fail, Pass};
        assert_eq!(combine([Pass, Error, Fail]), Fail);
        assert_eq!(combine([Pass, Error]), Error);
        assert_eq!(combine([Pass, Pass]), Pass);
        assert_eq!(combine([]), Error);
    }

    #[test]
    fn one_summary_and_finer_objects_for_several_checks() {
        let pair = Pair {
            kind: "lint".into(),
            target: None,
        };
        let runs = [
            run_of("fmt", Ended::Exited(0)),
            run_of("clippy", Ended::Exited(1)),
        ];
        let claims = claims(&context(), &pair, &runs, None, SystemTime::now());
        assert_eq!(claims.len(), 3);
        assert_eq!(claims[0].result, Outcome::Fail);
        assert_eq!(claims[0].target, None);
        assert_eq!(claims[1].target.as_deref(), Some("#fmt"));
        assert_eq!(claims[2].result, Outcome::Fail);
        let checks = claims[0].annotations["checks"].as_array().unwrap();
        assert_eq!(checks[1]["exit"], 1);
        assert_eq!(checks[1]["source"], "executor");
        assert_eq!(claims[0].annotations["sandbox"]["kind"], "none");
    }

    #[test]
    fn a_pair_with_no_check_is_an_error_with_its_note() {
        let pair = Pair {
            kind: "test".into(),
            target: Some("a".into()),
        };
        let claims = claims(&context(), &pair, &[], Some("no check"), SystemTime::now());
        assert_eq!(claims.len(), 1);
        assert_eq!(claims[0].result, Outcome::Error);
        assert_eq!(claims[0].annotations["note"], "no check");
    }
}
