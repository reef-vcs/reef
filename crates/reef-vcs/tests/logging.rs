//! The `reef` binary initializes logging the same way as the daemon (R0.09).

#![allow(
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::process::Command;

use serde_json::Value;

mod common;

fn reef_config_show(vars: &[(&str, &str)]) -> std::process::Output {
    // `reef config show` runs `reef-daemon config show` (R2.05).
    common::daemon_bin();
    let tmp = tempfile::Builder::new().prefix("reef-").tempdir().unwrap();
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef"));
    command
        .args(["config", "show"])
        .env("REEF_STATE_DIR", tmp.path())
        .env_remove("REEF_CONFIG")
        .env_remove("REEF_SOCKET")
        .env_remove("REEF_LOG")
        .env_remove("RUST_LOG")
        .env_remove("REEF_AGENT")
        .env("REEF_LOG_FORMAT", "json");
    command.envs(vars.iter().copied());
    command.output().unwrap()
}

/// `starting` is a debug event (R1.04 made it one, so commands print
/// nothing on stderr at the default filter, `warn` for the CLI), written as
/// JSON to stderr.
#[test]
fn logs_starting_as_json_to_stderr() {
    let quiet = reef_config_show(&[]);
    assert!(quiet.status.success());
    assert_eq!(
        String::from_utf8_lossy(&quiet.stderr),
        "",
        "nothing at the default filter"
    );

    let output = reef_config_show(&[("REEF_LOG", "reef=debug")]);
    assert!(output.status.success());
    assert!(
        !String::from_utf8(output.stdout)
            .unwrap()
            .contains("\"message\""),
        "logs go to stderr only"
    );
    let text = String::from_utf8(output.stderr).unwrap();
    let events: Vec<Value> = text
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect();
    let event = events
        .iter()
        .find(|event| event["message"] == "starting")
        .unwrap_or_else(|| panic!("{text}"));
    assert_eq!(event["target"], "reef");
    assert_eq!(event["version"], env!("CARGO_PKG_VERSION"));
}

#[test]
fn invalid_log_options_exit_2() {
    let output = reef_config_show(&[("REEF_LOG_FORMAT", "yaml")]);
    assert_eq!(output.status.code(), Some(2));
    let text = String::from_utf8(output.stderr).unwrap();
    assert!(text.contains("initialize logging"), "{text}");
}
