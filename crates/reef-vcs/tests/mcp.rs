//! R2.13: `reef mcp` over stdio against a daemon. Tests are numbered as
//! the brief numbers them.
//!
//! Each runs a [`TestDaemon`] in this process with the watcher's snapshots
//! held off, so only the tools snapshot, and `reef --no-autostart mcp`
//! against its socket from a chosen directory, driven by the hand-written
//! client in `common/mcp.rs`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use common::mcp::{Mcp, result_of};
use reef_api::v1::{self, GetWorkspaceRequest};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, short_tmp};
use reef_testing::synth::Preset;
use serde_json::{Value, json};
use tokio::process::Command;

/// The tools, in `tools/list`'s order.
const TOOLS: [&str; 21] = [
    "list_repositories",
    "list_workspaces",
    "get_workspace",
    "get_status",
    "list_changes",
    "get_change",
    "get_diff",
    "list_operations",
    "query_changes",
    "get_operation",
    "read_file",
    "list_files",
    "wait_for_operations",
    "create_workspace",
    "snapshot_workspace",
    "move_workspace",
    "release_workspace",
    "describe_change",
    "abandon_change",
    "undo_operation",
    "edit_files",
];

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn fixture_with(
    configure: impl FnOnce(&mut reef_daemon::config::Config),
) -> (TestDaemon, SyntheticRepo) {
    TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        configure(config);
    })
    .await
}

/// `reef --no-autostart <args> mcp` against `state`, from `dir`, with a
/// clean environment.
fn command_in(state: &Path, dir: &Path, args: &[&str]) -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    for var in [
        "REEF_SOCKET",
        "REEF_CONFIG",
        "REEF_LOG",
        "RUST_LOG",
        "REEF_LOG_FORMAT",
        "REEF_OUTPUT",
        "REEF_WORKSPACE",
        "REEF_AGENT",
    ] {
        command.env_remove(var);
    }
    command
        .env("REEF_STATE_DIR", state)
        .current_dir(dir)
        .arg("--no-autostart")
        .args(args)
        .arg("mcp");
    command
}

fn command(daemon: &TestDaemon, dir: &Path) -> Command {
    command_in(daemon.state_dir(), dir, &[])
}

async fn server(daemon: &TestDaemon, dir: &Path) -> Mcp {
    Mcp::start(command(daemon, dir)).await
}

/// A workspace created through `mcp`.
async fn create(mcp: &mut Mcp) -> Value {
    mcp.ok("create_workspace", json!({})).await
}

fn id(ws: &Value) -> String {
    ws["id"].as_str().unwrap().to_owned()
}

fn path(ws: &Value) -> PathBuf {
    PathBuf::from(ws["path"].as_str().unwrap())
}

fn write(ws: &Value, file: &str, contents: impl AsRef<[u8]>) {
    let file = path(ws).join(file);
    std::fs::create_dir_all(file.parent().unwrap()).unwrap();
    std::fs::write(file, contents).unwrap();
}

/// A new empty directory in no checkout.
fn new_dir(daemon: &TestDaemon, name: &str) -> PathBuf {
    let dir = daemon.tmp.path().join(name);
    std::fs::create_dir_all(&dir).unwrap();
    dir.canonicalize().unwrap()
}

/// The workspace as the daemon reports it, without a snapshot.
async fn inspect(daemon: &TestDaemon, ws: &str) -> v1::Workspace {
    daemon
        .client
        .get_workspace(GetWorkspaceRequest {
            repo: String::new(),
            workspace: ws.to_owned(),
            snapshot_first: false,
            skip_disk_usage: true,
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn owner() -> String {
    format!("{}:mcp", reef_client::default_owner())
}

// 1
#[tokio::test(flavor = "multi_thread")]
async fn handshake_and_tools() {
    let (daemon, repo) = fixture().await;
    let mut mcp = server(&daemon, &repo.root).await;
    let init = mcp.init.clone();
    assert_eq!(init["serverInfo"]["name"], "reef", "{init}");
    assert_eq!(init["serverInfo"]["version"], env!("CARGO_PKG_VERSION"));
    let capabilities: Vec<&String> = init["capabilities"].as_object().unwrap().keys().collect();
    assert_eq!(capabilities, ["tools"], "{init}");
    let instructions = init["instructions"].as_str().unwrap();
    assert!(instructions.len() <= 1000, "{} bytes", instructions.len());
    let listed = mcp.request("tools/list", json!({})).await;
    let tools = listed["result"]["tools"].as_array().unwrap();
    let names: Vec<&str> = tools.iter().map(|t| t["name"].as_str().unwrap()).collect();
    assert_eq!(names, TOOLS);
    for tool in tools {
        let name = &tool["name"];
        assert!(tool["title"].is_string(), "{name}");
        assert!(!tool["description"].as_str().unwrap().is_empty(), "{name}");
        assert_eq!(tool["inputSchema"]["type"], "object", "{name}");
        assert_eq!(tool["annotations"]["openWorldHint"], false, "{name}");
        assert!(tool["annotations"]["readOnlyHint"].is_boolean(), "{name}");
    }
    let readonly = |name: &str| {
        tools.iter().find(|t| t["name"] == name).unwrap()["annotations"]["readOnlyHint"] == true
    };
    assert!(readonly("get_status") && readonly("wait_for_operations"));
    assert!(!readonly("edit_files") && !readonly("snapshot_workspace"));
    let diff = tools.iter().find(|t| t["name"] == "get_diff").unwrap();
    assert_eq!(
        diff["inputSchema"]["properties"]["format"]["enum"],
        json!(["summary", "stat", "hunks", "symbols"]),
        "{diff}"
    );
    let bytes = serde_json::to_vec(&listed["result"]).unwrap().len();
    eprintln!("tools/list: {bytes} bytes");
    // 20,000 until R2.15, whose symbol view of `get_diff` (the one input
    // whose purpose is fewer tokens per answer) took it to 20,048 after
    // tightening `get_diff`'s own texts (ADR 0043). R2.16's `query_changes`
    // answers "who touched this" in one call instead of paging
    // `list_operations` and diffing each change, and took it to 21,191
    // (ADR 0044). Raising it again needs the same argument in that
    // ticket's brief.
    assert!(bytes <= 21_250, "tools/list is {bytes} bytes");
}

// 3
#[tokio::test(flavor = "multi_thread")]
async fn stdout_is_the_protocol() {
    let daemon = TestDaemon::start().await;
    let checkout = new_dir(&daemon, "plain-git");
    reef_testing::git::init_repo(&checkout, &[("README", "plain git\n")]);
    let mut mcp = Mcp::start(command_in(daemon.state_dir(), &checkout, &["-v"])).await;
    let status = mcp.ok("get_status", json!({})).await;
    assert_eq!(status["workspace_status"], Value::Null, "{status}");
    mcp.request("tools/list", json!({})).await;
    let stderr = mcp.stderr();
    assert!(stderr.contains("reef: registered"), "{stderr}");
    for line in mcp.lines.lock().unwrap().iter() {
        let message: Value = serde_json::from_str(line).unwrap_or_else(|e| panic!("{e}: {line}"));
        assert_eq!(message["jsonrpc"], "2.0", "{line}");
    }
    let (status, took) = mcp.close().await;
    assert!(status.success(), "{status}");
    assert!(took < Duration::from_secs(2), "{took:?} to exit on EOF");

    let mcp = Mcp::start(command(&daemon, &checkout)).await;
    let (status, took) = mcp.terminate().await;
    assert!(status.success(), "{status}");
    assert!(took < Duration::from_secs(2), "{took:?} to exit on SIGTERM");
}

// 4
#[tokio::test(flavor = "multi_thread")]
async fn starts_without_a_daemon() {
    let tmp = tempfile::Builder::new()
        .prefix("reef-")
        .tempdir_in(short_tmp())
        .unwrap();
    let state = tmp.path().join("state");
    let state_flag = state.display().to_string();
    let mut command = command_in(&state, tmp.path(), &["--state-dir", &state_flag]);
    command.env_remove("REEF_STATE_DIR");
    let mut mcp = Mcp::start(command).await;
    let listed = mcp.request("tools/list", json!({})).await;
    assert_eq!(listed["result"]["tools"].as_array().unwrap().len(), 21);
    let error = mcp.err("get_status", json!({})).await;
    assert_eq!(error["kind"], "unavailable", "{error}");
    assert_eq!(
        error["next"],
        json!(["reef", "--state-dir", state_flag, "daemon", "start"])
    );
    assert!(!state.join("daemon.pid").exists());
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn scope_defaults() {
    let (daemon, repo) = fixture().await;
    let ws_id = {
        let mut root = server(&daemon, &repo.root).await;
        let status = root.ok("get_status", json!({})).await;
        assert_eq!(status["workspace_status"], Value::Null, "{status}");
        id(&create(&mut root).await)
    };
    let ws = inspect(&daemon, &ws_id).await;
    let in_status = |status: &Value| status["workspace_status"]["workspace"]["id"].clone();

    let mut inside = server(&daemon, Path::new(&ws.path)).await;
    let status = inside.ok("get_status", json!({})).await;
    assert_eq!(in_status(&status), ws_id.as_str(), "{status}");
    let root_path = repo.root.display().to_string();
    let status = inside.ok("get_status", json!({"repo": root_path})).await;
    assert_eq!(status["workspace_status"], Value::Null, "{status}");

    let mut from_env = command(&daemon, &repo.root);
    from_env.env("REEF_WORKSPACE", &ws_id);
    let mut from_env = Mcp::start(from_env).await;
    let status = from_env.ok("get_status", json!({})).await;
    assert_eq!(in_status(&status), ws_id.as_str());

    let mut root = server(&daemon, &repo.root).await;
    let status = root
        .ok("get_status", json!({"workspace": &ws_id[..8]}))
        .await;
    assert_eq!(in_status(&status), ws_id.as_str());

    // Outside every checkout: the repository must be named.
    let empty = new_dir(&daemon, "empty");
    let mut outside = server(&daemon, &empty).await;
    let error = outside.err("get_status", json!({})).await;
    assert_eq!(error["reason"], "REPOSITORY_REQUIRED", "{error}");
    assert!(
        error["hint"]
            .as_str()
            .unwrap()
            .contains("list_repositories"),
        "{error}"
    );
    let status = outside.ok("get_status", json!({"repo": root_path})).await;
    assert_eq!(status["repository"]["id"], repo.id.as_str());
    // A tool that takes only a workspace asks for one, not for `repo`
    // (review finding 2).
    let error = outside.err("snapshot_workspace", json!({})).await;
    assert_eq!(error["reason"], "WORKSPACE_REQUIRED", "{error}");

    // A `repo` path is never registered implicitly (review finding 12):
    // only the launch checkout is.
    let other = new_dir(&daemon, "other-git");
    reef_testing::git::init_repo(&other, &[("README", "other\n")]);
    let error = root
        .err("get_status", json!({"repo": other.display().to_string()}))
        .await;
    assert_eq!(error["reason"], "REPOSITORY_NOT_REGISTERED", "{error}");
    assert!(!other.join(".jj").exists(), "the daemon wrote .jj there");

    // A plain git checkout with --no-register: the daemon's error.
    let checkout = new_dir(&daemon, "plain-git");
    reef_testing::git::init_repo(&checkout, &[("README", "plain git\n")]);
    let mut unregistered = Mcp::start(command_in(
        daemon.state_dir(),
        &checkout,
        &["--no-register"],
    ))
    .await;
    let error = unregistered.err("get_status", json!({})).await;
    assert_eq!(error["reason"], "REPOSITORY_NOT_REGISTERED", "{error}");
    assert_eq!(error["next"][1], "repo", "{error}");
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn workspace_lifecycle() {
    let (daemon, repo) = fixture().await;
    let mut mcp = server(&daemon, &repo.root).await;
    let created = create(&mut mcp).await;
    let ws = id(&created);
    assert!(path(&created).is_dir(), "{created}");
    assert_eq!(created["owner"], owner().as_str());
    let listed = mcp.ok("list_workspaces", json!({})).await;
    assert!(
        listed["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .any(|w| w["id"] == ws.as_str()),
        "{listed}"
    );
    write(&created, "new.txt", "new\n");
    let taken = mcp.ok("snapshot_workspace", json!({"workspace": ws})).await;
    assert_eq!(taken["changed"], true, "{taken}");
    assert_eq!(taken["operation"]["kind"], "snapshot");
    assert_eq!(taken["operation"]["snapshot_kind"], "explicit");
    assert_eq!(taken["workspace"]["id"], ws.as_str());
    let moved = mcp
        .ok(
            "move_workspace",
            json!({"workspace": ws, "new_on": "trunk()"}),
        )
        .await;
    assert_eq!(moved["id"], ws.as_str(), "{moved}");
    let released = mcp.ok("release_workspace", json!({"workspace": ws})).await;
    assert_eq!(released["id"], ws.as_str(), "{released}");
    assert!(!path(&created).exists());
    let again = mcp.err("release_workspace", json!({"workspace": ws})).await;
    assert_eq!(again["kind"], "conflict", "{again}");

    // Never the launch workspace by default.
    let other = create(&mut mcp).await;
    let mut inside = server(&daemon, &path(&other)).await;
    let error = inside.err("release_workspace", json!({})).await;
    assert_eq!(error["kind"], "invalid_argument", "{error}");
    assert!(
        error["message"].as_str().unwrap().contains("workspace"),
        "{error}"
    );
    assert!(path(&other).is_dir());
}

/// Short leases for tests 7 and 8: 3 s, a heartbeat every 500 ms, 2 s of
/// grace.
fn short_leases(config: &mut reef_daemon::config::Config) {
    let lease = &mut config.file.lease;
    lease.default = Duration::from_secs(3);
    lease.max = Duration::from_secs(10);
    lease.grace = Duration::from_secs(2);
    lease.heartbeat = Duration::from_millis(500);
    lease.tick = Duration::from_millis(500);
}

/// Waits up to `bound` for workspace `ws` to leave `active`.
async fn reclaimed_within(daemon: &TestDaemon, ws: &str, bound: Duration) -> bool {
    let deadline = Instant::now() + bound;
    while Instant::now() < deadline {
        if inspect(daemon, ws).await.state != i32::from(v1::WorkspaceState::Active) {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    false
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn leases_are_kept_alive() {
    let (daemon, repo) = fixture_with(short_leases).await;
    let mut mcp = server(&daemon, &repo.root).await;
    let ws = id(&create(&mut mcp).await);
    let first = inspect(&daemon, &ws).await.lease.unwrap().last_heartbeat_at;
    tokio::time::sleep(Duration::from_secs(8)).await;
    let alive = inspect(&daemon, &ws).await;
    assert_eq!(
        alive.state,
        i32::from(v1::WorkspaceState::Active),
        "{alive:?}"
    );
    assert_ne!(alive.lease.unwrap().last_heartbeat_at, first);
    let (status, _) = mcp.close().await;
    assert!(status.success());
    // Nothing released at exit: the lease runs out, and the reclaim takes
    // a final snapshot.
    assert_eq!(
        inspect(&daemon, &ws).await.state,
        i32::from(v1::WorkspaceState::Active)
    );
    assert!(
        reclaimed_within(&daemon, &ws, Duration::from_secs(20)).await,
        "not reclaimed after the server stopped"
    );
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn a_restarted_server_takes_over() {
    let (daemon, repo) = fixture_with(short_leases).await;
    let mut first = server(&daemon, &repo.root).await;
    let ws = id(&create(&mut first).await);
    first.close().await;
    let mut second = server(&daemon, &repo.root).await;
    second.ok("get_workspace", json!({"workspace": ws})).await;
    tokio::time::sleep(Duration::from_secs(8)).await;
    assert_eq!(
        inspect(&daemon, &ws).await.state,
        i32::from(v1::WorkspaceState::Active)
    );
}

// 9
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one fixture, every read tool")]
async fn reads() {
    let (daemon, repo) = fixture().await;
    let mut root = server(&daemon, &repo.root).await;
    let created = create(&mut root).await;
    write(&created, "src/read.txt", "one\ntwo\n");
    let mut mcp = server(&daemon, &path(&created)).await;
    let status = mcp.ok("get_status", json!({})).await;
    let changed = status["workspace_status"]["changed_files"].to_string();
    assert!(changed.contains("src/read.txt"), "{status}");

    let page = mcp.ok("list_changes", json!({"page_size": 1})).await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 1, "{page}");
    let token = page["next_page_token"].as_str().unwrap().to_owned();
    let next = mcp
        .ok("list_changes", json!({"page_size": 1, "page_token": token}))
        .await;
    assert_eq!(next["entries"].as_array().unwrap().len(), 1, "{next}");
    assert_ne!(next["entries"][0], page["entries"][0]);

    let change = mcp.ok("get_change", json!({})).await;
    assert_eq!(
        change["change"]["change_id"], created["change_id"],
        "{change}"
    );

    // R2.16: the change that touched the file, once its touches are in.
    daemon
        .handle
        .state()
        .history
        .set_throttle(Duration::ZERO, Duration::ZERO);
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        let history = mcp
            .ok("query_changes", json!({"paths": ["src/read.txt"]}))
            .await;
        assert!(history["coverage"].is_object(), "{history}");
        if history["changes"][0]["change_id"] == created["change_id"] {
            assert_eq!(
                history["changes"][0]["matched_paths"],
                json!(["src/read.txt"])
            );
            break;
        }
        assert!(Instant::now() < deadline, "{history}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    let hunks = mcp.ok("get_diff", json!({})).await;
    let file = &hunks["files"][0];
    assert!(file["hunks"].is_array(), "{hunks}");
    for format in ["stat", "summary"] {
        let diff = mcp.ok("get_diff", json!({"format": format})).await;
        assert_eq!(diff["files"].as_array().unwrap().len(), 1, "{diff}");
        assert!(diff.get("next_page_token").is_some());
    }
    let absolute = path(&created).join("src/read.txt").display().to_string();
    let only = mcp.ok("get_diff", json!({"paths": [absolute]})).await;
    assert_eq!(only["files"].as_array().unwrap().len(), 1, "{only}");
    write(&created, "src/more.txt", "more\n");
    let paged = mcp.ok("get_diff", json!({"page_size": 1})).await;
    assert_eq!(paged["files"].as_array().unwrap().len(), 1, "{paged}");
    assert!(paged["next_page_token"].is_string(), "{paged}");

    // The symbol view, and one symbol's lines (R2.15).
    write(
        &created,
        "src/p.py",
        "def parse():\n    return 1\n\n\ndef other():\n    return 2\n",
    );
    let symbols = mcp
        .ok(
            "get_diff",
            json!({"format": "symbols", "paths": ["src/p.py"]}),
        )
        .await;
    let symbol_changes = &symbols["files"][0]["symbols"]["changes"];
    let names: Vec<&str> = symbol_changes
        .as_array()
        .unwrap()
        .iter()
        .map(|c| c["name"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["parse", "other"], "{symbols}");
    assert_eq!(symbol_changes[0]["kind"], "added", "{symbols}");
    assert_eq!(symbols["symbol_summary"]["added"], 2, "{symbols}");
    let expanded = mcp.ok("get_diff", json!({"expand": ["parse"]})).await;
    let files = expanded["files"].as_array().unwrap();
    assert_eq!(files.len(), 1, "{expanded}");
    assert_eq!(
        files[0]["hunks"][0]["lines"], "+def parse():\n+    return 1\n",
        "{expanded}"
    );

    let snapshots = mcp
        .ok(
            "list_operations",
            json!({"kinds": ["snapshot"], "page_size": 5}),
        )
        .await;
    let operations = snapshots["operations"].as_array().unwrap();
    assert!(!operations.is_empty(), "{snapshots}");
    assert!(operations.iter().all(|op| op["kind"] == "snapshot"));
    let nobody = mcp
        .ok("list_operations", json!({"agents": ["nobody-at-all"]}))
        .await;
    assert_eq!(nobody["operations"], json!([]));
    let op = operations[0]["id"].as_str().unwrap();
    let shown = mcp
        .ok("get_operation", json!({"operation_id": &op[..12]}))
        .await;
    assert_eq!(shown["id"], op);
    assert!(shown["effects"].is_object(), "{shown}");
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn writes_return_the_workspace() {
    let (daemon, repo) = fixture().await;
    let mut root = server(&daemon, &repo.root).await;
    let created = create(&mut root).await;
    let ws = id(&created);
    let mut mcp = server(&daemon, &path(&created)).await;
    let described = mcp
        .ok("describe_change", json!({"description": "fix parser\n"}))
        .await;
    assert_eq!(described["change"]["description"], "fix parser\n");
    assert_eq!(described["operation"]["kind"], "describe");
    assert_eq!(described["workspace"]["id"], ws.as_str());
    let same = mcp
        .ok("describe_change", json!({"description": "fix parser\n"}))
        .await;
    assert_eq!(same["operation"], Value::Null, "{same}");

    let child = mcp
        .ok(
            "edit_files",
            json!({"new_on": "@", "edits": [{"path": "child.txt", "content": "c\n"}]}),
        )
        .await;
    let child_change = child["change"]["change_id"].as_str().unwrap();
    let abandoned = mcp
        .ok("abandon_change", json!({"revision": child_change}))
        .await;
    assert!(abandoned["abandoned_commit_id"].is_string(), "{abandoned}");
    assert_eq!(abandoned["operation"]["kind"], "abandon");

    let undo = described["operation"]["id"].as_str().unwrap();
    let undone = mcp
        .ok("undo_operation", json!({"operation_id": undo}))
        .await;
    assert_eq!(undone["undone_operation_id"], undo, "{undone}");
    assert_eq!(undone["workspace"]["id"], ws.as_str());
    let change = mcp.ok("get_change", json!({})).await;
    assert_eq!(change["change"]["description"], "", "{change}");

    let error = mcp
        .err(
            "describe_change",
            json!({"revision": "root()", "description": "x"}),
        )
        .await;
    assert_eq!(error["reason"], "IMMUTABLE_REVISION", "{error}");
    assert!(
        error["hint"].as_str().unwrap().contains("immutable"),
        "{error}"
    );
    assert!(error["metadata"]["commit_id"].is_string(), "{error}");
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn files() {
    let (daemon, repo) = fixture().await;
    let mut root = server(&daemon, &repo.root).await;
    let created = create(&mut root).await;
    write(&created, "notes/a.txt", "hello\n");
    write(&created, "notes/b.bin", [0_u8, 159, 146, 150]);
    let big: String = std::iter::once('a')
        .chain(std::iter::repeat_n('é', 100_000))
        .collect();
    write(&created, "notes/big.txt", &big);
    let mut mcp = server(&daemon, &path(&created)).await;
    let text = mcp.ok("read_file", json!({"path": "notes/a.txt"})).await;
    assert_eq!(text["content"], "hello\n", "{text}");
    assert_eq!(text["next_offset"], Value::Null);
    let binary = mcp.ok("read_file", json!({"path": "notes/b.bin"})).await;
    assert_eq!(binary["content"], Value::Null, "{binary}");
    assert_eq!(binary["content_base64"], "AJ+Slg==");

    // 200 KiB in ranges of 64 KiB, each cut before a split character.
    let mut whole = String::new();
    let mut offset = 0;
    let mut ranges = 0;
    loop {
        let range = mcp
            .ok(
                "read_file",
                json!({"path": "notes/big.txt", "offset": offset}),
            )
            .await;
        assert_eq!(range["offset"], offset);
        let content = range["content"]
            .as_str()
            .unwrap_or_else(|| panic!("{range}"));
        whole.push_str(content);
        ranges += 1;
        match range["next_offset"].as_u64() {
            Some(next) => {
                assert!(next - offset <= 65_536);
                offset = next;
            }
            None => break,
        }
    }
    assert_eq!(whole, big);
    assert_eq!(ranges, 4);

    let listed = mcp.ok("list_files", json!({"path": "notes"})).await;
    let names: Vec<&str> = listed["entries"]
        .as_array()
        .unwrap()
        .iter()
        .map(|entry| entry["path"].as_str().unwrap())
        .collect();
    assert_eq!(names, ["notes/a.txt", "notes/b.bin", "notes/big.txt"]);
    let page = mcp
        .ok("list_files", json!({"recursive": true, "page_size": 2}))
        .await;
    assert_eq!(page["entries"].as_array().unwrap().len(), 2, "{page}");
    assert!(page["next_page_token"].is_string());
    let error = mcp.err("read_file", json!({"path": "notes"})).await;
    assert_eq!(error["reason"], "NOT_A_FILE", "{error}");
    assert!(error["metadata"].is_object(), "{error}");
}

/// The head of the operation log.
async fn head(mcp: &mut Mcp) -> Value {
    mcp.ok("list_operations", json!({"page_size": 1})).await["operations"][0]["id"].clone()
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn edit_files() {
    let (daemon, repo) = fixture().await;
    let mut root = server(&daemon, &repo.root).await;
    let created = create(&mut root).await;
    write(&created, "edit/old.txt", "keep this line\nfix me\n");
    write(&created, "edit/gone.txt", "bye\n");
    write(&created, "edit/run.sh", "#!/bin/sh\n");
    let dir = path(&created);
    let mut mcp = server(&daemon, &dir).await;
    let edited = mcp
        .ok(
            "edit_files",
            json!({"revision": "@", "edits": [
                {"path": "edit/new.txt", "content": "new\n"},
                {"path": "edit/old.txt", "replace": {"old_text": "fix me", "new_text": "fixed"}},
                {"path": "edit/gone.txt", "delete": true},
                {"path": "edit/moved.txt", "copy_from": dir.join("edit/new.txt").display().to_string()},
                {"path": "edit/new.txt", "delete": true},
                {"path": "edit/run.sh", "executable": true},
            ]}),
        )
        .await;
    assert_eq!(edited["operation"]["kind"], "edit", "{edited}");
    assert_eq!(edited["workspace"]["id"], created["id"]);
    let read = |file: &str| std::fs::read_to_string(dir.join(file)).ok();
    assert_eq!(read("edit/moved.txt").as_deref(), Some("new\n"));
    assert_eq!(read("edit/new.txt"), None);
    assert_eq!(read("edit/gone.txt"), None);
    assert_eq!(
        read("edit/old.txt").as_deref(),
        Some("keep this line\nfixed\n")
    );
    let mode = std::os::unix::fs::PermissionsExt::mode(
        &std::fs::metadata(dir.join("edit/run.sh"))
            .unwrap()
            .permissions(),
    );
    assert_ne!(mode & 0o100, 0, "{mode:o}");

    let on_trunk = mcp
        .ok(
            "edit_files",
            json!({"new_on": "trunk()", "edits": [
                {"path": "edit/bytes.bin", "content_base64": "AP8="}
            ], "description": "bytes\n"}),
        )
        .await;
    assert_ne!(on_trunk["change"]["change_id"], created["change_id"]);
    let change = on_trunk["change"]["change_id"].as_str().unwrap();
    let bytes = mcp
        .ok(
            "read_file",
            json!({"path": "edit/bytes.bin", "revision": change}),
        )
        .await;
    assert_eq!(bytes["content_base64"], "AP8=", "{bytes}");

    let before = head(&mut mcp).await;
    let error = mcp
        .err(
            "edit_files",
            json!({"revision": "@", "edits": [
                {"path": "a", "content": "x"},
                {"path": "b", "content": "x", "delete": true},
            ]}),
        )
        .await;
    assert_eq!(error["reason"], "INVALID_EDIT", "{error}");
    assert_eq!(error["metadata"]["edit_index"], "1");
    assert_eq!(
        head(&mut mcp).await,
        before,
        "a refused edit recorded nothing"
    );
    let error = mcp
        .err(
            "edit_files",
            json!({"revision": "@", "edits": [
                {"path": "edit/old.txt", "replace": {"old_text": "absent", "new_text": "x"}}
            ]}),
        )
        .await;
    assert_eq!(error["reason"], "TEXT_NOT_FOUND", "{error}");
    assert_eq!(error["metadata"]["edit_index"], "0");
    let huge = "x".repeat(5 << 20);
    let error = mcp
        .err(
            "edit_files",
            json!({"revision": "@", "edits": [{"path": "huge.txt", "content": huge}]}),
        )
        .await;
    assert_eq!(error["kind"], "invalid_argument", "{error}");
    assert!(
        error["hint"].as_str().unwrap().contains("Edit large files"),
        "{error}"
    );
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn errors_are_tool_results() {
    let (daemon, repo) = fixture().await;
    let mut mcp = server(&daemon, &repo.root).await;
    let response = mcp
        .request(
            "tools/call",
            json!({"name": "get_change", "arguments": {"revision": "nope_xyz"}}),
        )
        .await;
    let (is_error, value) = result_of(&response);
    assert!(is_error, "{response}");
    let error = &value["error"];
    assert_eq!(error["kind"], "not_found", "{error}");
    assert_eq!(error["reason"], "REVISION_NOT_FOUND");
    assert_eq!(error["metadata"]["revision"], "nope_xyz");
    let error = mcp.err("get_change", json!({"revision": "@"})).await;
    assert_eq!(error["reason"], "WORKSPACE_REQUIRED", "{error}");
    let error = mcp.err("get_status", json!({"max_files": "x"})).await;
    assert_eq!(error["kind"], "invalid_argument", "{error}");
    assert_eq!(error["hint"], "See the tool's input schema.");
    let error = mcp.err("get_status", json!({"bogus": 1})).await;
    assert!(
        error["message"].as_str().unwrap().contains("bogus"),
        "{error}"
    );
    let unknown = mcp
        .request(
            "tools/call",
            json!({"name": "no_such_tool", "arguments": {}}),
        )
        .await;
    assert_eq!(unknown["error"]["code"], -32602, "{unknown}");
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn agent_names() {
    let (daemon, repo) = fixture().await;
    let mut root = server(&daemon, &repo.root).await;
    let created = create(&mut root).await;
    let dir = path(&created);
    let mut flagged = Mcp::start(command_in(daemon.state_dir(), &dir, &["--agent", "bot-7"])).await;
    let described = flagged
        .ok("describe_change", json!({"description": "by bot 7\n"}))
        .await;
    assert_eq!(described["operation"]["agent"], "bot-7", "{described}");
    let mut from_env = command(&daemon, &dir);
    from_env.env("REEF_AGENT", "bot-8");
    let mut from_env = Mcp::start(from_env).await;
    write(&created, "agent.txt", "8\n");
    let taken = from_env.ok("snapshot_workspace", json!({})).await;
    assert_eq!(taken["operation"]["agent"], "bot-8", "{taken}");

    let output = command_in(daemon.state_dir(), &dir, &["--agent", "bad\tname"])
        .stdin(std::process::Stdio::null())
        .output()
        .await
        .unwrap();
    assert_eq!(output.status.code(), Some(2), "{output:?}");
    assert!(output.stdout.is_empty(), "{output:?}");
    assert!(String::from_utf8_lossy(&output.stderr).contains("agent"));
}

// 15
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one test per brief test")]
async fn wait_for_operations() {
    let (daemon, repo) = fixture().await;
    let mut root = server(&daemon, &repo.root).await;
    let first = create(&mut root).await;
    let second = create(&mut root).await;
    let mut waiter = server(&daemon, &repo.root).await;
    let mut writer = server(&daemon, &path(&first)).await;

    let started = Instant::now();
    let wait = waiter
        .send(
            "tools/call",
            json!({"name": "wait_for_operations", "arguments": {"kinds": ["describe"], "timeout_seconds": 10}}),
        )
        .await;
    tokio::time::sleep(Duration::from_secs(1)).await;
    let described = writer
        .ok("describe_change", json!({"description": "one\n"}))
        .await;
    let (_, answer) = result_of(&waiter.response(wait).await);
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "{:?}",
        started.elapsed()
    );
    assert_eq!(answer["timed_out"], false, "{answer}");
    assert_eq!(answer["operations"][0]["id"], described["operation"]["id"]);
    let last = answer["last_operation_id"].as_str().unwrap().to_owned();

    let again = waiter
        .ok(
            "wait_for_operations",
            json!({"kinds": ["describe"], "after_operation": last, "timeout_seconds": 1}),
        )
        .await;
    assert_eq!(again["timed_out"], true, "{again}");
    assert_eq!(again["operations"], json!([]));
    assert!(again["last_operation_id"].is_string());

    // R2.09 test 20: a timed-out wait's last_operation_id resumes exactly
    // where it stood, so a describe recorded between the calls is seen.
    let between = writer
        .ok("describe_change", json!({"description": "between\n"}))
        .await;
    let resumed = waiter
        .ok(
            "wait_for_operations",
            json!({"kinds": ["describe"], "after_operation": again["last_operation_id"], "timeout_seconds": 5}),
        )
        .await;
    assert_eq!(resumed["timed_out"], false, "{resumed}");
    assert_eq!(resumed["operations"][0]["id"], between["operation"]["id"]);
    let last = resumed["last_operation_id"].as_str().unwrap().to_owned();

    // Resuming from an older operation returns what happened since, at once.
    let two = writer
        .ok("describe_change", json!({"description": "two\n"}))
        .await;
    let three = writer
        .ok("describe_change", json!({"description": "three\n"}))
        .await;
    let started = Instant::now();
    let resumed = waiter
        .ok(
            "wait_for_operations",
            json!({"kinds": ["describe"], "after_operation": last, "timeout_seconds": 10}),
        )
        .await;
    assert!(started.elapsed() < Duration::from_secs(2));
    let ids: Vec<&Value> = resumed["operations"]
        .as_array()
        .unwrap()
        .iter()
        .map(|op| &op["id"])
        .collect();
    assert_eq!(
        ids,
        [&two["operation"]["id"], &three["operation"]["id"]],
        "{resumed}"
    );
    // A timeout of 0 checks once: it returns what is already there.
    let checked = waiter
        .ok(
            "wait_for_operations",
            json!({"kinds": ["describe"], "after_operation": last, "timeout_seconds": 0}),
        )
        .await;
    assert_eq!(checked["timed_out"], false, "{checked}");
    assert_eq!(checked["operations"], resumed["operations"]);

    let started = Instant::now();
    let once = waiter
        .ok("wait_for_operations", json!({"timeout_seconds": 0}))
        .await;
    assert_eq!(once["timed_out"], true);
    assert!(once["last_operation_id"].is_string(), "{once}");
    assert!(started.elapsed() < Duration::from_secs(2));

    // Another workspace's snapshot does not end a wait on this one.
    let wait = waiter
        .send(
            "tools/call",
            json!({"name": "wait_for_operations", "arguments": {"workspaces": [id(&first)], "timeout_seconds": 3}}),
        )
        .await;
    write(&second, "other.txt", "x\n");
    root.ok("snapshot_workspace", json!({"workspace": id(&second)}))
        .await;
    let (_, filtered) = result_of(&waiter.response(wait).await);
    assert_eq!(filtered["timed_out"], true, "{filtered}");

    // Progress while waiting, with a token.
    let wait = waiter
        .send(
            "tools/call",
            json!({"name": "wait_for_operations", "arguments": {"timeout_seconds": 12}, "_meta": {"progressToken": "p1"}}),
        )
        .await;
    waiter.response(wait).await;
    assert!(
        waiter.notifications.iter().any(
            |n| n["method"] == "notifications/progress" && n["params"]["progressToken"] == "p1"
        ),
        "{:?}",
        waiter.notifications
    );

    // A cancelled wait ends: the server answers the next call at once.
    let wait = waiter
        .send(
            "tools/call",
            json!({"name": "wait_for_operations", "arguments": {"timeout_seconds": 60}}),
        )
        .await;
    tokio::time::sleep(Duration::from_millis(200)).await;
    waiter
        .notify(
            "notifications/cancelled",
            json!({"requestId": wait, "reason": "test"}),
        )
        .await;
    let started = Instant::now();
    waiter.ok("list_repositories", json!({})).await;
    assert!(started.elapsed() < Duration::from_secs(2));
    // And it ended: a wait still running would hold the exit up to rmcp's
    // 5 s drain (review finding 10).
    let (status, took) = waiter.close().await;
    assert!(status.success());
    assert!(
        took < Duration::from_secs(2),
        "{took:?} to exit after a cancel"
    );
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn concurrent_calls() {
    let (daemon, repo) = fixture().await;
    let mut mcp = server(&daemon, &repo.root).await;
    let wait = mcp
        .send(
            "tools/call",
            json!({"name": "wait_for_operations", "arguments": {"timeout_seconds": 5}}),
        )
        .await;
    let started = Instant::now();
    let mut ids = Vec::new();
    for _ in 0..10 {
        ids.push(
            mcp.send(
                "tools/call",
                json!({"name": "get_status", "arguments": {"skip_snapshot": true}}),
            )
            .await,
        );
    }
    for id in ids {
        let (is_error, value) = result_of(&mcp.response(id).await);
        assert!(!is_error, "{value}");
    }
    assert!(
        started.elapsed() < Duration::from_secs(4),
        "the wait blocked the others: {:?}",
        started.elapsed()
    );
    let (_, waited) = result_of(&mcp.response(wait).await);
    assert_eq!(waited["timed_out"], true);
}

/// The live `activity` entries, through `reef presence watch`.
async fn activities(daemon: &TestDaemon, dir: &Path) -> Vec<Value> {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    command.env_remove("REEF_AGENT").env_remove("REEF_SOCKET");
    let output = command
        .env("REEF_STATE_DIR", daemon.state_dir())
        .current_dir(dir)
        .args([
            "--no-autostart",
            "presence",
            "watch",
            "--kind",
            "activity",
            "--for",
            "0",
            "-o",
            "json",
        ])
        .output()
        .await
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout)
        .unwrap()
        .lines()
        .map(|line| serde_json::from_str::<Value>(line).unwrap()["presence"].clone())
        .collect()
}

/// The live `activity` entry for workspace `ws` by `agent`, once it says
/// `activity`.
async fn activity_of(
    daemon: &TestDaemon,
    dir: &Path,
    ws: &str,
    agent: &str,
    activity: &str,
) -> Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let found: Vec<Value> = activities(daemon, dir)
            .await
            .into_iter()
            .filter(|entry| entry["workspace_id"] == ws && entry["agent"] == agent)
            .collect();
        assert!(found.len() <= 1, "one entry per subject: {found:?}");
        if let Some(entry) = found.first()
            && entry["activity"] == activity
        {
            return entry.clone();
        }
        assert!(Instant::now() < deadline, "no {activity} entry: {found:?}");
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

// R2.11 15: each call is reported as its subject's activity, under the
// agent's name or the workspace's owner, and cleared when the server
// exits.
#[tokio::test(flavor = "multi_thread")]
async fn mcp_reports_its_calls() {
    let (daemon, repo) = fixture().await;
    let mut mcp = server(&daemon, &repo.root).await;
    let ws = create(&mut mcp).await;
    let ws_id = id(&ws);
    mcp.ok("get_status", json!({"workspace": ws_id})).await;
    let first = activity_of(&daemon, &repo.root, &ws_id, &owner(), "get_status").await;
    assert_eq!(first["by_daemon"], false);
    assert_eq!(first["change_id"], ws["change_id"]);
    assert_eq!(
        millis(&first["expires_at"]) - millis(&first["updated_at"]),
        60_000
    );
    mcp.ok("list_files", json!({"workspace": ws_id})).await;
    let second = activity_of(&daemon, &repo.root, &ws_id, &owner(), "list_files").await;
    assert_eq!(second["id"], first["id"], "replaced, not added");

    // Repository scope has an entry of its own; list_repositories names
    // no scope.
    let in_repo = activities(&daemon, &repo.root)
        .await
        .into_iter()
        .filter(|entry| entry["workspace_id"].is_null())
        .count();
    assert_eq!(in_repo, 1, "create_workspace's");
    let before = activities(&daemon, &repo.root).await.len();
    mcp.ok("list_repositories", json!({})).await;
    tokio::time::sleep(Duration::from_millis(1_200)).await;
    assert_eq!(activities(&daemon, &repo.root).await.len(), before);

    let mut named = command(&daemon, &repo.root);
    named.env("REEF_AGENT", "bot-2");
    let mut named = Mcp::start(named).await;
    named.ok("get_status", json!({"workspace": ws_id})).await;
    activity_of(&daemon, &repo.root, &ws_id, "bot-2", "get_status").await;

    let (status, took) = mcp.close().await;
    assert!(status.success(), "{status:?}");
    assert!(took < Duration::from_secs(3), "{took:?}");
    let left = activities(&daemon, &repo.root).await;
    assert!(
        left.iter().all(|entry| entry["agent"] == "bot-2"),
        "the closed server's entries are cleared: {left:?}"
    );
    assert_eq!(left.len(), 1, "{left:?}");
}

/// An RFC 3339 time from the JSON as Unix milliseconds.
fn millis(value: &Value) -> u128 {
    let text = value
        .as_str()
        .unwrap_or_else(|| panic!("not a time: {value}"));
    humantime::parse_rfc3339_weak(text.trim_end_matches('Z'))
        .unwrap_or_else(|e| panic!("{e}: {text}"))
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis()
}
