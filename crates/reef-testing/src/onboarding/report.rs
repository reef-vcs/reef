//! Medians over trials, as Markdown: the summary a run prints
//! (`results/latest.md`) and the dated rows `docs/onboarding-benchmark.md`
//! keeps per scenario.

use std::collections::BTreeMap;
use std::fmt::Write as _;

use super::run::{DEFAULT_REPOSITORY, DEFAULT_REVISION};
use super::{Outcome, Scenario, TrialResult};
use crate::agent::table::{
    cell, lossy, median, median_failures_last, number, opt, table_line, thousands,
};

/// The trials of one scenario that share a date and every pin: one row of
/// the results table.
#[derive(Debug, Clone, PartialEq)]
pub struct Row {
    /// The scenario.
    pub scenario: Scenario,
    /// UTC, `YYYY-MM-DD`.
    pub date: String,
    /// The model asked for.
    pub model: String,
    /// The Claude Code version, or `?`.
    pub claude_code_version: String,
    /// The Reef commit, or `?`.
    pub reef_commit: String,
    /// `linux` or `macos`.
    pub os: String,
    /// The repository the agents worked in, as [`super::Pins::repository`] gives
    /// it.
    pub repository: String,
    /// Trials in which the agent ran.
    pub trials: usize,
    /// Trials that failed before the agent started; not in the medians.
    pub setup_failures: usize,
    /// Trials that succeeded.
    pub successes: usize,
    /// Median tool calls to the first successful `reef ws run` (D: to the
    /// answer), a failed trial counting as infinitely many. `None` when
    /// that median is a failure.
    pub calls_to_success: Option<f64>,
    /// Median tool calls.
    pub tool_calls: Option<f64>,
    /// Median `reef` calls that exited non-zero.
    pub failed_reef_calls: Option<f64>,
    /// Median model round trips.
    pub num_turns: Option<f64>,
    /// Median input tokens, cached and uncached.
    pub input_tokens: Option<f64>,
    /// Median output tokens.
    pub output_tokens: Option<f64>,
    /// Median spend in US dollars.
    pub cost_usd: Option<f64>,
    /// Median wall time in seconds.
    pub wall_s: Option<f64>,
    /// Trials in which the agent ran `git worktree`, `stash`, or
    /// `checkout`.
    pub escaped_to_git: usize,
    /// Tool calls the permission policy refused, over all trials.
    pub permission_denials: u32,
    /// Free text for the Notes column: what changed since the last row.
    pub note: String,
}

/// How [`super::Pins::repository`] names the default repository; rows from any
/// other say so in their notes.
fn default_repository() -> String {
    format!("{DEFAULT_REPOSITORY}@{}", &DEFAULT_REVISION[..12])
}

/// Phase 1's target for a scenario: at most this many calls to success and
/// failed `reef` calls, as medians (`docs/dev-phases.md`). E, set up by the
/// plugin, is held to B's (R2.25).
pub fn target(scenario: Scenario) -> Option<(f64, f64)> {
    match scenario {
        Scenario::A => Some((8.0, 2.0)),
        Scenario::B | Scenario::E => Some((4.0, 0.0)),
        Scenario::C | Scenario::D => None,
    }
}

/// Groups trials into rows, ordered by scenario, then date.
pub fn rows(trials: &[TrialResult]) -> Vec<Row> {
    let mut groups: BTreeMap<_, Vec<&TrialResult>> = BTreeMap::new();
    for trial in trials {
        let key = (
            trial.scenario,
            trial.date.clone(),
            trial.pins.model.clone(),
            trial
                .pins
                .claude_code_version
                .clone()
                .unwrap_or_else(|| "?".to_owned()),
            trial
                .pins
                .reef_commit
                .clone()
                .unwrap_or_else(|| "?".to_owned()),
            trial.pins.os.clone(),
            trial.pins.repository.clone(),
        );
        groups.entry(key).or_default().push(trial);
    }
    groups
        .into_iter()
        .map(
            |(
                (scenario, date, model, claude_code_version, reef_commit, os, repository),
                trials,
            )| {
                let ran: Vec<&TrialResult> =
                    trials.iter().copied().filter(|t| t.agent_ran()).collect();
                let metric = |f: &dyn Fn(&TrialResult) -> Option<f64>| {
                    median(ran.iter().filter_map(|t| f(t)).collect())
                };
                Row {
                    scenario,
                    date,
                    model,
                    claude_code_version,
                    reef_commit,
                    os,
                    repository,
                    trials: ran.len(),
                    setup_failures: trials.len() - ran.len(),
                    successes: ran.iter().filter(|t| t.success).count(),
                    calls_to_success: median_failures_last(
                        ran.iter().map(|t| calls_to_success(t)).collect(),
                    ),
                    tool_calls: metric(&|t| t.metrics.as_ref().map(|m| f64::from(m.tool_calls))),
                    failed_reef_calls: metric(&|t| {
                        t.metrics.as_ref().map(|m| f64::from(m.failed_reef_calls))
                    }),
                    num_turns: metric(&|t| t.metrics.as_ref()?.num_turns.map(lossy)),
                    input_tokens: metric(&|t| t.metrics.as_ref().map(|m| lossy(m.input_tokens))),
                    output_tokens: metric(&|t| t.metrics.as_ref().map(|m| lossy(m.output_tokens))),
                    cost_usd: metric(&|t| t.metrics.as_ref()?.total_cost_usd),
                    wall_s: metric(&|t| t.metrics.as_ref().map(|m| lossy(m.wall_ms) / 1000.0)),
                    escaped_to_git: ran
                        .iter()
                        .filter(|t| {
                            t.metrics
                                .as_ref()
                                .is_some_and(|m| !m.git_escapes.is_empty())
                        })
                        .count(),
                    permission_denials: ran
                        .iter()
                        .filter_map(|t| t.metrics.as_ref())
                        .map(|m| m.permission_denials)
                        .sum(),
                    note: String::new(),
                }
            },
        )
        .collect()
}

/// A trial's calls to success: for A to C, the first successful `reef ws
/// run`, or every tool call if the agent got there another way (`ws
/// create`, an edit, `ws release`); for D, every tool call. `None` if the
/// trial failed.
fn calls_to_success(trial: &TrialResult) -> Option<f64> {
    if !trial.success {
        return None;
    }
    let metrics = trial.metrics.as_ref()?;
    let calls = match trial.scenario {
        Scenario::D => None,
        _ => metrics.calls_to_success,
    };
    Some(f64::from(calls.unwrap_or(metrics.tool_calls)))
}

/// The header of a scenario's table in `docs/onboarding-benchmark.md`.
pub const TABLE_HEADER: &str = "\
| Date | Model | Claude Code | Reef | OS | Success | Calls to success | Tool calls | Failed `reef` | Turns | Input tokens | Output tokens | Cost | Wall | Escaped to git | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |";

impl Row {
    /// The row as a Markdown table line under [`TABLE_HEADER`].
    pub fn markdown(&self) -> String {
        table_line(&self.cells())
    }

    /// The cells under [`TABLE_HEADER`], in order.
    fn cells(&self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.setup_failures > 0 {
            notes.push(format!(
                "{} setup failure(s) not counted",
                self.setup_failures
            ));
        }
        if self.permission_denials > 0 {
            notes.push(format!("{} permission denial(s)", self.permission_denials));
        }
        if self.repository != default_repository() {
            notes.push(format!("repository: {}", self.repository));
        }
        if !self.note.is_empty() {
            notes.push(self.note.clone());
        }
        let calls = match (self.calls_to_success, self.trials) {
            (Some(calls), _) => number(calls),
            (None, 0) => "–".to_owned(),
            (None, _) => "failed".to_owned(),
        };
        vec![
            self.date.clone(),
            self.model.clone(),
            self.claude_code_version.clone(),
            self.reef_commit.clone(),
            self.os.clone(),
            format!("{}/{}", self.successes, self.trials),
            calls,
            opt(self.tool_calls, number),
            opt(self.failed_reef_calls, number),
            opt(self.num_turns, number),
            opt(self.input_tokens, thousands),
            opt(self.output_tokens, thousands),
            opt(self.cost_usd, |c| format!("${c:.2}")),
            opt(self.wall_s, |s| format!("{s:.0} s")),
            format!("{}/{}", self.escaped_to_git, self.trials),
            cell(&notes.join("; ")),
        ]
    }

    /// For A and B: whether the medians meet Phase 1's target, in a
    /// sentence.
    pub fn criterion(&self) -> Option<String> {
        let (max_calls, max_failed) = target(self.scenario)?;
        let calls = self.calls_to_success;
        let failed = self.failed_reef_calls;
        let met = calls.is_some_and(|c| c <= max_calls) && failed.is_some_and(|f| f <= max_failed);
        Some(format!(
            "Scenario {} ({}): target at most {} calls to success and {} failed `reef` calls; median {} and {}: {}.",
            self.scenario,
            self.date,
            number(max_calls),
            number(max_failed),
            calls.map_or_else(|| "failed".to_owned(), number),
            failed.map_or_else(|| "–".to_owned(), number),
            if met { "met" } else { "not met" },
        ))
    }
}

/// `results/latest.md`: a row per scenario, the Phase 1 criterion lines,
/// and a line per trial.
pub fn summary(trials: &[TrialResult]) -> String {
    let mut out = String::from("# Onboarding benchmark: latest run\n\n");
    let mut header = TABLE_HEADER.lines();
    let _ = writeln!(out, "| Scenario {}", header.next().unwrap_or_default());
    let _ = writeln!(out, "| --- {}", header.next().unwrap_or_default());
    let rows = rows(trials);
    for row in &rows {
        let mut cells = vec![row.scenario.to_string()];
        cells.extend(row.cells());
        let _ = writeln!(out, "{}", table_line(&cells));
    }
    let criteria: Vec<String> = rows.iter().filter_map(Row::criterion).collect();
    if !criteria.is_empty() {
        out.push('\n');
        for line in criteria {
            let _ = writeln!(out, "- {line}");
        }
    }
    out.push_str("\n## Trials\n\n| Scenario | Trial | Outcome | Success | Calls to success | Tool calls | Failed `reef` | Cost | Verdict |\n| --- | ---: | --- | --- | ---: | ---: | ---: | ---: | --- |\n");
    for trial in trials {
        let outcome = match &trial.outcome {
            Outcome::SetupFailed { error } => format!("setup failed: {error}"),
            Outcome::Exited { code } => {
                let code = code.map_or_else(|| "signal".to_owned(), |c| c.to_string());
                match &trial.result_subtype {
                    Some(subtype) => format!("exit {code}, {subtype}"),
                    None => format!("exit {code}"),
                }
            }
            Outcome::TimedOut => "timed out".to_owned(),
        };
        let metrics = trial.metrics.as_ref();
        let _ = writeln!(
            out,
            "| {} | {} | {} | {} | {} | {} | {} | {} | {} |",
            trial.scenario,
            trial.trial,
            cell(&outcome),
            if trial.success { "yes" } else { "no" },
            metrics
                .and_then(|m| m.calls_to_success)
                .map_or_else(|| "–".to_owned(), |c| c.to_string()),
            metrics.map_or_else(|| "–".to_owned(), |m| m.tool_calls.to_string()),
            metrics.map_or_else(|| "–".to_owned(), |m| m.failed_reef_calls.to_string()),
            metrics
                .and_then(|m| m.total_cost_usd)
                .map_or_else(|| "–".to_owned(), |c| format!("${c:.2}")),
            trial
                .verdict
                .as_ref()
                .map_or_else(String::new, |v| cell(&v.reason)),
        );
    }
    out
}

/// The marker under each scenario's table in `docs/onboarding-benchmark.md`;
/// new rows go just above it.
pub fn marker(scenario: Scenario) -> String {
    format!("<!-- onboarding-rows:{scenario} -->")
}

/// Inserts each row above its scenario's marker in `doc`.
pub fn insert_rows(doc: &str, rows: &[Row]) -> Result<String, String> {
    let mut doc = doc.to_owned();
    for row in rows {
        let marker = marker(row.scenario);
        let at = doc
            .find(&marker)
            .ok_or_else(|| format!("no `{marker}` line in the results page"))?;
        doc.insert_str(at, &format!("{}\n", row.markdown()));
    }
    Ok(doc)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::onboarding::judge::Verdict;
    use crate::onboarding::stream::Metrics;
    use crate::onboarding::{Pins, SCHEMA};

    fn trial(scenario: Scenario, n: u32, calls: Option<u32>, success: bool) -> TrialResult {
        TrialResult {
            schema: SCHEMA,
            scenario,
            trial: n,
            date: "2026-10-24".to_owned(),
            started_at: "2026-10-24T00:00:00Z".to_owned(),
            pins: Pins {
                model: "claude-sonnet-5-5".to_owned(),
                claude_code_version: Some("2.1.310".to_owned()),
                reef_version: "reef 0.0.0 (abcdef123456, x)".to_owned(),
                reef_commit: Some("abcdef123456".to_owned()),
                os: "linux".to_owned(),
                repository: default_repository(),
                prompt: scenario.prompt().to_owned(),
            },
            outcome: Outcome::Exited { code: Some(0) },
            result_subtype: Some("success".to_owned()),
            budget_exceeded: false,
            metrics: Some(Metrics {
                calls_to_success: calls,
                tool_calls: calls.unwrap_or(20) + 1,
                failed_reef_calls: 1,
                input_tokens: 123_456,
                output_tokens: 2_345,
                total_cost_usd: Some(0.25),
                wall_ms: 61_000,
                num_turns: Some(7),
                ..Metrics::default()
            }),
            verdict: Some(Verdict {
                success,
                workspace: None,
                reason: "because".to_owned(),
            }),
            success,
            stream_file: None,
        }
    }

    #[test]
    fn a_failure_counts_as_infinitely_many_calls() {
        let trials = [
            trial(Scenario::A, 1, Some(5), true),
            trial(Scenario::A, 2, Some(9), true),
            trial(Scenario::A, 3, None, false),
        ];
        let row = &rows(&trials)[0];
        assert_eq!(row.calls_to_success, Some(9.0));
        assert_eq!(row.successes, 2);
        let criterion = row.criterion().unwrap();
        assert!(
            criterion.ends_with("median 9 and 1: not met."),
            "{criterion}"
        );

        let trials = [
            trial(Scenario::A, 1, Some(5), true),
            trial(Scenario::A, 2, None, false),
            trial(Scenario::A, 3, None, false),
        ];
        let row = &rows(&trials)[0];
        assert_eq!(row.calls_to_success, None);
        assert!(
            row.markdown().contains("| 1/3 | failed |"),
            "{}",
            row.markdown()
        );
    }

    #[test]
    fn success_without_ws_run_counts_every_call() {
        // `trial` gives a trial without `ws run` 21 tool calls.
        let row = &rows(&[trial(Scenario::A, 1, None, true)])[0];
        assert_eq!(row.calls_to_success, Some(21.0));
    }

    #[test]
    fn setup_failures_are_left_out_and_noted() {
        let mut failed = trial(Scenario::B, 3, None, false);
        failed.outcome = Outcome::SetupFailed {
            error: "clone".to_owned(),
        };
        failed.metrics = None;
        failed.verdict = None;
        let trials = [
            trial(Scenario::B, 1, Some(3), true),
            trial(Scenario::B, 2, Some(4), true),
            failed,
        ];
        let row = &rows(&trials)[0];
        assert_eq!((row.trials, row.setup_failures), (2, 1));
        assert_eq!(row.calls_to_success, Some(3.5));
        assert_eq!(
            row.markdown(),
            "| 2026-10-24 | claude-sonnet-5-5 | 2.1.310 | abcdef123456 | linux | 2/2 | 3.5 | 4.5 | 1 | 7 | 123,456 | 2,345 | $0.25 | 61 s | 0/2 | 1 setup failure(s) not counted |"
        );
        assert!(row.criterion().unwrap().ends_with("not met."));
    }

    #[test]
    fn another_repository_is_another_row() {
        let mut offline = trial(Scenario::A, 2, Some(4), true);
        offline.pins.repository = "reef-synth small, seed 1".to_owned();
        let rows = rows(&[trial(Scenario::A, 1, Some(5), true), offline]);
        assert_eq!(rows.len(), 2);
        assert!(!rows[0].markdown().contains("repository:"));
        assert!(
            rows[1]
                .markdown()
                .ends_with("| repository: reef-synth small, seed 1 |")
        );
    }

    #[test]
    fn rows_go_above_their_marker() {
        let doc = format!(
            "## A\n\n{TABLE_HEADER}\n{}\n\n## B\n\n{TABLE_HEADER}\n{}\n",
            marker(Scenario::A),
            marker(Scenario::B)
        );
        let rows = rows(&[trial(Scenario::B, 1, Some(2), true)]);
        let updated = insert_rows(&doc, &rows).unwrap();
        let b = updated.find("## B").unwrap();
        let row = updated.find("| 2026-10-24").unwrap();
        assert!(row > b && row < updated.find(&marker(Scenario::B)).unwrap());
        assert!(insert_rows("no markers", &rows).is_err());
    }

    #[test]
    fn summary_has_a_scenario_column_and_a_line_per_trial() {
        let trials = [
            trial(Scenario::A, 1, Some(5), true),
            trial(Scenario::D, 1, None, true),
        ];
        let summary = summary(&trials);
        assert!(summary.contains("| Scenario | Date |"), "{summary}");
        assert!(summary.contains("| A | 2026-10-24 |"), "{summary}");
        assert!(summary.contains("| D | 2026-10-24 |"), "{summary}");
        assert!(
            summary.contains("Scenario A (2026-10-24): target"),
            "{summary}"
        );
        assert_eq!(
            summary.matches("| exit 0, success |").count(),
            2,
            "{summary}"
        );
    }

    /// The results page has every scenario's table, headed as rows are
    /// written, with its marker under it.
    #[test]
    fn the_results_page_has_a_table_per_scenario() {
        let page = include_str!("../../../../docs/onboarding-benchmark.md");
        for scenario in Scenario::ALL {
            let table = format!("{TABLE_HEADER}\n");
            let at = page.find(&marker(scenario)).unwrap();
            let heading = page[..at].rfind("### Scenario ").unwrap();
            assert!(
                page[heading..at].contains(&table),
                "scenario {scenario}'s table is not headed as rows are written"
            );
            assert!(page[heading..].starts_with(&format!("### Scenario {scenario}: ")));
        }
    }
}
