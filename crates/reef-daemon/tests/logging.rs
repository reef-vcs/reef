//! `reef_daemon::logging` as the `reef-daemon` binary uses it (R0.09). Each
//! test runs the binary with a controlled environment and reads its stderr.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::process::{Command, Output};

use serde_json::Value;

fn run(vars: &[(&str, &str)]) -> Output {
    let mut command = Command::new(env!("CARGO_BIN_EXE_reef-daemon"));
    for var in ["REEF_LOG", "RUST_LOG", "REEF_LOG_FORMAT", "NO_COLOR"] {
        command.env_remove(var);
    }
    command.envs(vars.iter().copied());
    let output = command.output().unwrap();
    assert!(output.stdout.is_empty(), "logs go to stderr only");
    output
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

fn first_json_line(output: &Output) -> Value {
    assert!(output.status.success(), "{}", stderr(output));
    let text = stderr(output);
    let line = text.lines().next().expect("a log line");
    serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?} is not JSON: {e}"))
}

fn assert_starting_event(event: &Value) {
    assert_eq!(event["level"], "INFO");
    assert_eq!(event["message"], "starting");
    assert_eq!(event["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(event["target"], "reef_daemon");
    let timestamp = event["timestamp"].as_str().expect("timestamp string");
    assert!(
        timestamp.ends_with('Z') && timestamp.contains('T'),
        "RFC 3339 UTC: {timestamp}"
    );
}

#[test]
fn json_format() {
    assert_starting_event(&first_json_line(&run(&[("REEF_LOG_FORMAT", "json")])));
}

#[test]
fn auto_format_is_json_when_stderr_is_a_pipe() {
    assert_starting_event(&first_json_line(&run(&[])));
}

#[test]
fn pretty_format() {
    let output = run(&[("REEF_LOG_FORMAT", "pretty")]);
    assert!(output.status.success());
    let text = stderr(&output);
    let line = text.lines().next().expect("a log line");
    assert!(serde_json::from_str::<Value>(line).is_err(), "{line}");
    assert!(line.contains("INFO") && line.contains("starting"), "{line}");
    assert!(
        !line.contains('\x1b'),
        "no colour when stderr is not a terminal"
    );
}

#[test]
fn filter_comes_from_reef_log_then_rust_log() {
    let debug_logged = |vars: &[(&str, &str)]| {
        let output = run(vars);
        assert!(output.status.success(), "{}", stderr(&output));
        stderr(&output).contains("logging initialized")
    };
    assert!(!debug_logged(&[]), "the default filter is info");
    assert!(debug_logged(&[("REEF_LOG", "debug")]));
    assert!(debug_logged(&[("RUST_LOG", "debug")]));
    assert!(!debug_logged(&[
        ("REEF_LOG", "info"),
        ("RUST_LOG", "debug")
    ]));
}

#[test]
fn invalid_options_fail_with_the_chain() {
    for (var, value, expected) in [
        ("REEF_LOG", "foo=bogus[", "invalid log filter"),
        ("REEF_LOG_FORMAT", "yaml", "invalid log format \"yaml\""),
    ] {
        let output = run(&[(var, value)]);
        assert_eq!(output.status.code(), Some(1));
        let text = stderr(&output);
        assert!(text.contains("initialize logging"), "{text}");
        assert!(text.contains(expected), "{text}");
    }
}
