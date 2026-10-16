//! `reef config show` (R1.04): the daemon's configuration, resolved without
//! the daemon.

#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::process::{Command, Output};

use reef_daemon::config::FileConfig;
use serde_json::Value;
use tempfile::TempDir;

fn state_dir() -> TempDir {
    tempfile::Builder::new().prefix("reef-").tempdir().unwrap()
}

fn reef(state: &Path, args: &[&str], vars: &[(&str, &Path)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    command
        .args(args)
        .env("REEF_STATE_DIR", state)
        .env_remove("REEF_SOCKET")
        .env_remove("REEF_CONFIG")
        .env_remove("REEF_LOG")
        .env_remove("RUST_LOG");
    for (name, value) in vars {
        command.env(name, value);
    }
    command.output().unwrap()
}

fn stdout(output: &Output) -> String {
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout.clone()).unwrap()
}

/// Test 8, and the empty-state-dir criterion: every key is printed with
/// `# source: default`, and the text loads back as a `config.toml`.
#[test]
fn show_prints_loadable_toml_with_sources() {
    let tmp = state_dir();
    let text = stdout(&reef(tmp.path(), &["config", "show"], &[]));
    let config: FileConfig = toml::from_str(&text).unwrap_or_else(|e| panic!("{e}\n{text}"));
    assert_eq!(config.lease.default.as_secs(), 30 * 60);
    let keys: Vec<_> = text.lines().filter(|line| line.contains(" = ")).collect();
    let expected: usize = reef_daemon::config::KEYS
        .iter()
        .map(|(_, keys)| keys.len())
        .sum();
    assert_eq!(keys.len(), expected, "{text}");
    assert!(
        keys.iter().all(|line| line.ends_with("# source: default")),
        "{text}"
    );
    assert!(
        std::fs::read_dir(tmp.path()).unwrap().next().is_none(),
        "show creates nothing"
    );
}

#[test]
fn show_reports_file_env_and_flag_layers() {
    let tmp = state_dir();
    std::fs::write(
        tmp.path().join("config.toml"),
        "[lease]\ndefault = \"2h\"\n[daemon]\nsocket = \"/tmp/file.sock\"\n",
    )
    .unwrap();
    let text = stdout(&reef(
        tmp.path(),
        &["config", "show"],
        &[("REEF_SOCKET", Path::new("/tmp/env.sock"))],
    ));
    assert!(text.contains("default = \"2h\"  # source: file"), "{text}");
    assert!(
        text.contains("socket = \"/tmp/env.sock\"  # source: env"),
        "{text}"
    );
    let text = stdout(&reef(
        tmp.path(),
        &["--socket", "/tmp/flag.sock", "config", "show"],
        &[],
    ));
    assert!(
        text.contains("socket = \"/tmp/flag.sock\"  # source: flag"),
        "{text}"
    );
}

/// Test 8, JSON half.
#[test]
fn show_json_has_sources() {
    let tmp = state_dir();
    std::fs::write(tmp.path().join("config.toml"), "[gc]\nreindex_limit = 5\n").unwrap();
    let text = stdout(&reef(tmp.path(), &["config", "show", "-o", "json"], &[]));
    let mut value: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(value["gc"]["reindex_limit"], 5);
    assert_eq!(value["lease"]["default"], "30m");
    assert_eq!(value["sources"]["gc.reindex_limit"], "file");
    assert_eq!(value["sources"]["lease.default"], "default");
    value.as_object_mut().unwrap().remove("sources");
    let file: FileConfig = serde_json::from_value(value).unwrap_or_else(|e| panic!("{e}"));
    assert_eq!(file.gc.reindex_limit, 5);
}

/// `REEF_CONFIG` naming a file with an unknown key: exit 2, naming the key
/// and the file. `reef-daemon`'s half is in its own tests.
#[test]
fn unknown_key_exits_2_naming_key_and_file() {
    let tmp = state_dir();
    let bad = tmp.path().join("bad.toml");
    std::fs::write(
        &bad,
        "[lease]\ndefault = \"1h\"\n[leas]\ndefault = \"2h\"\n",
    )
    .unwrap();
    let output = reef(tmp.path(), &["config", "show"], &[("REEF_CONFIG", &bad)]);
    assert_eq!(output.status.code(), Some(2));
    assert_eq!(String::from_utf8_lossy(&output.stdout), "");
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.starts_with("error: "), "{text}");
    assert!(text.contains("`leas.default`"), "{text}");
    assert!(text.contains(&bad.display().to_string()), "{text}");
    assert!(text.contains("line 3"), "toml's span: {text}");
}

#[test]
fn invalid_value_exits_2() {
    let tmp = state_dir();
    std::fs::write(
        tmp.path().join("config.toml"),
        "[lease]\ndefault = \"2d\"\nmax = \"24h\"\n",
    )
    .unwrap();
    let output = reef(tmp.path(), &["config", "show"], &[]);
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.contains("invalid lease.default"), "{text}");
}
