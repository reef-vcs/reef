//! The scripted part: every candidate of every question, run against the
//! fixture as an agent's shell or MCP client would run it, each step's call
//! and output counted. What a competent agent that already knows the best
//! commands would pay; the steelman for each arm, git's included.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::count::Counter;
use super::fixture::Fixture;
use super::mcp::McpClient;
use super::scenario::{Candidate, Question, Scenario, placeholders};
use super::{Arm, SCHEMA};

/// One step as run: a command line or an MCP call, and its output.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Step {
    /// The command line, or the tool and its arguments as compact JSON.
    pub call: String,
    /// What the agent would read: stdout then stderr, or the result's
    /// content text.
    pub output: String,
    /// The bytes of `structuredContent`, for an MCP call that sent it;
    /// reported apart, since clients put the content text in context.
    pub structured_bytes: Option<u64>,
    /// Whether the command failed or the call was an error.
    pub is_error: bool,
}

impl Step {
    /// The text that enters the agent's context: the call and its output.
    pub fn text(&self) -> String {
        format!("{}\n{}", self.call, self.output)
    }
}

/// One candidate, run and counted.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CandidateResult {
    /// The question.
    pub question: String,
    /// The arm.
    pub arm: Arm,
    /// The candidate's name.
    pub name: String,
    /// Whether it asks for text output (`-o text`).
    pub text_output: bool,
    /// The steps, placeholders filled and per-item steps expanded.
    pub steps: Vec<Step>,
    /// The whole text's count under each counter.
    pub counts: BTreeMap<String, u64>,
    /// Whether every name the truth mentions is in the output, and the
    /// scenario does not mark it insufficient.
    pub sufficient: bool,
    /// The truth's names the output lacks.
    pub missing: Vec<String>,
    /// The scenario's reason, if it marks the candidate insufficient.
    pub insufficient: Option<String>,
    /// For each JSON output, compact bytes per field (see [`breakdown`]).
    pub breakdown: Vec<Vec<FieldShare>>,
}

/// A JSON output's bytes attributed to one field.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FieldShare {
    /// `name` at the top level, `name.inner` one level down (`[]` for an
    /// array's elements), or `(structure)` for braces, commas and the like.
    pub field: String,
    /// Compact JSON bytes.
    pub bytes: u64,
}

/// What a session with the MCP server costs before any call.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct FixedCost {
    /// `tools/list`'s result as compact JSON, in bytes.
    pub tools_list_bytes: u64,
    /// `initialize`'s instructions, in bytes.
    pub instructions_bytes: u64,
    /// The tool definitions (name, description, input schema) and the
    /// instructions together, under each counter. For `anthropic`, the
    /// tools as the model sees them.
    pub counts: BTreeMap<String, u64>,
    /// How many tools.
    pub tools: usize,
}

/// One commit of the diff sweep.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SweepCommit {
    /// The commit.
    pub commit: String,
    /// Each view's count under each counter, by view name.
    pub views: BTreeMap<String, BTreeMap<String, u64>>,
}

/// The views the sweep records, in table order.
pub const SWEEP_VIEWS: [&str; 6] = [
    "git show",
    "git show --stat",
    "reef diff",
    "reef diff --stat",
    "reef diff --symbols",
    "reef diff --symbols -o text",
];

/// A scripted run, as written to `results/<date>-scripted.json`.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScriptedResult {
    /// The file format's version.
    pub schema: u32,
    /// UTC, `YYYY-MM-DD`.
    pub date: String,
    /// UTC, RFC 3339.
    pub started_at: String,
    /// `reef --version`.
    pub reef_version: String,
    /// The commit in it.
    pub reef_commit: Option<String>,
    /// `linux` or `macos`.
    pub os: String,
    /// [`Scenario::version`].
    pub fixture_version: String,
    /// The model the `anthropic` counter counted for.
    pub anthropic_model: String,
    /// Where `o200k` came from.
    pub o200k_source: String,
    /// Counters that did not run, with why.
    pub counters_missing: Vec<(String, String)>,
    /// The MCP session's fixed cost.
    pub fixed: FixedCost,
    /// Every candidate.
    pub candidates: Vec<CandidateResult>,
    /// The diff sweep; empty if skipped or failed.
    pub sweep: Vec<SweepCommit>,
    /// Why the sweep failed, if it did.
    #[serde(default)]
    pub sweep_error: Option<String>,
}

/// A candidate's steps with placeholders filled from `fixture`: a step with
/// a `*` placeholder repeats once per other open session, in agent order.
pub fn expand_steps(candidate: &Candidate, fixture: &Fixture) -> Result<Vec<String>, String> {
    let mut out = Vec::new();
    for step in &candidate.steps {
        out.extend(expand(step, fixture)?);
    }
    Ok(out)
}

/// `text` with placeholders filled, once per other open session if it has
/// a `*` one.
pub fn expand(text: &str, fixture: &Fixture) -> Result<Vec<String>, String> {
    let found = placeholders(text);
    let repeat = found.iter().any(|p| p.ends_with(":*"));
    let agents: Vec<Option<&str>> = if repeat {
        fixture.others().into_iter().map(Some).collect()
    } else {
        vec![None]
    };
    agents
        .into_iter()
        .map(|each| {
            let mut filled = text.to_owned();
            for placeholder in &found {
                let (kind, agent) = placeholder.split_once(':').unwrap_or_default();
                let agent = if agent == "*" {
                    each.unwrap_or_default()
                } else {
                    agent
                };
                let value = fixture
                    .value(kind, agent)
                    .ok_or_else(|| format!("no value for {{{placeholder}}}"))?;
                filled = filled.replace(&format!("{{{placeholder}}}"), &value);
            }
            Ok(filled)
        })
        .collect()
}

/// Whether `output` names everything in `names`, case-insensitively; the
/// names it lacks.
pub fn missing_names(output: &str, names: &[String]) -> Vec<String> {
    let output = output.to_lowercase();
    names
        .iter()
        .filter(|name| !output.contains(&name.to_lowercase()))
        .cloned()
        .collect()
}

/// A JSON value's compact bytes per field: each top-level field (key,
/// colon and value), one level down for objects and for arrays of objects
/// (summed over elements, as `name.[].inner`), and `(structure)` for the
/// rest, so the shares sum to the compact whole. A top-level array is
/// attributed by its elements' fields, as `[].name`.
pub fn breakdown(value: &Value) -> Vec<FieldShare> {
    let whole = compact_len(value);
    let mut shares: BTreeMap<String, u64> = BTreeMap::new();
    let mut attributed = 0;
    match value {
        Value::Object(map) => {
            for (key, inner) in map {
                let len = field_len(key, inner);
                attributed += len;
                let inner_shares = children(inner);
                if inner_shares.is_empty() {
                    shares.insert(key.clone(), len);
                } else {
                    let mut rest = len;
                    for (name, bytes) in inner_shares {
                        rest -= bytes;
                        *shares.entry(format!("{key}.{name}")).or_default() += bytes;
                    }
                    *shares.entry(format!("{key} (structure)")).or_default() += rest;
                }
            }
        }
        Value::Array(_) => {
            for (name, bytes) in children(value) {
                attributed += bytes;
                *shares.entry(name).or_default() += bytes;
            }
        }
        _ => {}
    }
    let mut out: Vec<FieldShare> = shares
        .into_iter()
        .map(|(field, bytes)| FieldShare { field, bytes })
        .collect();
    out.push(FieldShare {
        field: "(structure)".to_owned(),
        bytes: whole - attributed,
    });
    out.sort_by(|a, b| b.bytes.cmp(&a.bytes).then(a.field.cmp(&b.field)));
    out
}

/// One level down: an object's fields, or an array's elements' fields as
/// `[].name`; empty for anything else.
fn children(value: &Value) -> Vec<(String, u64)> {
    match value {
        Value::Object(map) => map
            .iter()
            .map(|(key, inner)| (key.clone(), field_len(key, inner)))
            .collect(),
        Value::Array(items) if items.iter().all(Value::is_object) && !items.is_empty() => {
            let mut sums: BTreeMap<String, u64> = BTreeMap::new();
            for item in items {
                if let Value::Object(map) = item {
                    for (key, inner) in map {
                        *sums.entry(format!("[].{key}")).or_default() += field_len(key, inner);
                    }
                }
            }
            sums.into_iter().collect()
        }
        _ => Vec::new(),
    }
}

/// `"key":value` compact.
fn field_len(key: &str, value: &Value) -> u64 {
    compact_len(&Value::String(key.to_owned())) + 1 + compact_len(value)
}

fn compact_len(value: &Value) -> u64 {
    value.to_string().len() as u64
}

/// Runs a shell step as an agent's shell tool would: `sh -c`, stdout then
/// stderr.
fn run_shell(step: &str, cwd: &Path, env: &[(String, std::ffi::OsString)]) -> Result<Step, String> {
    let out = Command::new("sh")
        .args(["-c", step])
        .current_dir(cwd)
        .env_clear()
        .envs(env.iter().cloned())
        .stdin(Stdio::null())
        .output()
        .map_err(|e| format!("sh -c {step}: {e}"))?;
    let mut output = String::from_utf8_lossy(&out.stdout).into_owned();
    output.push_str(&String::from_utf8_lossy(&out.stderr));
    Ok(Step {
        call: step.to_owned(),
        output,
        structured_bytes: None,
        is_error: !out.status.success(),
    })
}

/// Everything a scripted run needs.
pub struct Scripted<'a> {
    /// The scenario.
    pub scenario: &'a Scenario,
    /// The built fixture.
    pub fixture: &'a Fixture,
    /// The counters to run.
    pub counters: &'a mut [Box<dyn Counter>],
    /// Where every step's output is saved.
    pub out_dir: &'a Path,
    /// Counters that failed, with why: each is skipped from then on, and
    /// [`ScriptedResult::drop_counter`] takes its partial counts out.
    pub failed: Vec<(String, String)>,
}

impl Scripted<'_> {
    /// `count` under every counter that has not failed; a counter that
    /// fails now is recorded and skipped from then on, so one endpoint
    /// error costs that counter, not the run.
    fn count_all(
        &mut self,
        mut count: impl FnMut(&mut dyn Counter) -> Result<u64, String>,
    ) -> BTreeMap<String, u64> {
        let mut counts = BTreeMap::new();
        for counter in self.counters.iter_mut() {
            let name = counter.name();
            if self.failed.iter().any(|(failed, _)| failed == name) {
                continue;
            }
            match count(counter.as_mut()) {
                Ok(n) => {
                    counts.insert(name.to_owned(), n);
                }
                Err(error) => self.failed.push((name.to_owned(), error)),
            }
        }
        counts
    }

    /// Runs every candidate of `questions` (all of them if empty).
    pub fn candidates(&mut self, questions: &[String]) -> Result<Vec<CandidateResult>, String> {
        let home = self.fixture.root.join("scripted-home");
        std::fs::create_dir_all(&home).map_err(|e| e.to_string())?;
        let mut results = Vec::new();
        for question in &self.scenario.questions {
            if !questions.is_empty() && !questions.contains(&question.id) {
                continue;
            }
            for candidate in &question.candidates {
                results.push(self.candidate(question, candidate, &home)?);
            }
        }
        Ok(results)
    }

    fn candidate(
        &mut self,
        question: &Question,
        candidate: &Candidate,
        home: &Path,
    ) -> Result<CandidateResult, String> {
        let arm = candidate.arm;
        let cwd = self.fixture.cwd(arm);
        let env = self.fixture.env(arm, home)?;
        let steps = if arm == Arm::Mcp {
            let mut client = McpClient::start(&self.fixture.bin.join("reef"), &["mcp"], cwd, &env)?;
            let mut steps = Vec::new();
            for call in &candidate.calls {
                let args = Value::Object(call.args.clone()).to_string();
                for filled in expand(&args, self.fixture)? {
                    let arguments: Value =
                        serde_json::from_str(&filled).map_err(|e| e.to_string())?;
                    let result = client.call(&call.tool, &arguments)?;
                    steps.push(Step {
                        call: format!("{} {filled}", call.tool),
                        output: result.text,
                        structured_bytes: result.structured.map(|s| s.len() as u64),
                        is_error: result.is_error,
                    });
                }
            }
            steps
        } else {
            expand_steps(candidate, self.fixture)?
                .iter()
                .map(|step| run_shell(step, cwd, &env))
                .collect::<Result<_, _>>()?
        };
        let text: String = steps.iter().map(Step::text).collect::<Vec<_>>().join("\n");
        let counts = self.count_all(|counter| counter.count(&text));
        let truth = question.kind.parse(&question.truth)?;
        let names: Vec<String> = truth.names().into_iter().collect();
        let outputs: String = steps.iter().map(|s| s.output.as_str()).collect();
        let missing = missing_names(&outputs, &names);
        let breakdown = steps
            .iter()
            .filter_map(|step| serde_json::from_str::<Value>(&step.output).ok())
            .filter(|value| value.is_object() || value.is_array())
            .map(|value| breakdown(&value))
            .collect();
        let saved = self.out_dir.join("scripted").join(format!(
            "{}-{}-{}.txt",
            question.id,
            arm,
            candidate.name.replace([' ', ','], "-")
        ));
        if let Some(dir) = saved.parent() {
            std::fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        std::fs::write(&saved, crate::agent::scrub::scrub(&text))
            .map_err(|e| format!("{}: {e}", saved.display()))?;
        Ok(CandidateResult {
            question: question.id.clone(),
            arm,
            name: candidate.name.clone(),
            text_output: candidate.is_text(),
            steps,
            counts,
            sufficient: missing.is_empty() && candidate.insufficient.is_none(),
            missing,
            insufficient: candidate.insufficient.clone(),
            breakdown,
        })
    }

    /// The MCP server's fixed cost: `tools/list` and the instructions,
    /// under each counter.
    pub fn fixed_cost(&mut self) -> Result<FixedCost, String> {
        let home = self.fixture.root.join("scripted-home");
        let env = self.fixture.env(Arm::Mcp, &home)?;
        let mut client = McpClient::start(
            &self.fixture.bin.join("reef"),
            &["mcp"],
            self.fixture.cwd(Arm::Mcp),
            &env,
        )?;
        let (listed, tools) = client.list_tools()?;
        // What a model is given: name, description and input schema.
        let definitions: Vec<Value> = tools
            .iter()
            .map(|tool| {
                serde_json::json!({
                    "name": tool["name"],
                    "description": tool["description"],
                    "input_schema": tool["inputSchema"],
                })
            })
            .collect();
        let instructions = client.instructions.clone();
        let counts = self.count_all(|counter| {
            Ok(counter.count_tools(&definitions)? + counter.count(&instructions)?)
        });
        Ok(FixedCost {
            tools_list_bytes: listed.to_string().len() as u64,
            instructions_bytes: client.instructions.len() as u64,
            counts,
            tools: tools.len(),
        })
    }

    /// The diff sweep: `commits` commits of the source's first-parent
    /// history up to the base, each diffed by git and by Reef's views, in a
    /// clone registered with the fixture's daemon.
    pub fn sweep(&mut self, source: &Path, commits: usize) -> Result<Vec<SweepCommit>, String> {
        let fixture = self.fixture;
        let clone = fixture.root.join("sweep");
        fixture.git(
            &fixture.root,
            &[
                "clone",
                "--quiet",
                "--no-checkout",
                &source.to_string_lossy(),
                &clone.to_string_lossy(),
            ],
        )?;
        let base = &self.scenario.fixture.base;
        fixture.git(&clone, &["checkout", "--quiet", "-B", "main", base])?;
        fixture.git(&clone, &["remote", "remove", "origin"])?;
        let registered: Value = serde_json::from_str(&fixture.reef(&clone, &["repo", "register"])?)
            .map_err(|e| e.to_string())?;
        let repo = registered["id"]
            .as_str()
            .ok_or("no repository id")?
            .to_owned();
        let list = fixture.git(
            &clone,
            &[
                "rev-list",
                "--first-parent",
                &format!("--max-count={commits}"),
                base,
            ],
        )?;
        let home = fixture.root.join("scripted-home");
        let env = fixture.env(Arm::Reef, &home)?;
        let mut out = Vec::new();
        for commit in list.lines() {
            let mut views = BTreeMap::new();
            for view in SWEEP_VIEWS {
                let command = match view {
                    "git show" => format!("git show --format= {commit}"),
                    "git show --stat" => format!("git show --stat --format= {commit}"),
                    "reef diff" => format!("reef diff --repo {repo} -r {commit}"),
                    "reef diff --stat" => format!("reef diff --repo {repo} -r {commit} --stat"),
                    "reef diff --symbols" => {
                        format!("reef diff --repo {repo} -r {commit} --symbols")
                    }
                    _ => format!("reef diff --repo {repo} -r {commit} --symbols -o text"),
                };
                let step = run_shell(&command, &clone, &env)?;
                if step.is_error {
                    return Err(format!("`{command}` failed: {}", step.output.trim()));
                }
                let counts = self.count_all(|counter| counter.count(&step.output));
                views.insert(view.to_owned(), counts);
            }
            out.push(SweepCommit {
                commit: commit.to_owned(),
                views,
            });
        }
        let _ = fixture.reef(&clone, &["repo", "forget", &repo]);
        Ok(out)
    }
}

/// The cheapest sufficient candidate for (question, arm) by `counter`,
/// among those `filter` keeps.
pub fn cheapest<'a>(
    candidates: &'a [CandidateResult],
    question: &str,
    arm: Arm,
    counter: &str,
    filter: impl Fn(&CandidateResult) -> bool,
) -> Option<&'a CandidateResult> {
    candidates
        .iter()
        .filter(|c| c.question == question && c.arm == arm && c.sufficient && filter(c))
        .filter(|c| c.counts.contains_key(counter))
        .min_by_key(|c| c.counts[counter])
}

/// Where scripted results go, by date.
pub fn result_path(out_dir: &Path, date: &str) -> PathBuf {
    out_dir.join(format!("{date}-scripted.json"))
}

impl ScriptedResult {
    /// An empty result with the run's pins.
    pub fn new(
        reef_version: &str,
        anthropic_model: &str,
        counters_missing: Vec<(String, String)>,
    ) -> Self {
        let (date, started_at) = crate::agent::process::today_and_now();
        ScriptedResult {
            schema: SCHEMA,
            date,
            started_at,
            reef_version: reef_version.to_owned(),
            reef_commit: crate::agent::process::reef_commit(reef_version),
            os: std::env::consts::OS.to_owned(),
            fixture_version: Scenario::version(),
            anthropic_model: anthropic_model.to_owned(),
            o200k_source: super::count::O200K_SOURCE.to_owned(),
            counters_missing,
            fixed: FixedCost::default(),
            candidates: Vec::new(),
            sweep: Vec::new(),
            sweep_error: None,
        }
    }

    /// Takes a counter's counts out of every table, and records why: for a
    /// counter that failed partway, whose counts would compare unlike runs.
    pub fn drop_counter(&mut self, name: &str, why: &str) {
        for candidate in &mut self.candidates {
            candidate.counts.remove(name);
        }
        self.fixed.counts.remove(name);
        for commit in &mut self.sweep {
            for counts in commit.views.values_mut() {
                counts.remove(name);
            }
        }
        self.counters_missing
            .push((name.to_owned(), why.to_owned()));
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn candidate(name: &str, bytes: u64, sufficient: bool) -> CandidateResult {
        CandidateResult {
            question: "q".to_owned(),
            arm: Arm::Git,
            name: name.to_owned(),
            text_output: false,
            steps: Vec::new(),
            counts: BTreeMap::from([("bytes".to_owned(), bytes)]),
            sufficient,
            missing: Vec::new(),
            insufficient: None,
            breakdown: Vec::new(),
        }
    }

    /// Test 4: a candidate whose output lacks a truth name is insufficient;
    /// the cheapest sufficient candidate wins over a cheaper insufficient
    /// one; the scenario's `insufficient` overrides the check.
    #[test]
    fn sufficiency() {
        let names = vec!["bot-4".to_owned(), "Bot-5".to_owned()];
        assert_eq!(
            missing_names("BOT-4 and bot-5", &names),
            Vec::<String>::new()
        );
        assert_eq!(missing_names("bot-4 only", &names), ["Bot-5"]);

        let candidates = [
            candidate("cheap but short", 10, false),
            candidate("right", 50, true),
            candidate("dear", 90, true),
        ];
        let best = cheapest(&candidates, "q", Arm::Git, "bytes", |_| true).unwrap();
        assert_eq!(best.name, "right");
        assert!(cheapest(&candidates, "q", Arm::Reef, "bytes", |_| true).is_none());

        let scenario = Scenario::load().unwrap();
        let marked = scenario
            .questions
            .iter()
            .flat_map(|q| &q.candidates)
            .find(|c| c.insufficient.is_some())
            .unwrap();
        assert_eq!(marked.name, "repository status");
    }

    fn fixture() -> Fixture {
        let agents = ["bot-0", "bot-4", "bot-5"];
        Fixture {
            root: PathBuf::from("/f"),
            home: PathBuf::from("/f/h"),
            state: PathBuf::from("/f/s"),
            bin: PathBuf::from("/f/bin"),
            reef_bin_dir: PathBuf::from("/build"),
            git_root: PathBuf::from("/f/git"),
            worktrees: agents
                .iter()
                .map(|a| ((*a).to_owned(), PathBuf::from(format!("/f/wt/{a}"))))
                .collect(),
            reef_root: PathBuf::from("/f/reef"),
            repo_id: "r".to_owned(),
            workspaces: agents
                .iter()
                .map(|a| {
                    (
                        (*a).to_owned(),
                        crate::tokens::fixture::Workspace {
                            id: format!("id-{a}"),
                            path: PathBuf::from(format!("/f/ws/{a}")),
                            change: format!("change-{a}"),
                        },
                    )
                })
                .collect(),
            trial_agent: "bot-0".to_owned(),
        }
    }

    /// Test 5: placeholders fill from the fixture; a `*` step repeats once
    /// per other open session, in agent order, never the trial agent's.
    #[test]
    fn expansion() {
        let fixture = fixture();
        let candidate = Candidate {
            arm: Arm::Reef,
            name: "c".to_owned(),
            steps: vec![
                "reef ws list".to_owned(),
                "reef status --workspace {ws:*} # {change:*}".to_owned(),
                "git -C {wt:bot-5} diff".to_owned(),
            ],
            calls: Vec::new(),
            insufficient: None,
        };
        assert_eq!(
            expand_steps(&candidate, &fixture).unwrap(),
            [
                "reef ws list",
                "reef status --workspace id-bot-4 # change-bot-4",
                "reef status --workspace id-bot-5 # change-bot-5",
                "git -C /f/wt/bot-5 diff",
            ]
        );
        assert_eq!(
            expand(r#"{"workspace":"{ws:*}"}"#, &fixture).unwrap(),
            [r#"{"workspace":"id-bot-4"}"#, r#"{"workspace":"id-bot-5"}"#]
        );
        assert!(expand("{ws:bot-9}", &fixture).is_err());
    }

    /// Test 7: bytes are attributed to fields and one level down, and the
    /// shares sum to the compact whole.
    #[test]
    fn field_breakdown() {
        let value = json!({
            "workspace": {"id": "abc", "changes": [1, 2, 3]},
            "heads": [{"id": "x", "desc": "long description"}, {"id": "y", "desc": "z"}],
            "n": 7,
        });
        let shares = breakdown(&value);
        let total: u64 = shares.iter().map(|s| s.bytes).sum();
        assert_eq!(total, value.to_string().len() as u64);
        let get = |field: &str| shares.iter().find(|s| s.field == field).map(|s| s.bytes);
        assert_eq!(get("n"), Some(5)); // "n":7
        assert_eq!(get("workspace.id"), Some(10)); // "id":"abc"
        assert_eq!(get("heads.[].desc"), Some(25 + 10));
        assert!(get("heads (structure)").is_some());

        let array = json!([{"id": "a"}, {"id": "b", "x": 1}]);
        let shares = breakdown(&array);
        let total: u64 = shares.iter().map(|s| s.bytes).sum();
        assert_eq!(total, array.to_string().len() as u64);
        assert_eq!(
            shares.iter().find(|s| s.field == "[].id").map(|s| s.bytes),
            Some(16)
        );
    }
}
