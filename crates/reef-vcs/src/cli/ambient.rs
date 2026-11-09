//! What the environment declares for provenance (R3.44, ADR 0058): the
//! harness running this process and its session, from the agent table's
//! facts and the cross-tool `AI_AGENT` convention; and the session note a
//! harness's prompt hook leaves for the workspaces its session creates
//! next.
//!
//! Everything here is a declaration (threat model S-67, S-116): it names
//! what the environment says, and a value the daemon would refuse is
//! dropped, never fatal.

use std::ffi::OsString;
use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::time::{Duration, SystemTime};

use reef_api::v1::ProvenanceDeclaration;
use reef_client::evidence::provenance::{instruction_problem, label_problem};
use serde::{Deserialize, Serialize};

use super::CliError;
use super::hook::{create_private_dir, session_file_name};

/// The cross-tool variable a coding agent sets to name itself (Vercel's
/// `@vercel/detect-agent`, read by many libraries): free text, such as
/// Claude Code's `claude-code_2-1-316_agent`.
pub(crate) const AI_AGENT_ENV: &str = "AI_AGENT";

/// How long a session note counts.
pub(crate) const NOTE_LIFETIME: Duration = Duration::from_hours(24);

/// What the environment says about the harness running this process.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(crate) struct Ambient {
    /// `name/version`, or a name.
    pub(crate) harness: Option<String>,
    /// The harness's session id.
    pub(crate) client_session: Option<String>,
    /// The model the harness's environment names.
    pub(crate) model: Option<String>,
}

impl Ambient {
    /// From the environment; `var` looks a variable up (`std::env::var_os`).
    pub(crate) fn from_env(var: impl Fn(&str) -> Option<OsString>) -> Self {
        let text = |name: &str| {
            var(name)
                .and_then(|value| value.into_string().ok())
                .map(|value| value.trim().to_owned())
                .filter(|value| label_problem(value).is_none())
        };
        let named = var(AI_AGENT_ENV)
            .and_then(|value| value.into_string().ok())
            .and_then(|value| harness_label(&value));
        // The row's session and model only when `AI_AGENT` names the same
        // harness, or nothing: another tool's `AI_AGENT` means another
        // harness, whose session this is not.
        let facts = crate::agents::running_harness(&var).filter(|facts| {
            named
                .as_deref()
                .is_none_or(|label| label.split('/').next() == Some(facts.name))
        });
        let harness = named.or_else(|| facts.map(|facts| facts.name.to_owned()));
        Self {
            harness,
            client_session: facts.and_then(|facts| text(facts.session_env?)),
            model: facts.and_then(|facts| text(facts.model_env?)),
        }
    }

    /// From this process's environment.
    pub(crate) fn current() -> Self {
        Self::from_env(|name| std::env::var_os(name))
    }
}

/// What a new workspace declares from the environment alone: the harness,
/// its session, and the model, from [`Ambient`]; then the session's note
/// (under `state_dir`), for the instruction and a model the environment
/// does not name; and `REEF_MODEL` and `REEF_TASK` over both.
pub(crate) fn declaration(ambient: Ambient, state_dir: Option<&Path>) -> ProvenanceDeclaration {
    let note = ambient
        .client_session
        .as_deref()
        .zip(state_dir)
        .and_then(|(session, dir)| read_note(&notes_dir(dir), session, SystemTime::now()))
        .unwrap_or_default();
    let env = |name: &str, check: fn(&str) -> Option<String>| {
        std::env::var(name)
            .ok()
            .map(|value| value.trim().to_owned())
            .filter(|value| !value.is_empty() && check(value).is_none())
    };
    ProvenanceDeclaration {
        model: env(super::provenance::MODEL_ENV, label_problem)
            .or(ambient.model)
            .or(note.model),
        harness: ambient.harness.or(note.harness),
        task: env(
            super::provenance::TASK_ENV,
            reef_client::evidence::provenance::task_problem,
        ),
        instruction_sha256: note.instruction_sha256,
        client_session: ambient.client_session,
        ..ProvenanceDeclaration::default()
    }
}

/// A harness's label from an `AI_AGENT` value: `name[_/]version[_/]…`
/// reads as `name/version`, a version of digits and dashes with dots
/// (`claude-code_2-1-316_agent` is `claude-code/2.1.316`); any other value
/// a record can carry is kept as it is; a flag-like value (`1`, `true`)
/// names nobody.
pub(crate) fn harness_label(value: &str) -> Option<String> {
    let value = value.trim();
    if matches!(
        value.to_ascii_lowercase().as_str(),
        "" | "1" | "0" | "true" | "false" | "yes" | "no" | "on" | "off"
    ) {
        return None;
    }
    let mut parts = value.split(['_', '/']);
    let name = parts.next().unwrap_or_default();
    let version = parts.next();
    let name_ok = name.starts_with(|c: char| c.is_ascii_lowercase() || c.is_ascii_digit())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || ".-".contains(c));
    if let Some(version) = version
        && name_ok
        && version.starts_with(|c: char| c.is_ascii_digit())
        && version
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-+".contains(c))
    {
        let version = if version.chars().all(|c| c.is_ascii_digit() || c == '-') {
            version.replace('-', ".")
        } else {
            version.to_owned()
        };
        let label = format!("{name}/{version}");
        return label_problem(&label).is_none().then_some(label);
    }
    label_problem(value).is_none().then(|| value.to_owned())
}

/// What a harness session last declared, kept for the workspaces it
/// creates next: hashes and labels, never an instruction's text.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Note {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) instruction_sha256: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) harness: Option<String>,
}

impl Note {
    /// The note without anything a record could not carry.
    fn checked(self) -> Self {
        Self {
            instruction_sha256: self
                .instruction_sha256
                .filter(|sha| instruction_problem(sha).is_none()),
            model: self.model.filter(|model| label_problem(model).is_none()),
            harness: self
                .harness
                .filter(|harness| label_problem(harness).is_none()),
        }
    }

    fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

/// A note as stored: each field with when it was declared, so each lasts
/// [`NOTE_LIFETIME`] from its own declaration (a model declared now does
/// not renew an instruction declared yesterday).
#[derive(Clone, Debug, Default, Serialize, Deserialize)]
struct Stored {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    instruction_sha256: Option<Stamped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    model: Option<Stamped>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    harness: Option<Stamped>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
struct Stamped {
    value: String,
    /// Milliseconds since the Unix epoch.
    at: u64,
}

fn millis(time: SystemTime) -> u64 {
    time.duration_since(SystemTime::UNIX_EPOCH)
        .map_or(0, |since| {
            u64::try_from(since.as_millis()).unwrap_or(u64::MAX)
        })
}

impl Stored {
    /// The fields declared within [`NOTE_LIFETIME`] of `now`.
    fn fresh(self, now: SystemTime) -> Note {
        let cutoff = millis(now).saturating_sub(millis(SystemTime::UNIX_EPOCH + NOTE_LIFETIME));
        let fresh = |field: Option<Stamped>| field.filter(|f| f.at >= cutoff).map(|f| f.value);
        Note {
            instruction_sha256: fresh(self.instruction_sha256),
            model: fresh(self.model),
            harness: fresh(self.harness),
        }
        .checked()
    }

    /// Fields `update` sets replace these, stamped `now`; the rest are kept.
    fn merged(self, update: Note, now: SystemTime) -> Self {
        let at = millis(now);
        let stamp = |value: Option<String>, old: Option<Stamped>| {
            value.map(|value| Stamped { value, at }).or(old)
        };
        Self {
            instruction_sha256: stamp(update.instruction_sha256, self.instruction_sha256),
            model: stamp(update.model, self.model),
            harness: stamp(update.harness, self.harness),
        }
    }
}

/// `<state>/provenance/sessions`.
pub(crate) fn notes_dir(state_dir: &Path) -> PathBuf {
    state_dir.join("provenance/sessions")
}

fn note_path(dir: &Path, session: &str) -> PathBuf {
    dir.join(format!("{}.json", session_file_name(session)))
}

fn read_stored(path: &Path) -> Option<Stored> {
    serde_json::from_slice(&fs::read(path).ok()?).ok()
}

/// `session`'s note in `dir`: the fields declared in the last
/// [`NOTE_LIFETIME`].
pub(crate) fn read_note(dir: &Path, session: &str, now: SystemTime) -> Option<Note> {
    let note = read_stored(&note_path(dir, session))?.fresh(now);
    Some(note).filter(|note| !note.is_empty())
}

/// Merges `update` into `session`'s note in `dir` (a private directory, the
/// file 0600, replaced whole), and deletes other notes, and temporary files
/// a killed hook left, older than [`NOTE_LIFETIME`].
pub(crate) fn write_note(
    dir: &Path,
    session: &str,
    update: Note,
    now: SystemTime,
) -> Result<(), CliError> {
    let update = update.checked();
    if update.is_empty() {
        return Ok(());
    }
    create_private_dir(dir)?;
    let path = note_path(dir, session);
    let stored = read_stored(&path).unwrap_or_default().merged(update, now);
    let io_error = |source| CliError::Io {
        op: "write",
        path: path.clone(),
        source,
    };
    // `NamedTempFile` is created 0600.
    let mut temporary = tempfile::NamedTempFile::new_in(dir).map_err(io_error)?;
    let bytes = serde_json::to_vec(&stored).unwrap_or_default();
    temporary.write_all(&bytes).map_err(io_error)?;
    temporary
        .persist(&path)
        .map_err(|error| io_error(error.error))?;
    sweep(dir, &path, now);
    Ok(())
}

/// Deletes the notes in `dir` but `kept`, and leftover temporary files,
/// last written more than [`NOTE_LIFETIME`] before `now`; what it cannot
/// read or delete is left.
fn sweep(dir: &Path, kept: &Path, now: SystemTime) {
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    let stale = |path: &Path| {
        fs::metadata(path)
            .and_then(|meta| meta.modified())
            .is_ok_and(|modified| now.duration_since(modified).unwrap_or_default() > NOTE_LIFETIME)
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let note = path
            .extension()
            .is_some_and(|extension| extension == "json");
        let temporary = entry.file_name().to_string_lossy().starts_with(".tmp");
        if path == kept || !(note || temporary) {
            continue;
        }
        // Checked again just before: another session may have replaced it
        // since the listing.
        if stale(&path) {
            let _ = fs::remove_file(path);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt as _;

    use super::*;

    /// R3.44 test 3.
    #[test]
    fn ai_agent_values() {
        let cases = [
            ("claude-code_2-1-316_agent", Some("claude-code/2.1.316")),
            ("claude-code_2-1-316_harness", Some("claude-code/2.1.316")),
            ("claude-code/2.1.316", Some("claude-code/2.1.316")),
            ("example/1.2.3-beta.1/x", Some("example/1.2.3-beta.1")),
            ("cursor", Some("cursor")),
            ("Some-Tool", Some("Some-Tool")),
            ("1", None),
            ("true", None),
            ("", None),
            ("two words", None),
            ("tab\there", None),
        ];
        for (value, expected) in cases {
            assert_eq!(harness_label(value).as_deref(), expected, "{value:?}");
        }
        assert_eq!(harness_label(&"x".repeat(129)), None);
        assert_eq!(harness_label(&format!("a_{}", "1".repeat(200))), None);
    }

    fn env<'a>(pairs: &'a [(&'a str, &'a str)]) -> impl Fn(&str) -> Option<OsString> + 'a {
        move |name| {
            pairs
                .iter()
                .find(|(key, _)| *key == name)
                .map(|(_, value)| OsString::from(value))
        }
    }

    /// R3.44 test 4: the table's row, then `AI_AGENT` over it.
    #[test]
    fn ambient_from_the_table() {
        assert_eq!(Ambient::from_env(env(&[])), Ambient::default());
        let claude = Ambient::from_env(env(&[
            ("CLAUDECODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", "c0ffee00-1234"),
        ]));
        assert_eq!(claude.harness.as_deref(), Some("claude-code"));
        assert_eq!(claude.client_session.as_deref(), Some("c0ffee00-1234"));
        assert_eq!(claude.model, None);
        let versioned = Ambient::from_env(env(&[
            ("CLAUDECODE", "1"),
            ("AI_AGENT", "claude-code_2-1-316_agent"),
            ("CLAUDE_CODE_SESSION_ID", "bad id"),
        ]));
        assert_eq!(versioned.harness.as_deref(), Some("claude-code/2.1.316"));
        assert_eq!(versioned.client_session, None);
        // A harness with no row is named by AI_AGENT alone, with no session.
        let other = Ambient::from_env(env(&[
            ("AI_AGENT", "other-agent"),
            ("CLAUDE_CODE_SESSION_ID", "x"),
        ]));
        assert_eq!(other.harness.as_deref(), Some("other-agent"));
        assert_eq!(other.client_session, None);
        // Another tool's AI_AGENT over Claude Code's signals: that tool,
        // with no session of Claude Code's.
        let foreign = Ambient::from_env(env(&[
            ("CLAUDECODE", "1"),
            ("AI_AGENT", "cursor"),
            ("CLAUDE_CODE_SESSION_ID", "s"),
        ]));
        assert_eq!(foreign.harness.as_deref(), Some("cursor"));
        assert_eq!(foreign.client_session, None);
        // Two harnesses' signals (one started from the other's shell): no
        // session, and no name from the table.
        let nested = Ambient::from_env(env(&[
            ("CLAUDECODE", "1"),
            ("OPENCODE", "1"),
            ("CLAUDE_CODE_SESSION_ID", "s"),
        ]));
        assert_eq!(nested, Ambient::default());
    }

    /// R3.44 test 5.
    #[test]
    fn session_notes() {
        let dir = tempfile::tempdir().unwrap();
        let notes = notes_dir(dir.path());
        let now = SystemTime::now();
        let sha = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";
        assert_eq!(read_note(&notes, "s-1", now), None);
        write_note(
            &notes,
            "s-1",
            Note {
                model: Some("m-1".into()),
                ..Note::default()
            },
            now,
        )
        .unwrap();
        write_note(
            &notes,
            "s-1",
            Note {
                instruction_sha256: Some(sha.into()),
                model: Some("bad model".into()),
                ..Note::default()
            },
            now,
        )
        .unwrap();
        let note = read_note(&notes, "s-1", now).unwrap();
        assert_eq!(note.instruction_sha256.as_deref(), Some(sha));
        assert_eq!(
            note.model.as_deref(),
            Some("m-1"),
            "a bad model keeps the old"
        );
        assert_eq!(read_note(&notes, "s-2", now), None, "another session's");
        let file = notes.join("s-1.json");
        let mode = fs::metadata(&file).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o600, "{mode:o}");
        let mode = fs::metadata(&notes).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o700, "{mode:o}");
        // Too old to count, and swept by the next write.
        let later = now + NOTE_LIFETIME + Duration::from_secs(1);
        assert_eq!(read_note(&notes, "s-1", later), None);
        write_note(
            &notes,
            "../escape",
            Note {
                model: Some("m".into()),
                ..Note::default()
            },
            later,
        )
        .unwrap();
        assert!(!file.exists(), "swept");
        let names: Vec<_> = fs::read_dir(&notes)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().into_string().unwrap())
            .collect();
        assert_eq!(names.len(), 1);
        assert!(names[0].starts_with('h'), "{names:?}");
        // Nothing to write writes nothing.
        write_note(&notes, "s-3", Note::default(), now).unwrap();
        assert!(!notes.join("s-3.json").exists());
        // Each field ages from its own declaration: a model declared later
        // does not renew an older instruction.
        let day = Duration::from_hours(23);
        write_note(
            &notes,
            "s-4",
            Note {
                instruction_sha256: Some(sha.into()),
                ..Note::default()
            },
            now,
        )
        .unwrap();
        write_note(
            &notes,
            "s-4",
            Note {
                model: Some("m-2".into()),
                ..Note::default()
            },
            now + day,
        )
        .unwrap();
        let aged = read_note(&notes, "s-4", now + day + Duration::from_hours(2)).unwrap();
        assert_eq!(aged.instruction_sha256, None);
        assert_eq!(aged.model.as_deref(), Some("m-2"));
    }
}
