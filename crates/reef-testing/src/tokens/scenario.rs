//! `tokens/scenario.toml`: the fixture, the questions and their truth, the
//! scripted candidates; and the prompts in `tokens/prompts/`.

use std::collections::BTreeMap;

use serde::Deserialize;
use sha2::{Digest as _, Sha256};

use super::Arm;
use super::grade::Kind;

/// The scenario file, compiled in.
pub const SCENARIO_TOML: &str = include_str!("../../tokens/scenario.toml");
/// The trial agent's own work, compiled in.
pub const SELF_PATCH: &str = include_str!("../../tokens/patches/self.patch");
/// What every prompt starts with.
pub const PREAMBLE: &str = include_str!("../../tokens/prompts/preamble.txt");

/// Each question's prompt text, by id.
const QUESTIONS: [(&str, &str); 6] = [
    (
        "state-own",
        include_str!("../../tokens/prompts/state-own.txt"),
    ),
    (
        "state-repo",
        include_str!("../../tokens/prompts/state-repo.txt"),
    ),
    (
        "changed-small",
        include_str!("../../tokens/prompts/changed-small.txt"),
    ),
    (
        "changed-large",
        include_str!("../../tokens/prompts/changed-large.txt"),
    ),
    (
        "who-file",
        include_str!("../../tokens/prompts/who-file.txt"),
    ),
    (
        "who-symbol",
        include_str!("../../tokens/prompts/who-symbol.txt"),
    ),
];

/// The whole scenario.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Scenario {
    /// How the fixture is built.
    pub fixture: FixtureSpec,
    /// Each arm's tool sentence.
    pub arms: BTreeMap<Arm, String>,
    /// The diff sweep.
    pub sweep: Sweep,
    /// The questions, in order.
    #[serde(rename = "question")]
    pub questions: Vec<Question>,
}

/// How the fixture is built.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct FixtureSpec {
    /// The commit whose tree is the initial import.
    pub base: String,
    /// Paths removed from the import and from every session's diff.
    pub strip: Vec<String>,
    /// The agent a trial runs as.
    pub trial_agent: String,
    /// Sessions landed on `main`, in order.
    pub landed: Vec<Session>,
    /// Work in progress.
    pub open: Vec<Session>,
}

/// One agent's session: a commit of the source repository, whose diff
/// against its parent is the work, or a patch file.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Session {
    /// The agent.
    pub agent: String,
    /// The commit whose diff is the work.
    pub commit: Option<String>,
    /// A patch under `tokens/`, for the trial agent's own work.
    pub patch: Option<String>,
}

/// The diff sweep's settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Sweep {
    /// Commits of `main`, up to and including the base.
    pub commits: usize,
}

/// One question.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Question {
    /// Its id, which names its prompt file.
    pub id: String,
    /// How its answers are graded.
    pub kind: Kind,
    /// The correct answer line.
    pub truth: String,
    /// The scripted candidates.
    #[serde(rename = "candidate")]
    pub candidates: Vec<Candidate>,
}

/// A scripted way to answer: shell steps, or MCP calls.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Candidate {
    /// The arm.
    pub arm: Arm,
    /// A few words, unique within the question and arm.
    pub name: String,
    /// Shell command lines, for `git` and `reef`.
    #[serde(default)]
    pub steps: Vec<String>,
    /// MCP calls, for `mcp`.
    #[serde(default)]
    pub calls: Vec<McpCall>,
    /// Why this candidate cannot answer although its output names
    /// everything the truth does, if it cannot.
    #[serde(default)]
    pub insufficient: Option<String>,
}

impl Candidate {
    /// Whether a step asks for text output: such a candidate measures
    /// `reef (text)`, not what an agent gets unasked.
    pub fn is_text(&self) -> bool {
        !self.steps.is_empty() && self.steps.iter().any(|step| step.contains("-o text"))
    }
}

/// One MCP tool call.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct McpCall {
    /// The tool.
    pub tool: String,
    /// Its arguments; string values may hold placeholders.
    #[serde(default)]
    pub args: serde_json::Map<String, serde_json::Value>,
}

impl Scenario {
    /// The compiled-in scenario, checked.
    pub fn load() -> Result<Scenario, String> {
        Scenario::parse(SCENARIO_TOML)
    }

    /// Parses and checks a scenario file.
    pub fn parse(text: &str) -> Result<Scenario, String> {
        let scenario: Scenario =
            toml::from_str(text).map_err(|e| format!("tokens/scenario.toml: {e}"))?;
        scenario.validate()?;
        Ok(scenario)
    }

    /// The fixture's version, first 12 hex digits of a digest over what
    /// live trials depend on: the fixture's spec (its pinned commits
    /// included), the tool sentences, each question's id, kind and truth,
    /// the prompts, and the self patch. Not the scripted candidates: adding
    /// one changes no trial.
    pub fn version() -> String {
        let mut hasher = Sha256::new();
        match Scenario::parse(SCENARIO_TOML) {
            Ok(scenario) => {
                hasher.update(format!("{:?}{:?}", scenario.fixture, scenario.arms));
                for q in &scenario.questions {
                    hasher.update(format!("{}\n{:?}\n{}\n", q.id, q.kind, q.truth));
                }
            }
            Err(_) => hasher.update(SCENARIO_TOML),
        }
        hasher.update(SELF_PATCH);
        hasher.update(PREAMBLE);
        for (_, text) in QUESTIONS {
            hasher.update(text);
        }
        super::count::hex(&hasher.finalize()[..6])
    }

    /// The question `id`.
    pub fn question(&self, id: &str) -> Option<&Question> {
        self.questions.iter().find(|q| q.id == id)
    }

    /// The prompt for `question` in `arm`.
    pub fn prompt(&self, question: &Question, arm: Arm) -> String {
        let text = QUESTIONS
            .iter()
            .find(|(id, _)| *id == question.id)
            .map_or("", |(_, text)| text);
        format!(
            "{}\n{}\nDo not change any files.\n\n{}",
            PREAMBLE.trim_end(),
            self.arms.get(&arm).map_or("", String::as_str),
            text.trim_end()
        )
    }

    /// Every check the scenario must pass before anything runs.
    fn validate(&self) -> Result<(), String> {
        for arm in Arm::ALL {
            if !self.arms.contains_key(&arm) {
                return Err(format!("no tool sentence for the {arm} arm"));
            }
        }
        let sessions = self.fixture.landed.iter().chain(&self.fixture.open);
        for session in sessions {
            match (&session.commit, &session.patch) {
                (Some(_), None) | (None, Some(_)) => {}
                _ => {
                    return Err(format!(
                        "{}'s session needs a commit or a patch, not both",
                        session.agent
                    ));
                }
            }
        }
        if !self
            .fixture
            .open
            .iter()
            .any(|s| s.agent == self.fixture.trial_agent)
        {
            return Err("the trial agent has no open session".to_owned());
        }
        let agents: Vec<&str> = self.fixture.open.iter().map(|s| s.agent.as_str()).collect();
        for question in &self.questions {
            if !QUESTIONS.iter().any(|(id, _)| *id == question.id) {
                return Err(format!("question {} has no prompt file", question.id));
            }
            question
                .kind
                .parse(&question.truth)
                .map_err(|e| format!("question {}'s truth: {e}", question.id))?;
            for arm in Arm::ALL {
                if !question.candidates.iter().any(|c| c.arm == arm) {
                    return Err(format!(
                        "question {} has no candidate for the {arm} arm",
                        question.id
                    ));
                }
            }
            for candidate in &question.candidates {
                let label = format!("{} / {} / {}", question.id, candidate.arm, candidate.name);
                match (
                    candidate.arm,
                    candidate.steps.is_empty(),
                    candidate.calls.is_empty(),
                ) {
                    (Arm::Mcp, true, false) | (Arm::Git | Arm::Reef, false, true) => {}
                    _ => {
                        return Err(format!(
                            "{label}: `mcp` candidates have calls, the others steps"
                        ));
                    }
                }
                for step in &candidate.steps {
                    if step.contains('|') {
                        return Err(format!("{label}: `{step}` pipes its output"));
                    }
                    for placeholder in placeholders(step) {
                        check_placeholder(&placeholder, &agents)
                            .map_err(|e| format!("{label}: {e}"))?;
                    }
                }
                for call in &candidate.calls {
                    for placeholder in call.args.values().flat_map(strings).flat_map(placeholders) {
                        check_placeholder(&placeholder, &agents)
                            .map_err(|e| format!("{label}: {e}"))?;
                    }
                }
            }
        }
        Ok(())
    }
}

/// The string values in a JSON value, recursively.
fn strings(value: &serde_json::Value) -> Vec<&str> {
    match value {
        serde_json::Value::String(s) => vec![s.as_str()],
        serde_json::Value::Array(items) => items.iter().flat_map(strings).collect(),
        serde_json::Value::Object(map) => map.values().flat_map(strings).collect(),
        _ => Vec::new(),
    }
}

/// The `{kind:agent}` placeholders in `text`: a word, a colon, and a name
/// or `*`, in braces with no brace inside, so JSON around one is not taken
/// for one.
pub fn placeholders(text: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find('{') {
        let after = &rest[start + 1..];
        let Some(end) = after.find(['{', '}']) else {
            break;
        };
        if after.as_bytes()[end] == b'}' {
            let inner = &after[..end];
            let well_formed = inner.split_once(':').is_some_and(|(kind, name)| {
                !kind.is_empty()
                    && kind.chars().all(|c| c.is_ascii_lowercase())
                    && (name == "*"
                        || (!name.is_empty()
                            && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')))
            });
            if well_formed {
                out.push(inner.to_owned());
            }
        }
        rest = &after[end..];
    }
    out
}

fn check_placeholder(placeholder: &str, agents: &[&str]) -> Result<(), String> {
    let (kind, agent) = placeholder
        .split_once(':')
        .ok_or_else(|| format!("bad placeholder {{{placeholder}}}"))?;
    if !["wt", "ws", "change"].contains(&kind) {
        return Err(format!("unknown placeholder {{{placeholder}}}"));
    }
    if agent != "*" && !agents.contains(&agent) {
        return Err(format!(
            "placeholder {{{placeholder}}} names no open session"
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 10: the scenario parses and holds together; no prompt names a
    /// harness, vendor or model; the tool sentences differ only in their
    /// tool; the strip list covers every harness's configuration.
    #[test]
    fn scenario_valid() {
        const VENDOR_WORDS: [&str; 8] = [
            "claude",
            "codex",
            "cursor",
            "opencode",
            "gemini",
            "anthropic",
            "openai",
            "gpt",
        ];
        let scenario = Scenario::load().unwrap();
        assert_eq!(scenario.questions.len(), 6);
        for (id, _) in QUESTIONS {
            assert!(scenario.question(id).is_some(), "{id}");
        }

        for question in &scenario.questions {
            for arm in Arm::ALL {
                let prompt = scenario.prompt(question, arm).to_lowercase();
                for word in VENDOR_WORDS {
                    assert!(
                        !prompt.contains(word),
                        "{} / {arm} names `{word}`",
                        question.id
                    );
                }
                assert!(prompt.contains("answer:"), "{}", question.id);
            }
        }

        // "Answer with git." / "Answer with the `reef` command." / "Answer
        // with the reef tools.": the same frame around the tool.
        let sentences: Vec<&String> = scenario.arms.values().collect();
        for sentence in &sentences {
            assert!(sentence.starts_with("Answer with "), "{sentence}");
            assert!(sentence.ends_with('.'), "{sentence}");
            assert_eq!(sentence.lines().count(), 1, "{sentence}");
        }

        for path in [
            "AGENTS.md",
            "CLAUDE.md",
            ".claude",
            ".cursor",
            ".codex",
            ".mcp.json",
        ] {
            assert!(
                scenario.fixture.strip.iter().any(|p| p == path),
                "{path} is not stripped"
            );
        }
        assert_eq!(Scenario::version().len(), 12);
    }

    /// #114 test 21: `state-repo` has the one-call candidates, and only
    /// `-o text` makes one a text candidate.
    #[test]
    fn state_repo_has_one_call_candidates() {
        let scenario = Scenario::load().unwrap();
        let question = scenario.question("state-repo").unwrap();
        let named = |arm: Arm, name: &str| {
            question
                .candidates
                .iter()
                .find(|c| c.arm == arm && c.name == name)
                .unwrap_or_else(|| panic!("no {arm} {name}"))
        };
        assert_eq!(named(Arm::Reef, "list").steps, ["reef ws list"]);
        assert!(!named(Arm::Reef, "list").is_text());
        assert!(!named(Arm::Reef, "list full").is_text());
        assert!(named(Arm::Reef, "list text").is_text());
        assert_eq!(named(Arm::Mcp, "list").calls.len(), 1);
    }

    /// Test 5, the scenario's half: an unknown placeholder fails validation.
    #[test]
    fn unknown_placeholders_fail() {
        let bad = SCENARIO_TOML.replacen("git -C {wt:*} status", "git -C {wt:bot-9} status", 1);
        let error = Scenario::parse(&bad).unwrap_err();
        assert!(error.contains("names no open session"), "{error}");
        let bad = SCENARIO_TOML.replacen("--workspace {ws:*}", "--workspace {dir:*}", 1);
        assert!(
            Scenario::parse(&bad)
                .unwrap_err()
                .contains("unknown placeholder")
        );
        assert_eq!(placeholders("git -C {wt:*} diff '{x}'"), ["wt:*"]);
        assert_eq!(
            placeholders(r#"{"workspace":"{ws:bot-4}","paths":["a"]}"#),
            ["ws:bot-4"]
        );
        assert_eq!(placeholders("{a: b} {ws:} {:x}"), Vec::<String>::new());
    }
}
