//! `reef_daemon::logging` as the `reef-daemon` binary uses it (R0.09). Each
//! test runs the binary on a fresh state directory with a controlled
//! environment until its socket is bound, stops it with SIGTERM, and reads
//! its stderr.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use std::process::Output;

use common::run_binary;
use serde_json::Value;

fn run(vars: &[(&str, &str)]) -> Output {
    let (output, _state) = run_binary(vars);
    assert!(output.stdout.is_empty(), "logs go to stderr only");
    output
}

fn stderr(output: &Output) -> String {
    String::from_utf8(output.stderr.clone()).unwrap()
}

/// Every stderr line as JSON, and the `daemon started` event.
fn json_events(output: &Output) -> (Vec<Value>, Value) {
    assert!(output.status.success(), "{}", stderr(output));
    let events: Vec<Value> = stderr(output)
        .lines()
        .map(|line| {
            serde_json::from_str(line).unwrap_or_else(|e| panic!("{line:?} is not JSON: {e}"))
        })
        .collect();
    let started = events
        .iter()
        .find(|event| event["message"] == "daemon started")
        .cloned()
        .expect("a start event");
    (events, started)
}

fn assert_started_event(event: &Value) {
    assert_eq!(event["level"], "INFO");
    assert_eq!(event["version"], env!("CARGO_PKG_VERSION"));
    assert_eq!(event["target"], "reef_daemon::daemon");
    let timestamp = event["timestamp"].as_str().expect("timestamp string");
    assert!(
        timestamp.ends_with('Z') && timestamp.contains('T'),
        "RFC 3339 UTC: {timestamp}"
    );
}

#[test]
fn json_format() {
    assert_started_event(&json_events(&run(&[("REEF_LOG_FORMAT", "json")])).1);
}

#[test]
fn auto_format_is_json_when_stderr_is_a_pipe() {
    assert_started_event(&json_events(&run(&[])).1);
}

#[test]
fn pretty_format() {
    let output = run(&[("REEF_LOG_FORMAT", "pretty")]);
    assert!(output.status.success());
    let text = stderr(&output);
    let line = text
        .lines()
        .find(|line| line.contains("daemon started"))
        .expect("a start line");
    assert!(serde_json::from_str::<Value>(line).is_err(), "{line}");
    assert!(line.contains("INFO"), "{line}");
    assert!(
        !text.contains('\x1b'),
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
