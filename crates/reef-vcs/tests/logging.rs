//! The `reef` binary initializes logging the same way as the daemon (R0.09).

use std::process::Command;

use serde_json::Value;

#[test]
fn logs_starting_as_json_to_stderr() {
    let output = Command::new(env!("CARGO_BIN_EXE_reef"))
        .env_remove("REEF_LOG")
        .env_remove("RUST_LOG")
        .env("REEF_LOG_FORMAT", "json")
        .output()
        .unwrap();
    assert!(output.status.success());
    assert!(output.stdout.is_empty(), "logs go to stderr only");
    let text = String::from_utf8(output.stderr).unwrap();
    let event: Value = serde_json::from_str(text.lines().next().unwrap()).unwrap();
    assert_eq!(event["message"], "starting");
    assert_eq!(event["target"], "reef");
    assert_eq!(event["version"], env!("CARGO_PKG_VERSION"));
}
