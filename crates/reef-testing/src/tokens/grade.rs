//! Grading an answer line against the truth. Correct means exact, after
//! normalizing what two honest answers may spell differently: case,
//! Markdown emphasis and backquotes, a leading `./`, a symbol's module or
//! type, `()`, generic parameters, `main` for landed, `in progress` for
//! open, and a subject's trailing ` (#NNN)`.

use std::collections::BTreeSet;

use serde::{Deserialize, Serialize};

/// How a question's answer line reads.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    /// `<A|M|D> <path>; ... | base: <subject>`.
    Files,
    /// `<agent> <count>; ...`.
    Counts,
    /// `added <names>; removed <names>; changed <names>`.
    Symbols,
    /// `<agent> <landed|open>; ...`.
    Agents,
}

/// A parsed answer line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Answer {
    /// Changed files and the base commit's subject.
    Files {
        /// (status letter, path) pairs.
        files: BTreeSet<(char, String)>,
        /// The subject, normalized.
        base: String,
    },
    /// (agent, changed file count) pairs.
    Counts(BTreeSet<(String, u64)>),
    /// Names by category.
    Symbols {
        /// Added.
        added: BTreeSet<String>,
        /// Removed.
        removed: BTreeSet<String>,
        /// Changed, or moved.
        changed: BTreeSet<String>,
    },
    /// (agent, `landed` or `open`) pairs.
    Agents(BTreeSet<(String, String)>),
}

impl Answer {
    /// The items precision and recall are taken over: each pair, the base
    /// subject, or each name in the union of the categories.
    fn items(&self) -> BTreeSet<String> {
        match self {
            Answer::Files { files, base } => files
                .iter()
                .map(|(status, path)| format!("{status} {path}"))
                .chain([format!("base: {base}")])
                .collect(),
            Answer::Counts(pairs) => pairs.iter().map(|(a, n)| format!("{a} {n}")).collect(),
            Answer::Symbols {
                added,
                removed,
                changed,
            } => added
                .iter()
                .chain(removed)
                .chain(changed)
                .cloned()
                .collect(),
            Answer::Agents(pairs) => pairs.iter().map(|(a, o)| format!("{a} {o}")).collect(),
        }
    }

    /// The names the truth mentions, as they would appear in any output
    /// that answers it: agents, paths, symbol names, the base subject.
    pub fn names(&self) -> BTreeSet<String> {
        match self {
            Answer::Files { files, base } => files
                .iter()
                .map(|(_, path)| path.clone())
                .chain([base.clone()])
                .collect(),
            Answer::Counts(pairs) => pairs.iter().map(|(a, _)| a.clone()).collect(),
            Answer::Symbols { .. } => self.items(),
            Answer::Agents(pairs) => pairs.iter().map(|(a, _)| a.clone()).collect(),
        }
    }
}

/// A trial's grade.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Grade {
    /// Exactly right.
    pub correct: bool,
    /// Of the items the answer gave, the share that the truth has.
    pub precision: f64,
    /// Of the truth's items, the share the answer gave.
    pub recall: f64,
    /// For symbol questions, whether every name was also in the right
    /// category; reported, not required.
    pub categories_right: Option<bool>,
    /// The answer line as read, or why there was none.
    pub answer: String,
}

impl Kind {
    /// Parses an answer line (with or without its `ANSWER:` prefix).
    pub fn parse(self, line: &str) -> Result<Answer, String> {
        let line = clean(line);
        let body = strip_prefix_ci(&line, "answer:").unwrap_or(&line).trim();
        match self {
            Kind::Files => {
                let (list, base) = body.rsplit_once('|').ok_or("no `| base:` part")?;
                let base = strip_prefix_ci(base.trim(), "base:").ok_or("no `base:`")?;
                let mut files = BTreeSet::new();
                for item in items(list) {
                    let (status, path) = item
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| format!("`{item}` has no status"))?;
                    files.insert((status_letter(status)?, normalize_path(path)));
                }
                Ok(Answer::Files {
                    files,
                    base: normalize_subject(base),
                })
            }
            Kind::Counts => {
                let mut pairs = BTreeSet::new();
                for item in items(body) {
                    // `bot-4 4`, `bot-4: 4 files`, `bot-4 (4 files)`.
                    let item = item.to_lowercase().replace(['(', ')'], " ");
                    let bare = item
                        .trim_end()
                        .trim_end_matches("files")
                        .trim_end_matches("file")
                        .trim_end();
                    let (agent, count) = bare
                        .rsplit_once(char::is_whitespace)
                        .ok_or_else(|| format!("`{item}` has no count"))?;
                    let count = count
                        .trim()
                        .parse()
                        .map_err(|_| format!("`{item}`: no number"))?;
                    pairs.insert((normalize_agent(agent), count));
                }
                Ok(Answer::Counts(pairs))
            }
            Kind::Symbols => {
                let mut added = BTreeSet::new();
                let mut removed = BTreeSet::new();
                let mut changed = BTreeSet::new();
                for part in body.split(';') {
                    let part = part.trim();
                    if part.is_empty() {
                        continue;
                    }
                    let (category, names) = part
                        .split_once(|c: char| c.is_whitespace() || c == ':')
                        .unwrap_or((part, ""));
                    let target = match category.to_lowercase().as_str() {
                        "added" => &mut added,
                        "removed" | "deleted" => &mut removed,
                        "changed" | "modified" | "moved" => &mut changed,
                        other => return Err(format!("unknown category `{other}`")),
                    };
                    target.extend(
                        names
                            .split(',')
                            .map(normalize_symbol)
                            .filter(|n| !n.is_empty() && n != "-"),
                    );
                }
                Ok(Answer::Symbols {
                    added,
                    removed,
                    changed,
                })
            }
            Kind::Agents => {
                let mut pairs = BTreeSet::new();
                for item in items(body) {
                    let (agent, outcome) = item
                        .split_once(char::is_whitespace)
                        .ok_or_else(|| format!("`{item}` has no outcome"))?;
                    pairs.insert((normalize_agent(agent), normalize_outcome(outcome)?));
                }
                Ok(Answer::Agents(pairs))
            }
        }
    }

    /// Grades an agent's final text against the truth line.
    pub fn grade(self, final_text: &str, truth: &str) -> Grade {
        let Ok(truth) = self.parse(truth) else {
            return Grade {
                correct: false,
                precision: 0.0,
                recall: 0.0,
                categories_right: None,
                answer: "the truth does not parse".to_owned(),
            };
        };
        let Some(line) = answer_line(final_text) else {
            return Grade {
                correct: false,
                precision: 0.0,
                recall: 0.0,
                categories_right: None,
                answer: "no ANSWER: line".to_owned(),
            };
        };
        let answer = match self.parse(&line) {
            Ok(answer) => answer,
            Err(error) => {
                return Grade {
                    correct: false,
                    precision: 0.0,
                    recall: 0.0,
                    categories_right: None,
                    answer: format!("{line} (unreadable: {error})"),
                };
            }
        };
        let given = answer.items();
        let wanted = truth.items();
        let hits = given.intersection(&wanted).count();
        let precision = ratio(hits, given.len());
        let recall = ratio(hits, wanted.len());
        let categories_right = matches!(self, Kind::Symbols).then(|| answer == truth);
        Grade {
            correct: given == wanted,
            precision,
            recall,
            categories_right,
            answer: line,
        }
    }
}

#[allow(clippy::cast_precision_loss, reason = "a few dozen items")]
fn ratio(hits: usize, of: usize) -> f64 {
    if of == 0 {
        if hits == 0 { 1.0 } else { 0.0 }
    } else {
        hits as f64 / of as f64
    }
}

/// The last line of `text` that starts with `ANSWER:`, cleaned of Markdown.
pub fn answer_line(text: &str) -> Option<String> {
    text.lines()
        .map(clean)
        .rfind(|line| strip_prefix_ci(line, "answer:").is_some())
}

/// A line without emphasis, backquotes, double quotes, a quote marker or a
/// list bullet.
fn clean(line: &str) -> String {
    let line: String = line
        .chars()
        .filter(|c| !matches!(c, '*' | '`' | '"'))
        .collect();
    line.trim().trim_start_matches(['>', '-']).trim().to_owned()
}

fn strip_prefix_ci<'a>(text: &'a str, prefix: &str) -> Option<&'a str> {
    text.get(..prefix.len())
        .filter(|head| head.eq_ignore_ascii_case(prefix))
        .map(|_| &text[prefix.len()..])
}

/// The `;`-separated items of a list, trimmed, without empty ones.
fn items(list: &str) -> impl Iterator<Item = &str> {
    list.split(';')
        .map(str::trim)
        .filter(|item| !item.is_empty())
}

fn status_letter(status: &str) -> Result<char, String> {
    match status.trim().to_lowercase().as_str() {
        "a" | "added" | "??" | "new" => Ok('A'),
        "m" | "modified" | "changed" => Ok('M'),
        "d" | "deleted" | "removed" => Ok('D'),
        other => Err(format!("unknown status `{other}`")),
    }
}

fn normalize_path(path: &str) -> String {
    path.trim().trim_start_matches("./").to_lowercase()
}

fn normalize_agent(agent: &str) -> String {
    agent.trim().trim_end_matches(':').to_lowercase()
}

/// `landed` or `open` from an outcome as written: a synonym, or a phrase
/// whose first word is one (`landed (on main)`).
fn normalize_outcome(outcome: &str) -> Result<String, String> {
    let outcome = outcome.to_lowercase().replace(['(', ')', ','], " ");
    let phrase = outcome.split_whitespace().collect::<Vec<_>>().join(" ");
    let first = phrase.split(' ').next().unwrap_or_default();
    let known = |text: &str| {
        matches!(
            text,
            "landed"
                | "main"
                | "on main"
                | "merged"
                | "open"
                | "in progress"
                | "in-progress"
                | "wip"
                | "not yet"
        )
    };
    let text = if known(&phrase) {
        phrase.as_str()
    } else {
        first
    };
    match text {
        "landed" | "main" | "on main" | "merged" => Ok("landed".to_owned()),
        "open" | "in progress" | "in-progress" | "wip" | "not yet" => Ok("open".to_owned()),
        other => Err(format!("unknown outcome `{other}`")),
    }
}

/// A subject without a trailing ` (#NNN)`, lowercased.
fn normalize_subject(subject: &str) -> String {
    let subject = subject.trim();
    let without = match subject.rsplit_once(" (#") {
        Some((head, tail))
            if tail.ends_with(')')
                && tail[..tail.len() - 1].chars().all(|c| c.is_ascii_digit()) =>
        {
            head
        }
        _ => subject,
    };
    without.trim().to_lowercase()
}

/// A symbol name's last segment after `::`, `.`, `#` or `/`, without `()`
/// or generic parameters, lowercased.
fn normalize_symbol(name: &str) -> String {
    // Generic parameters out first, nested ones included, so
    // `Current<'a>::holds` keeps `holds`.
    let mut depth = 0_u32;
    let name: String = name
        .trim()
        .chars()
        .filter(|c| match c {
            '<' => {
                depth += 1;
                false
            }
            '>' => {
                depth = depth.saturating_sub(1);
                false
            }
            _ => depth == 0,
        })
        .collect();
    let name = name.split('(').next().unwrap_or(&name);
    let last = name
        .rsplit(['#', '.', '/'])
        .next()
        .unwrap_or(name)
        .rsplit("::")
        .next()
        .unwrap_or(name);
    last.trim().to_lowercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tokens::scenario::Scenario;

    fn truth(id: &str) -> (Kind, String) {
        let scenario = Scenario::load().unwrap();
        let question = scenario.question(id).unwrap();
        (question.kind, question.truth.clone())
    }

    fn grade(id: &str, answer: &str) -> Grade {
        let (kind, truth) = truth(id);
        kind.grade(answer, &truth)
    }

    /// Test 3: for each question kind, the truth is correct; a missing
    /// name, an extra name, a wrong count and a wrong outcome are not;
    /// Markdown, backquotes, qualified names and synonyms grade as meant;
    /// no line is incorrect; the last of two lines counts.
    #[test]
    fn answers_grade() {
        let scenario = Scenario::load().unwrap();
        for question in &scenario.questions {
            let graded = question.kind.grade(&question.truth, &question.truth);
            assert!(graded.correct, "{}: {graded:?}", question.id);
            assert!((graded.precision - 1.0).abs() < f64::EPSILON);
        }

        // Files: order, `./`, statuses spelled out, a subject without its PR.
        let ok = "Done.\n**ANSWER:** M ./crates/reef-testing/src/git.rs; added docs/fixture-notes.md; D `.editorconfig` | base: burst_coalesces at the shipped watcher pacing";
        assert!(
            grade("state-own", ok).correct,
            "{:?}",
            grade("state-own", ok)
        );
        let missing = "ANSWER: M crates/reef-testing/src/git.rs; A docs/fixture-notes.md | base: burst_coalesces at the shipped watcher pacing (#112)";
        let graded = grade("state-own", missing);
        assert!(!graded.correct);
        assert!((graded.precision - 1.0).abs() < f64::EPSILON);
        assert!((graded.recall - 0.75).abs() < f64::EPSILON);

        // Counts: a wrong count.
        assert!(grade("state-repo", "ANSWER: bot-5 59; bot-4 4").correct);
        assert!(!grade("state-repo", "ANSWER: bot-4 4; bot-5 58").correct);
        assert!(grade("state-repo", "ANSWER: bot-4: 4 files; bot-5: 59 files").correct);

        // Symbols: qualified names, `()`, generics, categories swapped.
        let qualified = "ANSWER: added json::rfc3339_millis(), tests::rfc3339_millis_edges; removed -; changed rfc3339, tests::workspace_json_has_stable_names_and_nulls, Timestamp#to_rfc3339(), timestamps_convert_at_the_edges";
        let graded = grade("changed-small", qualified);
        assert!(graded.correct, "{graded:?}");
        assert_eq!(graded.categories_right, Some(true));
        let swapped = "ANSWER: added rfc3339_millis; removed -; changed rfc3339, rfc3339_millis_edges, workspace_json_has_stable_names_and_nulls, Timestamp::to_rfc3339, timestamps_convert_at_the_edges";
        let graded = grade("changed-small", swapped);
        assert!(graded.correct);
        assert_eq!(graded.categories_right, Some(false));
        let extra = "ANSWER: added rfc3339_millis, rfc3339_millis_edges, now; removed -; changed rfc3339, workspace_json_has_stable_names_and_nulls, to_rfc3339, timestamps_convert_at_the_edges";
        let graded = grade("changed-small", extra);
        assert!(!graded.correct);
        assert!(graded.precision < 1.0 && (graded.recall - 1.0).abs() < f64::EPSILON);
        assert!(grade("changed-large", "ANSWER: added Current::holds<'a>(), SymbolIndex::tree_lookups, answer_lookups_by_reference, testing::by_lookup, Store::find_ordered, ordered_row, tests::ordered_lookups_follow_path_order; removed -; changed by_lookup, Current#keeps, opened, Store::open").correct);

        // Agents: synonyms, and a wrong outcome.
        assert!(
            grade(
                "who-file",
                "ANSWER: `bot-2` main; bot-4 in progress; bot-5 open"
            )
            .correct
        );
        assert!(!grade("who-file", "ANSWER: bot-2 open; bot-4 open; bot-5 open").correct);
        assert!(!grade("who-symbol", "ANSWER: bot-2 landed; bot-4 open; bot-5 open").correct);

        // Quotes, parentheses, and generics before the path.
        assert!(grade("state-own", "ANSWER: D .editorconfig; M crates/reef-testing/src/git.rs; A docs/fixture-notes.md | base: \"burst_coalesces at the shipped watcher pacing (#112)\"").correct);
        assert!(grade("state-repo", "ANSWER: bot-4 (4 files); bot-5 (59 Files)").correct);
        assert!(
            grade(
                "who-symbol",
                "ANSWER: bot-2 landed (on main); bot-5 open (not yet on main)"
            )
            .correct
        );
        assert_eq!(normalize_symbol("Current<'a>::holds()"), "holds");
        assert_eq!(normalize_symbol("Store<Vec<u8>>::open"), "open");

        // No line; two lines, the last counts.
        let none = grade("who-symbol", "bot-2 and bot-5");
        assert!(!none.correct);
        assert_eq!(none.answer, "no ANSWER: line");
        let two = "ANSWER: bot-2 landed\nOn reflection:\nANSWER: bot-2 landed; bot-5 open";
        assert!(grade("who-symbol", two).correct);
        let two = "ANSWER: bot-2 landed; bot-5 open\nANSWER: bot-2 landed";
        assert!(!grade("who-symbol", two).correct);
    }

    #[test]
    fn names_are_what_an_answer_must_mention() {
        let (kind, line) = truth("who-file");
        let names = kind.parse(&line).unwrap().names();
        assert_eq!(
            names.into_iter().collect::<Vec<_>>(),
            ["bot-2", "bot-4", "bot-5"]
        );
        let (kind, line) = truth("state-own");
        let names = kind.parse(&line).unwrap().names();
        assert!(names.contains("burst_coalesces at the shipped watcher pacing"));
        assert!(names.contains(".editorconfig"));
    }
}
