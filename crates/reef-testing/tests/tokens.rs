//! The token benchmark's fixture and truth, end to end (R2.17, test 11).
//! Ignored by default: it needs the checkout's full history, `git`, and the
//! built `reef` and `reef-daemon` (`cargo build -p reef-vcs -p
//! reef-daemon`), and takes about a minute. `just tokens-check` and the
//! `tokens.yml` workflow run it.
//!
//! `REEF_TOKENS_SOURCE` names the git checkout holding the scenario's
//! commits (default: the workspace root) and `REEF_BIN_DIR` the builds
//! (default: the target directory's `debug`).

#![allow(
    clippy::unwrap_used,
    clippy::too_many_lines,
    clippy::print_stderr,
    reason = "an end-to-end test: its helpers panic as its assertions do, and it prints a skip notice"
)]

use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use reef_testing::tokens::Arm;
use reef_testing::tokens::count::{Bytes, Counter};
use reef_testing::tokens::fixture::{Fixture, FixtureConfig, missing_commits};
use reef_testing::tokens::grade::Answer;
use reef_testing::tokens::scenario::Scenario;
use reef_testing::tokens::scripted::{Scripted, cheapest};

fn workspace_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..")
}

fn config() -> FixtureConfig {
    let source = std::env::var_os("REEF_TOKENS_SOURCE").map_or_else(workspace_root, PathBuf::from);
    let bin = std::env::var_os("REEF_BIN_DIR").map_or_else(
        || {
            std::env::var_os("CARGO_TARGET_DIR")
                .map_or_else(|| workspace_root().join("target"), PathBuf::from)
                .join("debug")
        },
        PathBuf::from,
    );
    FixtureConfig {
        source: std::path::absolute(source).unwrap(),
        reef_bin_dir: std::path::absolute(bin).unwrap(),
        work_root: std::env::temp_dir().join("reef-tokens-test"),
        settle_timeout: Duration::from_secs(300),
    }
}

/// Both copies build and settle and hold the same work; the truth matches
/// git on the git copy and Reef on the Reef copy; every arm of every
/// question has a sufficient candidate; nothing changes the fixture.
#[test]
#[ignore = "needs full history and built binaries; run by `just tokens-check`"]
fn fixture_and_truth() {
    let scenario = Scenario::load().unwrap();
    let config = config();
    let missing = missing_commits(&config.source, &scenario);
    if !missing.is_empty() {
        eprintln!(
            "skipped: {} lacks the pinned commits {}",
            config.source.display(),
            missing.join(", ")
        );
        return;
    }
    let fixture = Fixture::build(&config, &scenario).unwrap();
    let checked =
        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| check(&scenario, &fixture)));
    fixture.teardown(false);
    if let Err(panic) = checked {
        std::panic::resume_unwind(panic);
    }
}

/// A porcelain status line's status and path. Read by word: the fixture's
/// commands trim their output, so the first line may have lost its leading
/// space.
fn porcelain(line: &str) -> (String, String) {
    let (status, path) = line.trim_start().split_once(' ').unwrap();
    (status.to_owned(), path.trim().to_owned())
}

fn check(scenario: &Scenario, fixture: &Fixture) {
    let truth = |id: &str| {
        let question = scenario.question(id).unwrap();
        question.kind.parse(&question.truth).unwrap()
    };

    // No stripped file in either copy.
    for root in [&fixture.git_root, &fixture.reef_root] {
        for path in &scenario.fixture.strip {
            assert!(!root.join(path).exists(), "{} holds {path}", root.display());
        }
    }

    // The same changed paths per agent in both copies, and the git copy's
    // counts are the truth's.
    let Answer::Counts(counts) = truth("state-repo") else {
        panic!()
    };
    for (agent, worktree) in &fixture.worktrees {
        let status = fixture
            .git(worktree, &["status", "--porcelain=v1", "-uall"])
            .unwrap();
        let git_paths: BTreeSet<String> = status.lines().map(|line| porcelain(line).1).collect();
        let workspace = &fixture.workspaces[agent];
        let summary: serde_json::Value = serde_json::from_str(
            &fixture
                .reef(
                    &fixture.reef_root,
                    &["diff", "-r", &workspace.change, "--summary"],
                )
                .unwrap(),
        )
        .unwrap();
        let reef_paths: BTreeSet<String> = summary["files"]
            .as_array()
            .unwrap()
            .iter()
            .map(|f| f["path"].as_str().unwrap().to_owned())
            .collect();
        assert_eq!(
            git_paths, reef_paths,
            "{agent}'s work differs between the copies"
        );
        if let Some((_, n)) = counts.iter().find(|(a, _)| a == agent) {
            assert_eq!(git_paths.len() as u64, *n, "{agent}'s count");
        }
    }

    // `state-own` from git: statuses and the base subject.
    let Answer::Files { files, base } = truth("state-own") else {
        panic!()
    };
    let own = &fixture.worktrees[&fixture.trial_agent];
    let status = fixture
        .git(own, &["status", "--porcelain=v1", "-uall"])
        .unwrap();
    let from_git: BTreeSet<(char, String)> = status
        .lines()
        .map(|line| {
            let (status, path) = porcelain(line);
            let letter = if status == "??" {
                'A'
            } else {
                status.chars().next().unwrap()
            };
            (letter, path.to_lowercase())
        })
        .collect();
    assert_eq!(from_git, files);
    let subject = fixture
        .git(own, &["log", "-1", "--format=%s"])
        .unwrap()
        .to_lowercase();
    assert!(subject.starts_with(&base), "{subject} against {base}");

    // `who-file`: landed authors from git, open sessions from the worktrees.
    let Answer::Agents(who) = truth("who-file") else {
        panic!()
    };
    let log = fixture
        .git(
            &fixture.git_root,
            &[
                "log",
                "--format=%an",
                "main",
                "--",
                "crates/reef-client/src/json.rs",
            ],
        )
        .unwrap();
    let mut from_git: BTreeSet<(String, String)> = log
        .lines()
        .filter(|a| *a != "import")
        .map(|a| (a.to_owned(), "landed".to_owned()))
        .collect();
    for (agent, worktree) in &fixture.worktrees {
        let touched = fixture
            .git(
                worktree,
                &[
                    "status",
                    "--porcelain",
                    "--",
                    "crates/reef-client/src/json.rs",
                ],
            )
            .unwrap();
        if !touched.is_empty() {
            from_git.insert((agent.clone(), "open".to_owned()));
        }
    }
    assert_eq!(from_git, who);

    // `who-symbol`: landed authors from `git log -L`, open sessions whose
    // function-context diff of the file names the function.
    let Answer::Agents(who) = truth("who-symbol") else {
        panic!()
    };
    let query = "crates/reef-daemon/src/symbols/query.rs";
    let log = fixture
        .git(
            &fixture.git_root,
            &[
                "log",
                "-L",
                &format!(":by_lookup:{query}"),
                "-s",
                "--format=%an",
                "main",
            ],
        )
        .unwrap();
    let mut from_git: BTreeSet<(String, String)> = log
        .lines()
        .filter(|a| !a.is_empty() && *a != "import")
        .map(|a| (a.to_owned(), "landed".to_owned()))
        .collect();
    for (agent, worktree) in &fixture.worktrees {
        let diff = fixture
            .git(worktree, &["diff", "--function-context", "--", query])
            .unwrap();
        if diff
            .lines()
            .any(|l| l.starts_with(['+', '-']) && l.contains("fn by_lookup("))
        {
            from_git.insert((agent.clone(), "open".to_owned()));
        }
    }
    assert_eq!(from_git, who);

    // `reef history` reports the landed change by author email and the open
    // ones by agent.
    let history: serde_json::Value = serde_json::from_str(
        &fixture
            .reef(
                &fixture.reef_root,
                &[
                    "history",
                    "--repo",
                    &fixture.repo_id,
                    "crates/reef-client/src/json.rs",
                ],
            )
            .unwrap(),
    )
    .unwrap();
    let changes = history["changes"].as_array().unwrap();
    assert!(
        changes
            .iter()
            .any(|c| c["outcome"] == "landed" && c["author_email"] == "bot-2@agents.invalid")
    );
    for agent in ["bot-4", "bot-5"] {
        assert!(
            changes.iter().any(|c| c["outcome"] == "open"
                && c["activity"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .any(|a| a["agent"] == agent)),
            "{agent} is not an open change's agent"
        );
    }

    // `changed-*`: the truth's names are Reef's symbol diff's functions and
    // methods.
    for (id, agent, path) in [
        ("changed-small", "bot-4", None),
        (
            "changed-large",
            "bot-5",
            Some("crates/reef-daemon/src/symbols/"),
        ),
    ] {
        let Answer::Symbols {
            added,
            removed,
            changed,
        } = truth(id)
        else {
            panic!()
        };
        let mut args = vec![
            "diff",
            "-r",
            &fixture.workspaces[agent].change,
            "--symbols",
            "--call-sites",
            "0",
        ];
        args.extend(path);
        let diff: serde_json::Value =
            serde_json::from_str(&fixture.reef(&fixture.reef_root, &args).unwrap()).unwrap();
        let mut names: BTreeSet<String> = BTreeSet::new();
        for file in diff["files"].as_array().unwrap() {
            for symbol in file["symbols"]["changes"].as_array().into_iter().flatten() {
                let kind = symbol["symbol_kind"].as_str().unwrap_or_default();
                if kind == "function" || kind == "method" {
                    names.insert(symbol["name"].as_str().unwrap().to_lowercase());
                }
            }
        }
        let wanted: BTreeSet<String> = added.into_iter().chain(removed).chain(changed).collect();
        assert_eq!(names, wanted, "{id}");
    }

    // Every arm of every question has a sufficient candidate, in bytes,
    // the `mcp` ones through the harness's own client; and nothing changed
    // the fixture.
    let before = fixture.fingerprint().unwrap();
    let out = tempfile::tempdir().unwrap();
    let mut counters: Vec<Box<dyn Counter>> = vec![Box::new(Bytes)];
    let mut run = Scripted {
        scenario,
        fixture,
        counters: &mut counters,
        out_dir: out.path(),
        failed: Vec::new(),
    };
    let fixed = run.fixed_cost().unwrap();
    assert!(fixed.tools > 0 && fixed.tools_list_bytes > 0);
    let results = run.candidates(&[]).unwrap();
    for question in &scenario.questions {
        for arm in Arm::ALL {
            // For `reef`, one an agent gets unasked: not `-o text`.
            assert!(
                cheapest(&results, &question.id, arm, "bytes", |c| !c.text_output).is_some(),
                "{} has no sufficient {arm} candidate",
                question.id
            );
        }
    }
    assert_eq!(
        fixture.fingerprint().unwrap(),
        before,
        "the scripted run changed the fixture"
    );
}
