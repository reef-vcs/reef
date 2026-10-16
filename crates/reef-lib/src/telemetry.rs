//! Logging conventions shared by every Reef crate (ADR 0009).
//!
//! Diagnostics go through `tracing`, always with the macros' full path
//! (`tracing::info!`). Events are structured: the message is a short lowercase
//! phrase and the data goes in fields named from [`fields`]. Every public
//! async entry point opens a span with `#[tracing::instrument(skip_all,
//! fields(..))]` and reports a returned error once, through [`log_failure`];
//! code beneath it returns errors without logging them. Libraries only emit;
//! `reef_daemon::logging` installs the subscriber.

use std::error::Error;
use std::time::Instant;

use crate::error::{Classify, ErrorChain};

/// The field names events and spans may use. `tracing`'s macros take field
/// names as identifiers, so these constants cannot be passed to them; they
/// fix the spelling for code, tests, and log consumers. Besides these, a
/// field may end in `_count`, `_ms`, or `_bytes`. A test in this module
/// checks every event and span in `reef-lib` against the list.
pub mod fields {
    /// A repository, by the path of its workspace root.
    pub const REPO: &str = "repo";
    /// A Reef workspace.
    pub const WORKSPACE_ID: &str = "workspace_id";
    /// A jj change id, in reverse hex.
    pub const CHANGE_ID: &str = "change_id";
    /// A jj operation id.
    pub const OP_ID: &str = "op_id";
    /// A commit id.
    pub const COMMIT_ID: &str = "commit_id";
    /// The agent a request came from.
    pub const AGENT_ID: &str = "agent_id";
    /// A workspace lease.
    pub const LEASE_ID: &str = "lease_id";
    /// A workspace materialization backend.
    pub const BACKEND: &str = "backend";
    /// A filesystem or repository-relative path.
    pub const PATH: &str = "path";
    /// A duration in milliseconds.
    pub const ELAPSED_MS: &str = "elapsed_ms";
    /// An error, with its source chain.
    pub const ERROR: &str = "error";

    /// Every name above.
    pub const ALL: &[&str] = &[
        REPO,
        WORKSPACE_ID,
        CHANGE_ID,
        OP_ID,
        COMMIT_ID,
        AGENT_ID,
        LEASE_ID,
        BACKEND,
        PATH,
        ELAPSED_MS,
        ERROR,
    ];
}

/// Milliseconds since `start`, for an `elapsed_ms` field.
pub fn elapsed_ms(start: Instant) -> u64 {
    u64::try_from(start.elapsed().as_millis()).unwrap_or(u64::MAX)
}

/// Logs a failed operation's error with its source chain, in the `error`
/// field: at `warn` when the caller can act on it (see
/// [`ErrorKind::is_caller_error`](crate::error::ErrorKind::is_caller_error)),
/// at `error` otherwise. Called once, by the entry point that opened the
/// operation's span, so the event carries the span's identifying fields.
pub fn log_failure<E: Error + Classify + 'static>(error: &E) {
    let chain = ErrorChain(error);
    if error.kind().is_caller_error() {
        tracing::warn!(error = %chain, "operation failed");
    } else {
        tracing::error!(error = %chain, "operation failed");
    }
}

#[cfg(test)]
mod tests {
    use std::fs;
    use std::path::{Path, PathBuf};

    use super::fields;

    const LEVELS: [&str; 5] = ["trace", "debug", "info", "warn", "error"];

    fn allowed(name: &str) -> bool {
        fields::ALL.contains(&name)
            || ["_count", "_ms", "_bytes"]
                .iter()
                .any(|suffix| name.ends_with(suffix))
    }

    /// The arguments of a macro or attribute call, split at top-level commas.
    /// `text` starts just after the opening parenthesis.
    fn args(text: &str) -> Vec<&str> {
        let (mut depth, mut start, mut in_str, mut escaped) = (0_u32, 0, false, false);
        let mut out = Vec::new();
        for (i, c) in text.char_indices() {
            if in_str {
                match c {
                    _ if escaped => escaped = false,
                    '\\' => escaped = true,
                    '"' => in_str = false,
                    _ => {}
                }
                continue;
            }
            match c {
                '"' => in_str = true,
                '(' | '[' | '{' => depth += 1,
                ')' | ']' | '}' if depth == 0 => {
                    out.push(&text[start..i]);
                    return out;
                }
                ')' | ']' | '}' => depth -= 1,
                ',' if depth == 0 => {
                    out.push(&text[start..i]);
                    start = i + 1;
                }
                _ => {}
            }
        }
        out
    }

    /// The name of a field argument such as `op_id = %id` or `%op_id`.
    fn field_name(arg: &str) -> &str {
        let lhs = arg.split_once('=').map_or(arg, |(lhs, _)| lhs);
        lhs.trim().trim_start_matches(['%', '?']).trim()
    }

    /// Every field name declared by an event macro or `instrument` attribute
    /// in `source`, plus every level macro not called as `tracing::<level>!`.
    fn scan(source: &str) -> (Vec<String>, Vec<String>) {
        let (mut names, mut unqualified) = (Vec::new(), Vec::new());
        for level in LEVELS {
            let call = format!("{level}!(");
            for (at, _) in source.match_indices(&call) {
                if !source[..at].ends_with("tracing::") {
                    unqualified.push(call.clone());
                    continue;
                }
                for arg in args(&source[at + call.len()..]) {
                    let arg = arg.trim();
                    // The message ends the fields; format arguments follow it.
                    if arg.starts_with('"') {
                        break;
                    }
                    if ["target:", "parent:", "name:"]
                        .iter()
                        .any(|key| arg.starts_with(key))
                    {
                        continue;
                    }
                    names.push(field_name(arg).to_owned());
                }
            }
        }
        // Split so the scan of this file does not match the literal itself.
        let attribute = concat!("tracing::", "instrument(");
        for (at, _) in source.match_indices(attribute) {
            for arg in args(&source[at + attribute.len()..]) {
                if let Some(inner) = arg.trim().strip_prefix("fields(") {
                    names.extend(args(inner).into_iter().map(|f| field_name(f).to_owned()));
                }
            }
        }
        (names, unqualified)
    }

    fn rust_files(dir: &Path, out: &mut Vec<PathBuf>) {
        for entry in fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                rust_files(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    #[test]
    fn scan_finds_fields() {
        // Built at run time so this file's own source holds no calls to scan.
        let source = format!(
            "tracing::{}!(op_id = %id, %path, target: \"t\", \"msg {{x}}\", x = 1);\n\
             #[tracing::{}(skip_all, fields(repo = %r.display(), size_bytes))]\n\
             {}!(\"bare\");",
            "info", "instrument", "warn",
        );
        let (names, unqualified) = scan(&source);
        assert_eq!(names, ["op_id", "path", "repo", "size_bytes"]);
        assert_eq!(unqualified, [format!("{}!(", "warn")]);
    }

    /// Field names drift silently, and log consumers key on them, so every
    /// event and span in this crate must use a name from [`fields`].
    #[test]
    fn field_names_are_standard() {
        let mut files = Vec::new();
        rust_files(
            &Path::new(env!("CARGO_MANIFEST_DIR")).join("src"),
            &mut files,
        );
        let mut checked = 0;
        for file in files {
            let (names, unqualified) = scan(&fs::read_to_string(&file).unwrap());
            for name in &names {
                assert!(
                    allowed(name),
                    "{}: field `{name}` is not in telemetry::fields and has no _count, _ms, or \
                     _bytes suffix",
                    file.display()
                );
            }
            assert!(
                unqualified.is_empty(),
                "{}: call {unqualified:?} as tracing::<level>! so this test sees it",
                file.display()
            );
            checked += names.len();
        }
        assert!(checked > 0, "no fields found; the scanner is broken");
    }
}
