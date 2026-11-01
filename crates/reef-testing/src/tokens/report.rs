//! Results as Markdown: the rows `docs/token-benchmark.md` keeps per table,
//! the summary a run writes (`results/latest.md`), and the Phase 2 exit
//! criterion's verdict.

use std::collections::{BTreeMap, BTreeSet};
use std::fmt::Write as _;

use super::Arm;
use super::count::COUNTERS;
use super::live::TrialResult;
use super::scripted::{CandidateResult, SWEEP_VIEWS, ScriptedResult, cheapest};
use crate::agent::process::Outcome;
use crate::agent::table::{cell, insert_above, lossy, median, number, opt, table_line, thousands};

/// The question the exit criterion is evaluated on.
pub const GATE_QUESTION: &str = "state-repo";
/// How many times cheaper than git a Reef arm must be.
pub const GATE_RATIO: f64 = 5.0;
/// Correct trials (of three) a live cell needs.
pub const MIN_CORRECT: usize = 2;

/// The Reef arm numbers the page reports, beside git.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Column {
    /// Git's cheapest sufficient candidate.
    Git,
    /// The CLI's cheapest sufficient candidate without `-o text`: what an
    /// agent's shell gets unasked.
    ReefDefault,
    /// The CLI's cheapest sufficient candidate with `-o text`; shown, never
    /// gated.
    ReefText,
    /// The MCP server's cheapest sufficient call sequence.
    Mcp,
}

/// The arm number for `column` on `question` by `counter`, with the
/// candidate it came from.
pub fn arm_number<'a>(
    result: &'a ScriptedResult,
    question: &str,
    column: Column,
    counter: &str,
) -> Option<(u64, &'a CandidateResult)> {
    let candidates = &result.candidates;
    let best = match column {
        Column::Git => cheapest(candidates, question, Arm::Git, counter, |_| true),
        Column::ReefDefault => {
            cheapest(candidates, question, Arm::Reef, counter, |c| !c.text_output)
        }
        Column::ReefText => cheapest(candidates, question, Arm::Reef, counter, |c| c.text_output),
        Column::Mcp => cheapest(candidates, question, Arm::Mcp, counter, |_| true),
    }?;
    Some((best.counts[counter], best))
}

fn ratio(git: u64, reef: u64) -> Option<f64> {
    (reef > 0).then(|| lossy(git) / lossy(reef))
}

/// The scripted table's header.
pub const SCRIPTED_HEADER: &str = "\
| Date | Reef | Fixture | Question | Counter | git | reef (default) | reef (text) | mcp | mcp + fixed | git ÷ best gated | Notes |
| --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- |";

/// The scripted table's rows: one per question and counter that ran.
pub fn scripted_rows(result: &ScriptedResult, note: &str) -> Vec<String> {
    let mut rows = Vec::new();
    let questions: Vec<&str> = ordered_questions(result);
    for question in questions {
        for counter in COUNTERS {
            if !result
                .candidates
                .iter()
                .any(|c| c.counts.contains_key(counter))
            {
                continue;
            }
            let get = |column| arm_number(result, question, column, counter).map(|(n, _)| n);
            let fixed = result.fixed.counts.get(counter).copied();
            let git = get(Column::Git);
            let default = get(Column::ReefDefault);
            let mcp = get(Column::Mcp);
            let mcp_fixed = mcp.zip(fixed).map(|(m, f)| m + f);
            let best = [default, mcp_fixed].into_iter().flatten().min();
            let cells = vec![
                result.date.clone(),
                short(result.reef_commit.as_deref()),
                result.fixture_version.clone(),
                format!("`{question}`"),
                counter.to_owned(),
                count_cell(git),
                count_cell(default),
                count_cell(get(Column::ReefText)),
                count_cell(mcp),
                count_cell(mcp_fixed),
                opt(git.zip(best).and_then(|(g, b)| ratio(g, b)), |r| {
                    format!("{r:.2}×")
                }),
                cell(note),
            ];
            rows.push(table_line(&cells));
        }
    }
    rows
}

fn ordered_questions(result: &ScriptedResult) -> Vec<&str> {
    let mut seen = Vec::new();
    for candidate in &result.candidates {
        if !seen.contains(&candidate.question.as_str()) {
            seen.push(candidate.question.as_str());
        }
    }
    seen
}

fn count_cell(value: Option<u64>) -> String {
    value.map_or_else(|| "–".to_owned(), |v| thousands(lossy(v)))
}

fn short(commit: Option<&str>) -> String {
    commit.map_or_else(|| "?".to_owned(), |c| c.get(..12).unwrap_or(c).to_owned())
}

/// The fixed cost table's header.
pub const FIXED_HEADER: &str = "\
| Date | Reef | Tools | `tools/list` bytes | Instructions bytes | bytes | anthropic | o200k | Break-even (questions) | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | --- | --- |";

/// How many questions a session must answer through `mcp` rather than the
/// CLI's default output to repay the fixed cost, by `counter`: the fixed
/// cost over the median saving per question. `None` when `mcp` saves
/// nothing (it never pays back).
pub fn break_even(result: &ScriptedResult, counter: &str) -> Option<f64> {
    let fixed = *result.fixed.counts.get(counter)?;
    let savings: Vec<f64> = ordered_questions(result)
        .into_iter()
        .filter_map(|q| {
            let default = arm_number(result, q, Column::ReefDefault, counter)?.0;
            let mcp = arm_number(result, q, Column::Mcp, counter)?.0;
            Some(lossy(default) - lossy(mcp))
        })
        .collect();
    let saving = median(savings)?;
    (saving > 0.0).then(|| lossy(fixed) / saving)
}

/// The fixed cost row.
pub fn fixed_row(result: &ScriptedResult, note: &str) -> String {
    let fixed = &result.fixed;
    let even: Vec<String> = COUNTERS
        .iter()
        .filter(|c| fixed.counts.contains_key(**c))
        .map(|c| match break_even(result, c) {
            Some(n) => format!("{c} {}", number((n * 10.0).round() / 10.0)),
            None => format!("{c} never"),
        })
        .collect();
    table_line(&[
        result.date.clone(),
        short(result.reef_commit.as_deref()),
        fixed.tools.to_string(),
        thousands(lossy(fixed.tools_list_bytes)),
        thousands(lossy(fixed.instructions_bytes)),
        count_cell(fixed.counts.get("bytes").copied()),
        count_cell(fixed.counts.get("anthropic").copied()),
        count_cell(fixed.counts.get("o200k").copied()),
        even.join(", "),
        cell(note),
    ])
}

/// The sweep table's header.
pub const SWEEP_HEADER: &str = "\
| Date | Reef | Commits | `git show` | `git show --stat` | `reef diff` | `reef diff --stat` | `reef diff --symbols` | `--symbols -o text` | `--symbols` ÷ `reef diff` | Notes |
| --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |";

/// The sweep row, in median bytes per view.
pub fn sweep_row(result: &ScriptedResult, note: &str) -> Option<String> {
    if result.sweep.is_empty() {
        return None;
    }
    let medians: Vec<String> = SWEEP_VIEWS
        .iter()
        .map(|view| {
            let values = result
                .sweep
                .iter()
                .filter_map(|c| c.views.get(*view)?.get("bytes").copied().map(lossy))
                .collect();
            opt(median(values), thousands)
        })
        .collect();
    let ratios = result
        .sweep
        .iter()
        .filter_map(|c| {
            let symbols = c.views.get("reef diff --symbols")?.get("bytes")?;
            let plain = c.views.get("reef diff")?.get("bytes")?;
            ratio(*symbols, *plain)
        })
        .collect();
    let mut cells = vec![
        result.date.clone(),
        short(result.reef_commit.as_deref()),
        result.sweep.len().to_string(),
    ];
    cells.extend(medians);
    cells.push(opt(median(ratios), |r| format!("{r:.2}")));
    cells.push(cell(note));
    Some(table_line(&cells))
}

/// The breakdown table's header.
pub const BREAKDOWN_HEADER: &str = "\
| Date | Reef | Question | Candidate | Field | Bytes | Share |
| --- | --- | --- | --- | --- | ---: | ---: |";

/// The breakdown rows: for the state questions, the CLI default candidate's
/// JSON by field, the eight largest per output.
pub fn breakdown_rows(result: &ScriptedResult) -> Vec<String> {
    let mut rows = Vec::new();
    for question in ["state-own", "state-repo"] {
        let Some((_, candidate)) = arm_number(result, question, Column::ReefDefault, "bytes")
        else {
            continue;
        };
        let mut merged: BTreeMap<String, u64> = BTreeMap::new();
        for output in &candidate.breakdown {
            for share in output {
                *merged.entry(share.field.clone()).or_default() += share.bytes;
            }
        }
        let total: u64 = merged.values().sum();
        let mut fields: Vec<(String, u64)> = merged.into_iter().collect();
        fields.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(&b.0)));
        for (field, bytes) in fields.into_iter().take(8) {
            rows.push(table_line(&[
                result.date.clone(),
                short(result.reef_commit.as_deref()),
                format!("`{question}`"),
                cell(&candidate.name),
                format!("`{field}`"),
                thousands(lossy(bytes)),
                format!("{:.0}%", 100.0 * lossy(bytes) / lossy(total.max(1))),
            ]));
        }
    }
    rows
}

/// The live table's header, one table per harness.
pub const LIVE_HEADER: &str = "\
| Date | Model | Harness | Reef | OS | Question | Arm | Correct | Answer tokens | Answer bytes | Session overhead | Billed input | Cache read | Output | Cost | Calls | Turns | Wall | Escapes | Notes |
| --- | --- | --- | --- | --- | --- | --- | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | ---: | --- |";

/// One live cell: the trials of one (question, arm) that share a date and
/// every pin.
#[derive(Debug, Clone, PartialEq)]
pub struct LiveCell {
    /// UTC date.
    pub date: String,
    /// The model.
    pub model: String,
    /// The harness and its version.
    pub harness: String,
    /// The harness's version, or `?`.
    pub harness_version: String,
    /// The Reef commit, or `?`.
    pub reef_commit: String,
    /// `linux` or `macos`.
    pub os: String,
    /// The question.
    pub question: String,
    /// The arm.
    pub arm: Arm,
    /// Trials that counted (the agent ran, the fixture held).
    pub trials: usize,
    /// Of those, correct.
    pub correct: usize,
    /// Trials kept out: setup failures and mutations.
    pub left_out: usize,
    /// Median answer tokens over correct trials; `None` under
    /// [`MIN_CORRECT`].
    pub answer_tokens: Option<f64>,
    /// The counter `answer_tokens` is in.
    pub counter: String,
    /// Median answer bytes over correct trials.
    pub answer_bytes: Option<f64>,
    /// Median first-request input tokens.
    pub session_overhead: Option<f64>,
    /// Median billed input, cached and not.
    pub billed_input: Option<f64>,
    /// Median cache reads.
    pub cache_read: Option<f64>,
    /// Median output tokens.
    pub output: Option<f64>,
    /// Median cost.
    pub cost: Option<f64>,
    /// Median tool calls.
    pub calls: Option<f64>,
    /// Median turns.
    pub turns: Option<f64>,
    /// Median wall seconds.
    pub wall_s: Option<f64>,
    /// Trials with an escape.
    pub escaped: usize,
}

/// Groups trials into cells, ordered by harness, question order, arm.
pub fn live_cells(trials: &[TrialResult], question_order: &[String]) -> Vec<LiveCell> {
    let mut groups: BTreeMap<_, Vec<&TrialResult>> = BTreeMap::new();
    for trial in trials {
        let order = question_order
            .iter()
            .position(|q| *q == trial.question)
            .unwrap_or(usize::MAX);
        let key = (
            trial.pins.harness.clone(),
            trial.date.clone(),
            trial.pins.model.clone(),
            trial
                .pins
                .harness_version
                .clone()
                .unwrap_or_else(|| "?".to_owned()),
            trial
                .pins
                .reef_commit
                .clone()
                .unwrap_or_else(|| "?".to_owned()),
            trial.pins.os.clone(),
            order,
            trial.question.clone(),
            trial.arm,
        );
        groups.entry(key).or_default().push(trial);
    }
    groups
        .into_iter()
        .map(
            |(
                (harness, date, model, harness_version, reef_commit, os, _, question, arm),
                trials,
            )| {
                let counted: Vec<&TrialResult> =
                    trials.iter().copied().filter(|t| t.counts()).collect();
                let correct: Vec<&TrialResult> =
                    counted.iter().copied().filter(|t| t.correct).collect();
                let enough = correct.len() >= MIN_CORRECT;
                // Over correct trials, and only with a value from at least
                // `MIN_CORRECT` of them: a counter that failed on one trial
                // must not leave a median of one.
                let over_correct = |f: &dyn Fn(&TrialResult) -> Option<f64>| {
                    let values: Vec<f64> = correct.iter().filter_map(|t| f(t)).collect();
                    if enough && values.len() >= MIN_CORRECT {
                        median(values)
                    } else {
                        None
                    }
                };
                let over_counted = |f: &dyn Fn(&TrialResult) -> Option<f64>| {
                    median(counted.iter().filter_map(|t| f(t)).collect())
                };
                LiveCell {
                    date,
                    model,
                    harness,
                    harness_version,
                    reef_commit,
                    os,
                    question,
                    arm,
                    trials: counted.len(),
                    correct: correct.len(),
                    left_out: trials.len() - counted.len(),
                    answer_tokens: over_correct(&|t| t.answer.as_ref()?.tokens.map(lossy)),
                    counter: correct
                        .iter()
                        .find_map(|t| t.answer.as_ref().map(|a| a.counter.clone()))
                        .unwrap_or_default(),
                    answer_bytes: over_correct(&|t| t.answer.as_ref().map(|a| lossy(a.bytes))),
                    session_overhead: over_counted(&|t| t.session_overhead.map(lossy)),
                    billed_input: over_counted(&|t| t.billed.map(|u| lossy(u.total_input()))),
                    cache_read: over_counted(&|t| {
                        t.billed.map(|u| lossy(u.cache_read_input_tokens))
                    }),
                    output: over_counted(&|t| t.billed.map(|u| lossy(u.output_tokens))),
                    cost: over_counted(&|t| t.cost_usd),
                    calls: over_counted(&|t| t.answer.as_ref().map(|a| f64::from(a.calls))),
                    turns: over_counted(&|t| t.turns.map(lossy)),
                    wall_s: over_counted(&|t| Some(lossy(t.wall_ms) / 1000.0)),
                    escaped: counted.iter().filter(|t| !t.escapes.is_empty()).count(),
                }
            },
        )
        .collect()
}

impl LiveCell {
    /// The cell as a line under [`LIVE_HEADER`].
    pub fn markdown(&self, note: &str) -> String {
        let mut notes = Vec::new();
        if self.left_out > 0 {
            notes.push(format!("{} left out (setup or mutated)", self.left_out));
        }
        if self.correct < MIN_CORRECT {
            notes.push("not answered reliably".to_owned());
        }
        if !note.is_empty() {
            notes.push(note.to_owned());
        }
        table_line(&[
            self.date.clone(),
            self.model.clone(),
            format!("{} {}", self.harness, self.harness_version),
            self.reef_commit.clone(),
            self.os.clone(),
            format!("`{}`", self.question),
            self.arm.to_string(),
            format!("{}/{}", self.correct, self.trials),
            opt(self.answer_tokens, thousands),
            opt(self.answer_bytes, thousands),
            opt(self.session_overhead, thousands),
            opt(self.billed_input, thousands),
            opt(self.cache_read, thousands),
            opt(self.output, thousands),
            opt(self.cost, |c| format!("${c:.3}")),
            opt(self.calls, number),
            opt(self.turns, number),
            opt(self.wall_s, |s| format!("{s:.0} s")),
            format!("{}/{}", self.escaped, self.trials),
            cell(&notes.join("; ")),
        ])
    }
}

/// The exit criterion's verdict.
#[derive(Debug, Clone, PartialEq)]
pub enum Verdict {
    /// Met by `arm`, with the scripted ratios and the live ratio per
    /// harness.
    Met {
        /// The gated arm that met it.
        arm: Arm,
        /// One sentence with the numbers.
        detail: String,
    },
    /// Not met, with the numbers.
    NotMet(String),
    /// Not evaluated, and why (a counter or the live half missing).
    NotEvaluated(String),
}

impl Verdict {
    /// The verdict in one line.
    pub fn line(&self) -> String {
        match self {
            Verdict::Met { arm, detail } => format!("met by the `{arm}` arm: {detail}"),
            Verdict::NotMet(detail) => format!("not met: {detail}"),
            Verdict::NotEvaluated(why) => format!("not evaluated: {why}"),
        }
    }
}

/// The newest live cell of the gate question for `harness` and `arm`.
fn newest<'a>(live: &'a [LiveCell], harness: &str, arm: Arm) -> Option<&'a LiveCell> {
    live.iter()
        .filter(|c| c.question == GATE_QUESTION && c.harness == harness && c.arm == arm)
        .max_by(|a, b| a.date.cmp(&b.date))
}

/// The harnesses measured on the gate question, or why the verdict cannot
/// be evaluated: a counter missing, the gate question not run, an arm with
/// no live cell, or answer tokens not counted.
fn gate_inputs<'a>(
    scripted: &ScriptedResult,
    live: &'a [LiveCell],
) -> Result<BTreeSet<&'a str>, String> {
    let missing: Vec<&str> = COUNTERS
        .iter()
        .copied()
        .filter(|c| {
            !scripted
                .candidates
                .iter()
                .any(|r| r.counts.contains_key(*c))
        })
        .collect();
    if !missing.is_empty() {
        return Err(format!("no `{}` counts", missing.join("`, `")));
    }
    if !scripted
        .candidates
        .iter()
        .any(|c| c.question == GATE_QUESTION)
    {
        return Err(format!("`{GATE_QUESTION}` was not in the scripted run"));
    }
    // Per harness, the newest live cells of the gate question.
    let gate: Vec<&LiveCell> = live
        .iter()
        .filter(|c| c.question == GATE_QUESTION)
        .collect();
    let harnesses: BTreeSet<&str> = gate.iter().map(|c| c.harness.as_str()).collect();
    if harnesses.is_empty() {
        return Err(format!("no live trials of `{GATE_QUESTION}`"));
    }
    for harness in &harnesses {
        for arm in [Arm::Git, Arm::Reef, Arm::Mcp] {
            match newest(live, harness, arm) {
                None => {
                    return Err(format!(
                        "no live `{arm}` trials of `{GATE_QUESTION}` in {harness}"
                    ));
                }
                Some(cell) if cell.correct >= MIN_CORRECT && cell.answer_tokens.is_none() => {
                    return Err(format!(
                        "answer tokens were not counted for `{arm}` in {harness}"
                    ));
                }
                Some(_) => {}
            }
        }
    }
    Ok(harnesses)
}

/// Evaluates the exit criterion on [`GATE_QUESTION`]: some gated Reef arm
/// (`reef (default)`, or `mcp` with its fixed cost once, scripted and live)
/// is at most a fifth of git under every counter, and in every harness
/// measured its median live answer tokens are at most a fifth of git's,
/// both cells with at least two correct trials.
pub fn verdict(scripted: &ScriptedResult, live: &[LiveCell]) -> Verdict {
    let harnesses = match gate_inputs(scripted, live) {
        Ok(harnesses) => harnesses,
        Err(why) => return Verdict::NotEvaluated(why),
    };
    let mut details = Vec::new();
    for (arm, column) in [(Arm::Reef, Column::ReefDefault), (Arm::Mcp, Column::Mcp)] {
        let mut scripted_ratios = Vec::new();
        for counter in COUNTERS {
            let git = arm_number(scripted, GATE_QUESTION, Column::Git, counter).map(|(n, _)| n);
            let mut reef = arm_number(scripted, GATE_QUESTION, column, counter).map(|(n, _)| n);
            if arm == Arm::Mcp {
                reef = reef
                    .zip(scripted.fixed.counts.get(counter))
                    .map(|(r, f)| r + f);
            }
            scripted_ratios.push((counter, git.zip(reef).and_then(|(g, r)| ratio(g, r))));
        }
        let mut live_ratios = Vec::new();
        for harness in &harnesses {
            let cell_of = |arm: Arm| newest(live, harness, arm);
            let git = cell_of(Arm::Git).and_then(|c| c.answer_tokens);
            // `mcp` pays its fixed cost live too, once per session, in the
            // counter its answer tokens are in (#122).
            let reef = cell_of(arm).and_then(|c| {
                let tokens = c.answer_tokens?;
                if arm == Arm::Mcp {
                    let fixed = scripted.fixed.counts.get(&c.counter)?;
                    Some(tokens + lossy(*fixed))
                } else {
                    Some(tokens)
                }
            });
            live_ratios.push((
                *harness,
                git.zip(reef).and_then(|(g, r)| (r > 0.0).then(|| g / r)),
            ));
        }
        let scripted_ok = scripted_ratios
            .iter()
            .all(|(_, r)| r.is_some_and(|r| r >= GATE_RATIO));
        let live_ok = live_ratios
            .iter()
            .all(|(_, r)| r.is_some_and(|r| r >= GATE_RATIO));
        let show = |r: &Option<f64>| r.map_or_else(|| "–".to_owned(), |r| format!("{r:.2}×"));
        let detail = format!(
            "`{arm}`{}: scripted {}; live {}",
            if arm == Arm::Mcp {
                " with its fixed cost"
            } else {
                " (default output)"
            },
            scripted_ratios
                .iter()
                .map(|(c, r)| format!("{} by {c}", show(r)))
                .collect::<Vec<_>>()
                .join(", "),
            live_ratios
                .iter()
                .map(|(h, r)| format!("{} in {h}", show(r)))
                .collect::<Vec<_>>()
                .join(", "),
        );
        if scripted_ok && live_ok {
            return Verdict::Met { arm, detail };
        }
        details.push(detail);
    }
    let one_harness = if harnesses.len() == 1 {
        "; one harness: a gap, not a proof"
    } else {
        ""
    };
    Verdict::NotMet(format!(
        "git ÷ Reef on `{GATE_QUESTION}`, at least {GATE_RATIO:.0}× needed; {}{one_harness}",
        details.join("; ")
    ))
}

/// The marker under a table in `docs/token-benchmark.md`.
pub fn marker(table: &str) -> String {
    format!("<!-- tokens-rows:{table} -->")
}

/// Adds every row to its table in `doc`. Rows already there are not added
/// again.
pub fn insert_rows(
    doc: &str,
    scripted: Option<&ScriptedResult>,
    live: &[LiveCell],
    note: &str,
) -> Result<String, String> {
    let mut doc = doc.to_owned();
    if let Some(result) = scripted {
        for row in scripted_rows(result, note) {
            insert_above(&mut doc, &marker("scripted"), &row)?;
        }
        insert_above(&mut doc, &marker("fixed"), &fixed_row(result, note))?;
        if let Some(row) = sweep_row(result, note) {
            insert_above(&mut doc, &marker("sweep"), &row)?;
        }
        for row in breakdown_rows(result) {
            insert_above(&mut doc, &marker("breakdown"), &row)?;
        }
    }
    for cell in live {
        insert_above(
            &mut doc,
            &marker(&format!("live-{}", cell.harness)),
            &cell.markdown(note),
        )?;
    }
    Ok(doc)
}

/// `results/latest.md`: every table, the chosen candidates, the verdict,
/// and a line per trial.
#[expect(clippy::too_many_lines, reason = "one section after another")]
pub fn summary(
    scripted: Option<&ScriptedResult>,
    trials: &[TrialResult],
    live: &[LiveCell],
) -> String {
    let mut out = String::from("# Token-efficiency benchmark: latest run\n");
    if let Some(result) = scripted {
        let _ = writeln!(out, "\n## Scripted\n\n{SCRIPTED_HEADER}");
        for row in scripted_rows(result, "") {
            let _ = writeln!(out, "{row}");
        }
        if !result.counters_missing.is_empty() {
            let _ = writeln!(out);
            for (name, why) in &result.counters_missing {
                let _ = writeln!(out, "- `{name}` did not run: {why}.");
            }
        }
        let _ = writeln!(out, "\n### Chosen candidates (by bytes)\n");
        for question in ordered_questions(result) {
            for (column, label) in [
                (Column::Git, "git"),
                (Column::ReefDefault, "reef (default)"),
                (Column::ReefText, "reef (text)"),
                (Column::Mcp, "mcp"),
            ] {
                let chosen = arm_number(result, question, column, "bytes").map_or_else(
                    || "none sufficient".to_owned(),
                    |(_, c)| {
                        format!(
                            "{} ({})",
                            c.name,
                            c.steps
                                .iter()
                                .map(|s| format!("`{}`", s.call))
                                .collect::<Vec<_>>()
                                .join(", ")
                        )
                    },
                );
                let _ = writeln!(out, "- `{question}` {label}: {}", cell(&chosen));
            }
        }
        let insufficient: Vec<&CandidateResult> =
            result.candidates.iter().filter(|c| !c.sufficient).collect();
        if !insufficient.is_empty() {
            let _ = writeln!(out, "\n### Insufficient candidates\n");
            for c in insufficient {
                let why = c
                    .insufficient
                    .clone()
                    .unwrap_or_else(|| format!("lacks {}", c.missing.join(", ")));
                let _ = writeln!(
                    out,
                    "- `{}` {} {}: {}",
                    c.question,
                    c.arm,
                    c.name,
                    cell(&why)
                );
            }
        }
        let _ = writeln!(
            out,
            "\n## Fixed cost\n\n{FIXED_HEADER}\n{}",
            fixed_row(result, "")
        );
        if let Some(row) = sweep_row(result, "") {
            let _ = writeln!(out, "\n## Diff sweep\n\n{SWEEP_HEADER}\n{row}");
        }
        if let Some(error) = &result.sweep_error {
            let _ = writeln!(out, "\n## Diff sweep\n\nFailed: {}", cell(error));
        }
        let rows = breakdown_rows(result);
        if !rows.is_empty() {
            let _ = writeln!(out, "\n## Where the bytes go\n\n{BREAKDOWN_HEADER}");
            for row in rows {
                let _ = writeln!(out, "{row}");
            }
        }
    }
    if !live.is_empty() {
        let _ = writeln!(out, "\n## Live\n\n{LIVE_HEADER}");
        for cell in live {
            let _ = writeln!(out, "{}", cell.markdown(""));
        }
        let _ = writeln!(
            out,
            "\n### Trials\n\n| Question | Arm | Trial | Outcome | Correct | Answer | Tokens | Cost | Escapes |\n| --- | --- | ---: | --- | --- | --- | ---: | ---: | --- |"
        );
        for trial in trials {
            let outcome = match &trial.outcome {
                Outcome::SetupFailed { error } => format!("setup failed: {error}"),
                Outcome::Exited { code } => format!(
                    "exit {}{}{}",
                    code.map_or_else(|| "signal".to_owned(), |c| c.to_string()),
                    trial
                        .result_subtype
                        .as_deref()
                        .map(|s| format!(", {s}"))
                        .unwrap_or_default(),
                    if trial.mutated { ", mutated" } else { "" }
                ),
                Outcome::TimedOut => "timed out".to_owned(),
            };
            let _ = writeln!(
                out,
                "| `{}` | {} | {} | {} | {} | {} | {} | {} | {} |",
                trial.question,
                trial.arm,
                trial.trial,
                cell(&outcome),
                if trial.correct { "yes" } else { "no" },
                cell(
                    &trial
                        .grade
                        .as_ref()
                        .map(|g| g.answer.clone())
                        .unwrap_or_default()
                ),
                trial
                    .answer
                    .as_ref()
                    .and_then(|a| a.tokens)
                    .map_or_else(|| "–".to_owned(), |t| thousands(lossy(t))),
                trial
                    .cost_usd
                    .map_or_else(|| "–".to_owned(), |c| format!("${c:.3}")),
                cell(&trial.escapes.join(" ; ")),
            );
        }
    }
    if let Some(result) = scripted {
        let _ = writeln!(
            out,
            "\n## Exit criterion\n\n{}",
            verdict(result, live).line()
        );
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::live::{AnswerCost, Pins};
    use crate::tokens::scripted::FixedCost;

    fn candidate(
        question: &str,
        arm: Arm,
        name: &str,
        text: bool,
        counts: [u64; 3],
    ) -> CandidateResult {
        CandidateResult {
            question: question.to_owned(),
            arm,
            name: name.to_owned(),
            text_output: text,
            steps: Vec::new(),
            counts: COUNTERS
                .iter()
                .map(|c| (*c).to_owned())
                .zip(counts)
                .collect(),
            sufficient: true,
            missing: Vec::new(),
            insufficient: None,
            breakdown: Vec::new(),
        }
    }

    fn scripted(reef: [u64; 3], mcp: [u64; 3]) -> ScriptedResult {
        let mut result = ScriptedResult::new("reef 0.2.0 (abcdef123456, x)", "m", Vec::new());
        result.date = "2026-10-30".to_owned();
        result.fixed = FixedCost {
            counts: COUNTERS.iter().map(|c| ((*c).to_owned(), 100)).collect(),
            ..FixedCost::default()
        };
        result.candidates = vec![
            candidate(GATE_QUESTION, Arm::Git, "g", false, [5_000, 1_000, 1_000]),
            candidate(GATE_QUESTION, Arm::Reef, "r", false, reef),
            candidate(GATE_QUESTION, Arm::Reef, "t", true, [10, 1, 1]),
            candidate(GATE_QUESTION, Arm::Mcp, "m", false, mcp),
        ];
        result
    }

    fn trial(arm: Arm, n: u32, tokens: u64, correct: bool) -> TrialResult {
        TrialResult {
            schema: 1,
            question: GATE_QUESTION.to_owned(),
            arm,
            trial: n,
            date: "2026-10-30".to_owned(),
            started_at: String::new(),
            pins: Pins {
                harness: "claude-code".to_owned(),
                harness_version: Some("1".to_owned()),
                model: "m".to_owned(),
                reef_version: String::new(),
                reef_commit: Some("abcdef123456".to_owned()),
                os: "linux".to_owned(),
                fixture_version: "f".to_owned(),
                prompt: String::new(),
            },
            outcome: Outcome::Exited { code: Some(0) },
            result_subtype: Some("success".to_owned()),
            budget_exceeded: false,
            grade: None,
            correct,
            answer: Some(AnswerCost {
                calls: 2,
                bytes: tokens * 4,
                tokens: Some(tokens),
                counter: "anthropic".to_owned(),
            }),
            session_overhead: None,
            billed: None,
            cost_usd: None,
            turns: None,
            wall_ms: 1_000,
            escapes: Vec::new(),
            mutated: false,
            stream_file: None,
        }
    }

    fn cells(git: [u64; 3], reef: [u64; 3], reef_correct: [bool; 3]) -> Vec<LiveCell> {
        let mut trials = Vec::new();
        for (n, t) in (1..).zip(git) {
            trials.push(trial(Arm::Git, n, t, true));
        }
        for (n, (t, ok)) in (1..).zip(reef.into_iter().zip(reef_correct)) {
            trials.push(trial(Arm::Reef, n, t, ok));
        }
        for n in 1..=3 {
            trials.push(trial(Arm::Mcp, n, 5_000, true));
        }
        live_cells(&trials, &[GATE_QUESTION.to_owned()])
    }

    /// Test 8: medians over correct trials only, `—` under two correct;
    /// the gate needs every counter at five and the live median at five;
    /// `mcp` carries its fixed cost; text output never satisfies it.
    #[test]
    fn medians_and_gate() {
        let live = cells([1_000, 1_200, 900], [100, 150, 9_999], [true, true, false]);
        let reef = live.iter().find(|c| c.arm == Arm::Reef).unwrap();
        assert_eq!(reef.correct, 2);
        assert_eq!(reef.answer_tokens, Some(125.0));
        let shaky = cells([1_000, 1_200, 900], [100, 150, 200], [true, false, false]);
        let shaky = shaky.iter().find(|c| c.arm == Arm::Reef).unwrap();
        assert_eq!(shaky.answer_tokens, None);
        assert!(shaky.markdown("").contains("not answered reliably"));

        // Every counter at least five times cheaper, live too: met.
        let good = scripted([900, 150, 190], [9_000, 9_000, 9_000]);
        assert!(matches!(
            verdict(&good, &live),
            Verdict::Met { arm: Arm::Reef, .. }
        ));
        // One counter short: not met.
        let short = scripted([900, 150, 250], [9_000, 9_000, 9_000]);
        assert!(matches!(verdict(&short, &live), Verdict::NotMet(_)));
        // The live half short: not met, and one harness is called a gap.
        let weak_live = cells([1_000, 1_200, 900], [400, 300, 500], [true; 3]);
        let said = verdict(&good, &weak_live).line();
        assert!(
            said.starts_with("not met") && said.contains("a gap, not a proof"),
            "{said}"
        );
        // Text output alone (10 bytes) never meets it.
        let text_only = scripted([4_000, 900, 900], [9_000, 9_000, 9_000]);
        assert!(matches!(verdict(&text_only, &live), Verdict::NotMet(_)));
        // `mcp` carries its fixed cost of 100: 150 + 100 is more than a
        // fifth of `anthropic`'s 1,000.
        let mcp = scripted([9_000, 9_000, 9_000], [150, 150, 150]);
        assert!(matches!(verdict(&mcp, &live), Verdict::NotMet(_)));
        // A counter missing: not evaluated.
        let mut partial = good.clone();
        for c in &mut partial.candidates {
            c.counts.remove("o200k");
        }
        assert!(matches!(verdict(&partial, &live), Verdict::NotEvaluated(_)));
        assert!(matches!(verdict(&good, &[]), Verdict::NotEvaluated(_)));

        // Live, `mcp` carries its fixed cost too (#122): 150 alone is a
        // fifth of git's 1,000 and less, but 150 + 100 is not; 90 + 100 is.
        let cheap_mcp = scripted([9_000, 9_000, 9_000], [10, 10, 10]);
        let mcp_live = |tokens: u64| {
            let mut trials = Vec::new();
            for (n, t) in (1..).zip([1_000, 1_200, 900]) {
                trials.push(trial(Arm::Git, n, t, true));
            }
            for n in 1..=3 {
                trials.push(trial(Arm::Reef, n, 5_000, true));
                trials.push(trial(Arm::Mcp, n, tokens, true));
            }
            live_cells(&trials, &[GATE_QUESTION.to_owned()])
        };
        assert!(matches!(
            verdict(&cheap_mcp, &mcp_live(150)),
            Verdict::NotMet(_)
        ));
        assert!(matches!(
            verdict(&cheap_mcp, &mcp_live(90)),
            Verdict::Met { arm: Arm::Mcp, .. }
        ));

        // The gate question not in the scripted run: not evaluated.
        let mut other = good.clone();
        for c in &mut other.candidates {
            c.question = "state-own".to_owned();
        }
        assert!(matches!(verdict(&other, &live), Verdict::NotEvaluated(_)));
        // An arm with no live cell: not evaluated.
        let no_mcp: Vec<LiveCell> = live.iter().filter(|c| c.arm != Arm::Mcp).cloned().collect();
        assert!(matches!(verdict(&good, &no_mcp), Verdict::NotEvaluated(_)));
        // An unreliable live cell is answered, and not met.
        let shaky = cells([1_000, 1_200, 900], [100, 150, 200], [true, false, false]);
        assert!(matches!(verdict(&good, &shaky), Verdict::NotMet(_)));
        // Correct trials whose tokens were not counted: not evaluated.
        let mut uncounted = live.clone();
        for c in &mut uncounted {
            c.answer_tokens = None;
        }
        assert!(matches!(
            verdict(&good, &uncounted),
            Verdict::NotEvaluated(_)
        ));
        // The newest cells decide: an older run that met it does not count.
        let mut older = live.clone();
        for c in &mut older {
            c.date = "2026-10-01".to_owned();
        }
        older.extend(weak_live.iter().cloned());
        assert!(matches!(verdict(&good, &older), Verdict::NotMet(_)));
    }

    /// Test 9: rows land at their markers; a rerun adds nothing; a
    /// missing marker is an error that names it.
    #[test]
    fn rows_insert() {
        let result = scripted([900, 150, 190], [9_000, 9_000, 9_000]);
        let live = cells([1_000, 1_200, 900], [100, 150, 200], [true; 3]);
        let page = include_str!("../../../../docs/token-benchmark.md");
        let once = insert_rows(page, Some(&result), &live, "first").unwrap();
        let twice = insert_rows(&once, Some(&result), &live, "first").unwrap();
        assert_eq!(once, twice);
        let scripted_at = once.find(&marker("scripted")).unwrap();
        let row = once.find("| 2026-10-30 | abcdef123456 |").unwrap();
        assert!(row < scripted_at);
        let error = insert_rows("no markers", Some(&result), &live, "").unwrap_err();
        assert!(error.contains("tokens-rows:scripted"), "{error}");
        let error = insert_rows(
            &page.replace(&marker("live-claude-code"), ""),
            None,
            &live,
            "",
        )
        .unwrap_err();
        assert!(error.contains("tokens-rows:live-claude-code"), "{error}");
    }

    /// The results page has each table headed as rows are written, with
    /// its marker under it, and one live table for the shipped harness.
    #[test]
    fn the_results_page_has_every_table() {
        let page = include_str!("../../../../docs/token-benchmark.md");
        for (table, header) in [
            ("scripted", SCRIPTED_HEADER),
            ("fixed", FIXED_HEADER),
            ("sweep", SWEEP_HEADER),
            ("breakdown", BREAKDOWN_HEADER),
            ("live-claude-code", LIVE_HEADER),
        ] {
            let at = page
                .find(&marker(table))
                .unwrap_or_else(|| panic!("no {table} marker"));
            let start = page[..at].rfind("\n\n").unwrap();
            assert!(
                page[start..at].contains(header),
                "{table}'s table is not headed as rows are written"
            );
        }
    }

    #[test]
    fn break_even_is_fixed_over_the_median_saving() {
        let mut result = scripted([900, 150, 190], [400, 100, 100]);
        // Saving 500 bytes a question, fixed 100: a fifth of a question.
        assert_eq!(break_even(&result, "bytes"), Some(0.2));
        result.candidates.retain(|c| c.arm != Arm::Mcp);
        result.candidates.push(candidate(
            GATE_QUESTION,
            Arm::Mcp,
            "m",
            false,
            [2_000, 2_000, 2_000],
        ));
        assert_eq!(break_even(&result, "bytes"), None);
    }
}
