//! `reef init` (R1.27): the brief's tests. Only the registration tests
//! run a [`TestDaemon`]; the rest pass `--no-register`, which never
//! contacts one. `HOME` is always a temporary directory, so `--user` never
//! writes the real user-level skill.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::fs;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::assert::OutputAssertExt as _;
use reef_daemon::testing::TestDaemon;
use serde_json::Value;
use tempfile::TempDir;

/// A git repository with one commit, and a home directory beside it.
struct Fixture {
    tmp: TempDir,
    root: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let tmp = tempfile::tempdir().unwrap();
        let root = tmp.path().join("repo");
        fs::create_dir(&root).unwrap();
        reef_testing::git::init_repo(&root, &[("README", "hello\n")]);
        let root = root.canonicalize().unwrap();
        fs::create_dir(tmp.path().join("home")).unwrap();
        Self { tmp, root }
    }

    fn home(&self) -> PathBuf {
        self.tmp.path().join("home")
    }

    /// `reef --no-autostart` run from the root, against `state`.
    fn reef(&self, state: &Path) -> Command {
        let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
        for var in [
            "REEF_SOCKET",
            "REEF_CONFIG",
            "REEF_OUTPUT",
            "REEF_WORKSPACE",
            "REEF_AGENT",
        ] {
            command.env_remove(var);
        }
        command
            .env("REEF_STATE_DIR", state)
            .env("HOME", self.home())
            .current_dir(&self.root)
            .arg("--no-autostart");
        command
    }

    /// `reef init --no-register` with `args`, never near a daemon.
    fn init(&self, args: &[&str]) -> Output {
        let state = self.tmp.path().join("no-daemon");
        self.reef(&state)
            .args(["--no-register", "init"])
            .args(args)
            .output()
            .unwrap()
    }

    fn read(&self, name: &str) -> String {
        fs::read_to_string(self.root.join(name)).unwrap()
    }

    fn write(&self, name: &str, text: &str) {
        let path = self.root.join(name);
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(path, text).unwrap();
    }
}

fn stdout(output: &Output) -> String {
    String::from_utf8(output.stdout.clone()).unwrap()
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Stdout as one JSON value, after asserting success.
fn json(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)))
}

/// `(path relative to the root, action)` of each file `init` reported.
fn actions(value: &Value, root: &Path) -> Vec<(String, String)> {
    value["files"]
        .as_array()
        .unwrap()
        .iter()
        .map(|file| {
            let path = Path::new(file["path"].as_str().unwrap());
            let path = path.strip_prefix(root).unwrap_or(path);
            (
                path.display().to_string(),
                file["action"].as_str().unwrap().to_owned(),
            )
        })
        .collect()
}

fn pairs(list: &[(&str, &str)]) -> Vec<(String, String)> {
    list.iter()
        .map(|(path, action)| ((*path).to_owned(), (*action).to_owned()))
        .collect()
}

/// Every file under `dir`, relative to it, without `.git` and `.jj`.
fn files(dir: &Path) -> Vec<String> {
    fn walk(dir: &Path, root: &Path, out: &mut Vec<String>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            let name = path.file_name().unwrap();
            if name == ".git" || name == ".jj" {
                continue;
            }
            if path.is_dir() {
                walk(&path, root, out);
            } else {
                out.push(path.strip_prefix(root).unwrap().display().to_string());
            }
        }
    }
    let mut out = Vec::new();
    walk(dir, dir, &mut out);
    out.sort();
    out
}

/// The section between (and including) the marker lines.
fn section(text: &str) -> &str {
    let begin = text.find("<!-- reef:begin v1 -->").unwrap();
    let end = text.find("<!-- reef:end -->").unwrap() + "<!-- reef:end -->".len();
    &text[begin..end]
}

#[tokio::test(flavor = "multi_thread")]
async fn fresh_git_repo_gets_agents_md_only() {
    let daemon = TestDaemon::start().await;
    let repo = Fixture::new();
    let mut command = repo.reef(daemon.state_dir());
    command.arg("init");
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["registered"]["root"], repo.root.to_str().unwrap());
    assert_eq!(value["registered"]["colocated"], true);
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[("AGENTS.md", "wrote")])
    );
    let text = stderr(&output);
    assert!(text.contains("and initialized jj alongside git"), "{text}");
    assert!(text.contains("files written, not committed"), "{text}");
    assert!(repo.root.join(".jj").is_dir());
    assert_eq!(files(&repo.root), ["AGENTS.md", "README"]);
    let agents = repo.read("AGENTS.md");
    assert!(agents.starts_with("<!-- reef:begin v1 -->\n"), "{agents}");
    assert!(agents.ends_with("<!-- reef:end -->\n"), "{agents}");
    assert!(agents.contains("reef ws run -- <command>"), "{agents}");

    let mut command = repo.reef(daemon.state_dir());
    command.args(["repo", "list"]);
    let listed = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    let listed = json(&listed);
    assert_eq!(listed.as_array().unwrap().len(), 1, "{listed}");
    assert_eq!(listed[0]["root"], repo.root.to_str().unwrap());

    // Again: registered already, nothing to write, neither said.
    let mut command = repo.reef(daemon.state_dir());
    command.arg("init");
    let output = tokio::task::spawn_blocking(move || command.output().unwrap())
        .await
        .unwrap();
    let value = json(&output);
    assert_eq!(value["registered"]["root"], repo.root.to_str().unwrap());
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[("AGENTS.md", "unchanged")])
    );
    assert_eq!(stderr(&output), "");
}

#[test]
fn detects_claude_and_cursor() {
    let repo = Fixture::new();
    fs::create_dir(repo.root.join(".claude")).unwrap();
    fs::create_dir(repo.root.join(".cursor")).unwrap();
    let value = json(&repo.init(&[]));
    assert_eq!(value["registered"], Value::Null);
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[
            ("AGENTS.md", "wrote"),
            ("CLAUDE.md", "wrote"),
            (".claude/skills/reef/SKILL.md", "wrote"),
            (".cursor/rules/reef.mdc", "wrote"),
        ])
    );
    assert_eq!(
        section(&repo.read("CLAUDE.md")),
        section(&repo.read("AGENTS.md"))
    );
    let skill = repo.read(".claude/skills/reef/SKILL.md");
    assert!(
        skill.starts_with("---\nname: reef\ndescription: "),
        "{skill}"
    );
    assert!(skill.contains("## Commands"), "{skill}");
    let rule = repo.read(".cursor/rules/reef.mdc");
    assert!(rule.contains("\nalwaysApply: false\n"), "{rule}");
    assert!(rule.contains("## Commands"), "{rule}");

    // Named instead of detected: only what is named, and AGENTS.md.
    let other = Fixture::new();
    let value = json(&other.init(&["--agents", "cursor"]));
    assert_eq!(
        actions(&value, &other.root),
        pairs(&[("AGENTS.md", "wrote"), (".cursor/rules/reef.mdc", "wrote")])
    );
    let output = other.init(&["--agents", "auto,claude"]);
    output.clone().assert().code(2);
    assert!(
        stderr(&output).contains("--agents auto"),
        "{}",
        stderr(&output)
    );
    let value = json(&other.init(&["--agents", "none"]));
    assert_eq!(actions(&value, &other.root), pairs(&[]));
    let value = json(&other.init(&["--agents", "all"]));
    assert_eq!(
        actions(&value, &other.root),
        pairs(&[
            ("AGENTS.md", "unchanged"),
            ("CLAUDE.md", "wrote"),
            (".claude/skills/reef/SKILL.md", "wrote"),
            (".cursor/rules/reef.mdc", "unchanged"),
        ])
    );
}

#[test]
fn damaged_markers_and_newer_sections_are_skipped() {
    let repo = Fixture::new();
    let damaged = "<!-- reef:begin v1 -->\nno end\n";
    repo.write("AGENTS.md", damaged);
    let output = repo.init(&["-o", "text"]);
    output.clone().assert().success();
    assert!(
        stdout(&output).contains("skipped AGENTS.md (markers damaged)\n"),
        "{}",
        stdout(&output)
    );
    assert_eq!(repo.read("AGENTS.md"), damaged);
    let newer = "<!-- reef:begin v9 -->\nnewer\n<!-- reef:end -->\n";
    repo.write("AGENTS.md", newer);
    let value = json(&repo.init(&[]));
    assert_eq!(
        value["files"][0]["reason"],
        "section v9 is from a newer reef"
    );
    assert_eq!(repo.read("AGENTS.md"), newer);
}

/// `--check` registers nothing, so it needs no daemon even without
/// `--no-register`.
#[test]
fn check_runs_without_a_daemon() {
    let repo = Fixture::new();
    let state = repo.tmp.path().join("no-daemon");
    let output = repo
        .reef(&state)
        .args(["init", "--check"])
        .output()
        .unwrap();
    output.clone().assert().code(3);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["next"], serde_json::json!(["reef", "init"]));
    assert!(!repo.root.join(".jj").exists());
}

#[test]
fn appends_to_existing_agents_md_without_touching_the_rest() {
    let repo = Fixture::new();
    let mine = "# Our agents\n\nRun `just ci` before you push.\n";
    repo.write("AGENTS.md", mine);
    let value = json(&repo.init(&[]));
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[("AGENTS.md", "updated")])
    );
    let text = repo.read("AGENTS.md");
    let at = text.find("<!-- reef:begin").unwrap();
    assert_eq!(&text[..at], format!("{mine}\n"));
    assert_eq!(&text[at + section(&text).len()..], "\n");

    // Edits around the section survive the next run byte for byte.
    let edited = format!("{text}\n## Later\n\nmore of ours\n");
    repo.write("AGENTS.md", &edited);
    let value = json(&repo.init(&[]));
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[("AGENTS.md", "unchanged")])
    );
    assert_eq!(repo.read("AGENTS.md"), edited);
}

#[test]
fn replaces_a_stale_section() {
    let repo = Fixture::new();
    let stale =
        "before\n<!-- reef:begin v1 -->\nWritten by an old reef.\n<!-- reef:end -->\nafter\n";
    repo.write("AGENTS.md", stale);
    let value = json(&repo.init(&[]));
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[("AGENTS.md", "updated")])
    );
    let text = repo.read("AGENTS.md");
    assert!(
        text.starts_with("before\n<!-- reef:begin v1 -->\n"),
        "{text}"
    );
    assert!(text.ends_with("<!-- reef:end -->\nafter\n"), "{text}");
    assert!(!text.contains("old reef"), "{text}");
    let value = json(&repo.init(&[]));
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[("AGENTS.md", "unchanged")])
    );
}

#[test]
fn claude_md_importing_agents_md_is_skipped() {
    let repo = Fixture::new();
    repo.write("CLAUDE.md", "@AGENTS.md\n");
    let value = json(&repo.init(&[]));
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[
            ("AGENTS.md", "wrote"),
            ("CLAUDE.md", "skipped"),
            (".claude/skills/reef/SKILL.md", "wrote"),
        ])
    );
    assert_eq!(value["files"][1]["reason"], "imports AGENTS.md");
    assert_eq!(repo.read("CLAUDE.md"), "@AGENTS.md\n");
    let output = repo.init(&["-o", "text"]);
    assert!(
        stdout(&output).contains("skipped CLAUDE.md (imports AGENTS.md)\n"),
        "{}",
        stdout(&output)
    );
}

#[test]
fn foreign_skill_file_is_not_overwritten() {
    let repo = Fixture::new();
    let foreign = "---\nname: reef\ndescription: ours\n---\n\nOur own notes.\n";
    repo.write(".claude/skills/reef/SKILL.md", foreign);
    let value = json(&repo.init(&[]));
    let skill = &value["files"][2];
    assert_eq!(skill["action"], "skipped", "{value}");
    assert_eq!(skill["reason"], "not written by reef init");
    assert_eq!(repo.read(".claude/skills/reef/SKILL.md"), foreign);
    // A skipped file is not stale: there is nothing init would write.
    repo.init(&["--check"]).assert().success();
}

#[test]
fn check_exit_codes() {
    let repo = Fixture::new();
    fs::create_dir(repo.root.join(".claude")).unwrap();
    let output = repo.init(&["--check"]);
    output.clone().assert().code(3);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(value["error"]["kind"], "not_found");
    let message = value["error"]["message"].as_str().unwrap();
    assert!(message.contains("AGENTS.md (missing)"), "{message}");
    assert!(message.contains("SKILL.md (missing)"), "{message}");
    assert_eq!(
        value["error"]["next"],
        serde_json::json!(["reef", "init", "--no-register"])
    );
    assert_eq!(files(&repo.root), ["README"], "--check wrote something");

    json(&repo.init(&[]));
    let value = json(&repo.init(&["--check"]));
    assert!(
        actions(&value, &repo.root)
            .iter()
            .all(|(_, action)| action == "unchanged"),
        "{value}"
    );

    repo.write("CLAUDE.md", "mine\n");
    let output = repo.init(&["--check", "--agents", "claude"]);
    output.clone().assert().code(3);
    let value: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert!(
        value["error"]["message"]
            .as_str()
            .unwrap()
            .contains("CLAUDE.md (stale)"),
        "{value}"
    );
    assert_eq!(
        value["error"]["next"],
        serde_json::json!(["reef", "init", "--no-register", "--agents", "claude"])
    );
}

#[test]
fn print_matches_section() {
    let repo = Fixture::new();
    let printed = repo.init(&["--print"]);
    printed.clone().assert().success();
    // Text even on a pipe, and nothing written.
    let printed = stdout(&printed);
    assert!(printed.starts_with("<!-- reef:begin v1 -->\n"), "{printed}");
    assert_eq!(files(&repo.root), ["README"]);
    json(&repo.init(&[]));
    assert_eq!(repo.read("AGENTS.md"), printed);
    let value = json(&repo.init(&["--print", "-o", "json"]));
    assert_eq!(format!("{}\n", value["section"].as_str().unwrap()), printed);
}

#[test]
fn user_skill() {
    let repo = Fixture::new();
    let value = json(&repo.init(&["--user"]));
    let user = repo.home().join(".claude/skills/reef/SKILL.md");
    assert_eq!(
        actions(&value, &repo.root),
        pairs(&[
            ("AGENTS.md", "wrote"),
            ("CLAUDE.md", "wrote"),
            (".claude/skills/reef/SKILL.md", "wrote"),
            (user.to_str().unwrap(), "wrote"),
        ])
    );
    assert_eq!(
        fs::read_to_string(&user).unwrap(),
        repo.read(".claude/skills/reef/SKILL.md")
    );

    let output = repo
        .reef(&repo.tmp.path().join("no-daemon"))
        .env_remove("HOME")
        .args(["--no-register", "init", "--user"])
        .output()
        .unwrap();
    output.clone().assert().code(2);
    assert!(
        stderr(&output).contains("HOME is not set"),
        "{}",
        stderr(&output)
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn init_is_non_interactive() {
    let daemon = TestDaemon::start().await;
    let repo = Fixture::new();
    repo.write("AGENTS.md", "mine\n");
    repo.write(".claude/skills/reef/SKILL.md", "not ours\n");
    for args in [&["init"][..], &["init", "--check"], &["init", "--print"]] {
        let mut command = repo.reef(daemon.state_dir());
        command
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        let status = tokio::task::spawn_blocking(move || {
            let mut child = command.spawn().unwrap();
            let started = Instant::now();
            loop {
                if let Some(status) = child.try_wait().unwrap() {
                    return status;
                }
                // A command waiting on stdin never finishes.
                if started.elapsed() > Duration::from_secs(60) {
                    let _ = child.kill();
                    panic!("reef {args:?} did not finish with stdin closed");
                }
                std::thread::sleep(Duration::from_millis(20));
            }
        })
        .await
        .unwrap();
        assert!(status.success(), "{args:?}: {status}");
    }
}
