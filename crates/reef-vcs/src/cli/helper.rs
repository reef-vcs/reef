//! Running a `reef-daemon` offline command (`config show`, `restore`) and
//! reading its one document (R2.05, ADR 0033): the daemon's library stays
//! out of `reef`, so what needs it without a running daemon runs in the
//! daemon's binary.
//!
//! The command writes its result on stdout and exits 0, or writes the
//! error object `reef` prints for any error and exits with its kind's
//! code. Stderr is passed through.

use std::ffi::OsString;
use std::process::{Command, Stdio};

use reef_client::error::kind_from_text;
use reef_client::json::ErrorDocument;
use reef_error::Advice;

use super::autostart::{daemon_binary, path_flags};
use super::{CliError, Global};

/// Runs `reef-daemon <path flags> <args>` and returns its stdout, or its
/// error as the command's own.
pub(crate) fn run(global: &Global, args: &[OsString]) -> Result<Vec<u8>, CliError> {
    let program = daemon_binary()?;
    let mut command = Command::new(&program);
    path_flags(&mut command, global)?;
    // `-v` asks for the library's debug events, as when it ran in-process.
    match global.verbose {
        0 => {}
        1 => {
            command.env("REEF_LOG", "debug");
        }
        _ => {
            command.env("REEF_LOG", "trace");
        }
    }
    let output = command
        .args(args)
        .stdin(Stdio::null())
        .stderr(Stdio::inherit())
        .output()
        .map_err(|source| CliError::Io {
            op: "run",
            path: program.clone(),
            source,
        })?;
    if output.status.success() {
        return Ok(output.stdout);
    }
    let Ok(ErrorDocument { error }) = serde_json::from_slice(&output.stdout) else {
        return Err(CliError::HelperFailed {
            program,
            status: output.status,
        });
    };
    let advice = (!error.hint.is_empty() || !error.next.is_empty()).then_some(Advice {
        hint: error.hint,
        next: error.next,
    });
    Err(CliError::Helper {
        kind: kind_from_text(&error.kind),
        reason: error.reason,
        message: error.message,
        advice,
    })
}
