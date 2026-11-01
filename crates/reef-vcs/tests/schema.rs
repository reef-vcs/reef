//! `reef schema` (R2.24): offline, its parts and names, and every
//! command's output checked against the schema it publishes. Tests are
//! numbered as the brief numbers them.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::time::{Duration, Instant};

use reef_daemon::testing::{SyntheticRepo, TestDaemon, init_synthetic_repo, short_tmp};
use reef_testing::synth::Preset;
use serde_json::{Value, json};

/// `reef` with a clean environment and `state` as its state directory.
fn reef_in(state: &Path) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_OUTPUT",
        "REEF_WORKSPACE",
        "REEF_AGENT",
        "REEF_REMOTE",
    ] {
        command.env_remove(var);
    }
    command.env("REEF_STATE_DIR", state);
    command
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[track_caller]
fn json(output: &Output) -> Value {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_str(&stdout(output)).unwrap_or_else(|e| panic!("{e}: {}", stdout(output)))
}

// 8
#[test]
fn schema_runs_offline() {
    let tmp = tempfile::Builder::new()
        .prefix("reef-")
        .tempdir_in(short_tmp())
        .unwrap();
    let state = tmp.path().join("state");
    let run = |args: &[&str]| {
        let output = reef_in(&state)
            .arg("--no-autostart")
            .args(args)
            .output()
            .unwrap();
        common::schema::check(args, &output);
        output
    };
    // On a pipe: one compact object and a newline; pretty with -o json.
    let piped = run(&["schema"]);
    let text = stdout(&piped);
    assert!(
        text.ends_with("}\n") && text.lines().count() == 1,
        "not compact"
    );
    let document: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(document["reef"]["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(document["reef"]["api_revision"], reef_api::API_REVISION);
    assert!(stdout(&run(&["schema", "-o", "json"])).lines().count() > 1);
    assert!(!state.exists(), "reef schema made a state directory");
    // Names select, as clap resolves them.
    let one = json(&run(&["schema", "cli", "ws", "list"]));
    let alias = json(&run(&["schema", "cli", "workspace", "ls"]));
    assert_eq!(one, alias);
    assert_eq!(one["cli"]["commands"].as_array().unwrap().len(), 1);
    assert!(one.get("mcp").is_none() && one.get("api").is_none());
    let defs: Vec<&String> = one["$defs"].as_object().unwrap().keys().collect();
    assert!(
        defs.iter().any(|name| *name == "brief.Workspace"),
        "{defs:?}"
    );
    assert!(
        !defs.iter().any(|name| name.starts_with("reef.api")),
        "{defs:?}"
    );
    let tool = json(&run(&["schema", "mcp", "get_status"]));
    assert_eq!(tool["mcp"]["tools"].as_array().unwrap().len(), 1);
    assert!(tool["mcp"]["tools"][0]["outputSchema"].is_object());
    let service = json(&run(&["schema", "api", "WorkspaceService"]));
    let full = json(&run(&["schema", "api", "reef.api.v1.WorkspaceService"]));
    assert_eq!(service, full);
    assert_eq!(service["api"]["services"].as_object().unwrap().len(), 1);
    let message = json(&run(&["schema", "api", "Workspace"]));
    assert_eq!(
        message["api"]["schema"]["$ref"],
        "#/$defs/reef.api.v1.Workspace"
    );
    // An unknown name: not_found, and a `next` that runs.
    let output = run(&["schema", "cli", "nope"]);
    assert_eq!(output.status.code(), Some(3));
    let error: Value = serde_json::from_str(&stdout(&output)).unwrap();
    assert_eq!(error["error"]["reason"], "SCHEMA_NAME_NOT_FOUND");
    assert_eq!(error["error"]["next"], json!(["reef", "schema", "cli"]));
    assert!(run(&["schema", "cli"]).status.success());
    // The descriptor set, byte for byte (not JSON, so not checked).
    let set = reef_in(&state)
        .args(["--no-autostart", "schema", "--descriptor-set"])
        .output()
        .unwrap();
    assert!(set.status.success());
    assert_eq!(set.stdout, reef_api::FILE_DESCRIPTOR_SET);
    // A remote is ignored: the document describes this binary.
    let remote = run(&["--remote", "nowhere", "schema"]);
    assert_eq!(json(&remote), document);
    assert!(!state.exists());
}

/// Commands test 9 does not run, and why. Each is still covered by the
/// unit tests of its inputs and row, and some by other tests' checks.
const NOT_RUN: &[(&[&str], &str)] = &[
    (
        &["daemon", "run"],
        "replaces the process that hosts the test daemon",
    ),
    (
        &["daemon", "start"],
        "would start a second daemon; tests/cli.rs checks it",
    ),
    (&["daemon", "stop"], "would signal this process"),
    (&["restore"], "needs the daemon stopped"),
    (&["store", "build"], "needs a store spec and a build"),
    (&["symbols", "upload"], "needs a SCIP index"),
    (
        &["presence", "hook", "edit"],
        "reads a hook event on stdin; tests/presence.rs runs it, checked against its row",
    ),
    (
        &["presence", "hook", "end"],
        "reads a hook event on stdin; tests/presence.rs runs it, checked against its row",
    ),
    (
        &["remote", "add"],
        "needs a client profile directory and a token on stdin",
    ),
    (&["remote", "remove"], "needs a saved profile"),
    (&["mcp"], "prints the MCP protocol, not JSON output"),
    (&["completions"], "prints a shell script"),
];

/// Runs `reef` command lines against one daemon, checks each one's stdout
/// against its schema, and records which commands ran.
struct Script {
    state: PathBuf,
    ran: BTreeSet<Vec<String>>,
}

impl Script {
    async fn run(&mut self, dir: &Path, args: &[&str]) -> Output {
        let mut command = reef_in(&self.state);
        command.current_dir(dir).arg("--no-autostart").args(args);
        let output = tokio::task::spawn_blocking(move || command.output().unwrap())
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "reef {args:?}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        common::schema::check(args, &output);
        let entry = common::schema::command_of(args).unwrap();
        self.ran
            .insert(serde_json::from_value(entry["command"].clone()).unwrap());
        output
    }

    /// A watch (`op watch`, `presence watch`) while `trigger` makes an
    /// event, in `form` (`json` or `brief`): its lines, checked.
    async fn watch(&mut self, dir: &Path, form: &str, args: &[&str], trigger: &[&str]) {
        let mut command = reef_in(&self.state);
        let mut line = vec!["-o", form];
        line.extend_from_slice(args);
        line.extend(["--for", "4"]);
        command.current_dir(dir).arg("--no-autostart").args(&line);
        let watching = tokio::task::spawn_blocking(move || command.output().unwrap());
        tokio::time::sleep(Duration::from_millis(1500)).await;
        self.run(dir, trigger).await;
        let output = watching.await.unwrap();
        assert!(output.status.success(), "reef {line:?}: {output:?}");
        assert!(!output.stdout.is_empty(), "reef {line:?} saw nothing");
        common::schema::check(&line, &output);
        let entry = common::schema::command_of(&line).unwrap();
        self.ran
            .insert(serde_json::from_value(entry["command"].clone()).unwrap());
    }

    /// With `-o json`, then on a pipe; the JSON form's value.
    async fn both(&mut self, dir: &Path, args: &[&str]) -> Value {
        let mut with_json = vec!["-o", "json"];
        with_json.extend_from_slice(args);
        let value = json(&self.run(dir, &with_json).await);
        self.run(dir, args).await;
        value
    }

    /// With `-o json` only.
    async fn json(&mut self, dir: &Path, args: &[&str]) -> Value {
        let mut with_json = vec!["-o", "json"];
        with_json.extend_from_slice(args);
        json(&self.run(dir, &with_json).await)
    }
}

/// A daemon with its remote listener on a loopback port, for `remote
/// show` and the token commands.
async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.remote.enabled = true;
        "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
        config.file.remote.names = vec!["localhost".to_owned()];
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
    })
    .await;
    let deadline = Instant::now() + Duration::from_secs(10);
    while daemon.handle.state().remote.bound_addr().is_none() {
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    (daemon, repo)
}

fn text(value: &Value) -> String {
    value.as_str().unwrap().to_owned()
}

// 9
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one script, in the order it runs")]
async fn outputs_match_their_schemas() {
    let (daemon, repo) = fixture().await;
    // `config show` runs this build's `reef-daemon` (R2.05).
    common::daemon_bin();
    let mut script = Script {
        state: daemon.state_dir().to_owned(),
        ran: BTreeSet::new(),
    };
    let root = repo.root.clone();

    // Workspaces.
    let a = script
        .json(&root, &["ws", "create", "--purpose", "schema"])
        .await;
    let a_id = text(&a["id"]);
    let a_dir = PathBuf::from(text(&a["path"]));
    let a_change = text(&a["change_id"]);
    let b = json(&script.run(&root, &["-o", "json", "ws", "create"]).await);
    script.run(&root, &["ws", "create"]).await;
    let c = script.json(&root, &["ws", "create"]).await;
    script.both(&root, &["ws", "list"]).await;
    script.both(&root, &["ws", "inspect", &a_id]).await;
    std::fs::create_dir_all(a_dir.join("py")).unwrap();
    std::fs::write(a_dir.join("py/a.py"), "def f():\n    return 1\n\nf()\n").unwrap();
    std::fs::write(a_dir.join("notes.txt"), "one\ntwo\n").unwrap();
    script.both(&root, &["ws", "snapshot", &a_id]).await;
    script.both(&root, &["ws", "renew", &a_id]).await;
    script
        .json(
            &root,
            &["ws", "move", &text(&b["id"]), "--new-on", &a_change],
        )
        .await;
    script
        .run(
            &root,
            &["ws", "move", &text(&c["id"]), "--new-on", &a_change],
        )
        .await;

    // The VCS commands, inside the workspace.
    script.both(&a_dir, &["status"]).await;
    script.both(&a_dir, &["log"]).await;
    script.both(&a_dir, &["show"]).await;
    script.both(&a_dir, &["diff"]).await;
    script.both(&a_dir, &["diff", "--stat"]).await;
    let described = script
        .both(&a_dir, &["describe", "-m", "schema test"])
        .await;
    let op = text(&described["operation"]["id"]);
    let last = script.json(&a_dir, &["describe", "-m", "to undo"]).await;
    script
        .json(&a_dir, &["undo", &text(&last["operation"]["id"])])
        .await;
    script.both(&a_dir, &["op", "log"]).await;
    script.both(&a_dir, &["op", "show", &op]).await;
    script
        .watch(
            &a_dir,
            "json",
            &["op", "watch"],
            &["describe", "-m", "watched"],
        )
        .await;
    script
        .watch(
            &a_dir,
            "brief",
            &["op", "watch"],
            &["describe", "-m", "again"],
        )
        .await;
    let child = script
        .json(&root, &["ws", "create", "--new-on", &a_change])
        .await;
    script
        .json(&a_dir, &["abandon", &text(&child["change_id"])])
        .await;
    script.both(&a_dir, &["history", "notes.txt"]).await;
    script.both(&a_dir, &["file", "list"]).await;
    script.json(&a_dir, &["file", "read", "notes.txt"]).await;
    script
        .run(&a_dir, &["-o", "brief", "file", "read", "notes.txt"])
        .await;
    script
        .both(&a_dir, &["symbols", "index", "--wait", "5m"])
        .await;
    script.both(&a_dir, &["symbols", "list", "py/a.py"]).await;
    script.both(&a_dir, &["diff", "--symbols"]).await;
    script.both(&a_dir, &["here"]).await;
    script.both(&root, &["here"]).await;

    // Presence.
    let reported = script
        .json(&a_dir, &["presence", "report", "checking schemas"])
        .await;
    script.run(&a_dir, &["presence", "report", "again"]).await;
    script
        .watch(
            &a_dir,
            "json",
            &["presence", "watch"],
            &["presence", "report", "x"],
        )
        .await;
    script
        .watch(
            &a_dir,
            "brief",
            &["presence", "watch"],
            &["presence", "report", "y"],
        )
        .await;
    script
        .json(&a_dir, &["presence", "clear", &text(&reported["id"])])
        .await;

    // Repositories, stores, the daemon, the web UI.
    script.both(&root, &["repo", "list"]).await;
    let other = daemon.tmp.path().join("other");
    init_synthetic_repo(&other, Preset::Small);
    let other_text = other.to_str().unwrap();
    script.json(&root, &["repo", "register", other_text]).await;
    script.json(&root, &["repo", "forget", other_text]).await;
    script.both(&root, &["store", "list"]).await;
    script.both(&root, &["daemon", "status"]).await;
    script.both(&root, &["ui", "--print"]).await;
    let dest = daemon.tmp.path().join("backup");
    script
        .json(&root, &["backup", dest.to_str().unwrap()])
        .await;

    // The remote listener and its tokens.
    script.both(&root, &["remote", "show"]).await;
    script.both(&root, &["remote", "list"]).await;
    script
        .json(&root, &["remote", "token", "create", "one"])
        .await;
    script
        .run(&root, &["remote", "token", "create", "two"])
        .await;
    script.both(&root, &["remote", "token", "list"]).await;
    script
        .json(&root, &["remote", "token", "rotate", "one"])
        .await;
    script
        .run(&root, &["remote", "token", "rotate", "one"])
        .await;
    script
        .json(&root, &["remote", "token", "revoke", "one"])
        .await;
    script
        .run(&root, &["remote", "token", "revoke", "two"])
        .await;

    // Commands that print text by default.
    script.json(&root, &["guide"]).await;
    script.run(&root, &["-o", "brief", "guide"]).await;
    script.json(&root, &["init", "--print"]).await;
    script.json(&root, &["init"]).await;
    script.run(&root, &["init", "--print"]).await;
    script.json(&root, &["config", "show"]).await;
    script
        .run(&root, &["-o", "json", "ws", "run", "--", "true"])
        .await;
    script
        .run(&root, &["-o", "brief", "ws", "run", "--", "true"])
        .await;
    script.both(&root, &["schema", "cli", "status"]).await;

    // Releases and GC last: they end the workspaces.
    script.json(&root, &["ws", "release", &a_id]).await;
    script.run(&root, &["ws", "release", &text(&b["id"])]).await;
    script.both(&root, &["ws", "gc", "--dry-run"]).await;

    // Every command ran or says why not.
    let document = common::schema::document();
    for entry in document["cli"]["commands"].as_array().unwrap() {
        let words: Vec<String> = serde_json::from_value(entry["command"].clone()).unwrap();
        let listed = NOT_RUN
            .iter()
            .any(|(command, _)| command.iter().eq(words.iter()));
        assert!(
            script.ran.contains(&words) != listed,
            "{words:?}: {}",
            if listed {
                "ran, and is listed as not run"
            } else {
                "neither run nor listed in NOT_RUN"
            }
        );
    }
}
