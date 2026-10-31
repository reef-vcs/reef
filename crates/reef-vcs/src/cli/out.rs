//! Output rules (README, CLI). JSON (`-o json`): exactly one JSON value on
//! stdout per command, an object or, for lists, an array, and on failure
//! `{"error": {"kind", "reason"?, "message", "hint"?, "next"?,
//! "metadata"?}}`. Brief (`-o brief`, or stdout not a terminal; ADR 0045):
//! the same value, compact, a top-level array one element per line, and
//! for the commands with a view in `reef_client::brief` (`ws list`,
//! `status`, `diff`) only the keys an agent acts on: keys are only left out,
//! never renamed or changed; zero values are left out; and each view drops
//! the keys its type lists. Text (`-o text`, or a
//! terminal): `key: value` blocks and aligned columns. Errors always go to stderr as `error: <chain>`,
//! followed by `hint: <sentence>` and `next: <command>` when the error has
//! advice (ADR 0025); notes such as `released <id>` go to stderr in text
//! mode only, and `-q` silences them.

use std::collections::BTreeMap;
use std::io::{self, IsTerminal as _, Write};

use reef_client::json::{ErrorDocument, ErrorObject};
use reef_error::Advice;
use reef_error::{Classify as _, ErrorChain};
use serde::Serialize;

use super::CliError;
use super::error::kind_text;

/// The output format.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub(crate) enum Output {
    // Aligned columns and `key: value` blocks. (Not doc comments, which
    // would list the values in every subcommand's `--help`.)
    #[value(alias = "toml")]
    Text,
    // One JSON value per command, every field.
    Json,
    // The JSON, compact, and for some commands only the keys an agent
    // acts on (ADR 0045).
    Brief,
}

/// Where a command's results go, and in what form.
#[derive(Clone, Debug)]
pub(crate) struct Out {
    mode: Output,
    quiet: bool,
}

/// A value printed as a `key: value` block in text mode.
pub(crate) trait TextRender {
    fn render(&self, w: &mut dyn Write) -> io::Result<()>;
}

/// A value printed as one row of a table in text mode.
pub(crate) trait TextRow {
    const HEADER: &'static [&'static str];
    fn row(&self) -> Vec<String>;
}

impl Out {
    /// `flag` if given, else brief JSON unless stdout is a terminal.
    pub(crate) fn detect(flag: Option<Output>, quiet: bool) -> Self {
        let mode = flag.unwrap_or_else(|| {
            if io::stdout().is_terminal() {
                Output::Text
            } else {
                Output::Brief
            }
        });
        Self { mode, quiet }
    }

    /// The format in effect.
    pub(crate) fn mode(&self) -> Output {
        self.mode
    }

    /// Whether results are JSON, full or brief.
    pub(crate) fn is_json(&self) -> bool {
        self.mode != Output::Text
    }

    /// A single result.
    pub(crate) fn object<T: Serialize + TextRender>(&self, value: &T) -> Result<(), CliError> {
        self.object_with(value, |value| value)
    }

    /// A single result, as `brief(value)` in brief mode.
    pub(crate) fn object_with<'a, T, B>(
        &self,
        value: &'a T,
        brief: impl FnOnce(&'a T) -> B,
    ) -> Result<(), CliError>
    where
        T: Serialize + TextRender,
        B: Serialize,
    {
        let mut buffer = Vec::new();
        match self.mode {
            Output::Json => json_into(&mut buffer, value),
            Output::Brief => compact_into(&mut buffer, &brief(value)),
            Output::Text => value.render(&mut buffer).map_err(CliError::Stdout)?,
        }
        self.raw(&buffer)
    }

    /// A list: a JSON array, or a header line and one aligned row per value.
    pub(crate) fn list<T: Serialize + TextRow>(&self, values: &[T]) -> Result<(), CliError> {
        self.list_with(values, <[T]>::iter)
    }

    /// A list, as the values `brief(values)` yields in brief mode.
    pub(crate) fn list_with<'a, T, B, I>(
        &self,
        values: &'a [T],
        brief: impl FnOnce(&'a [T]) -> I,
    ) -> Result<(), CliError>
    where
        T: Serialize + TextRow,
        B: Serialize,
        I: IntoIterator<Item = B>,
    {
        let mut buffer = Vec::new();
        match self.mode {
            Output::Json => json_into(&mut buffer, &values),
            Output::Brief => lines_into(&mut buffer, brief(values)),
            Output::Text => {
                let rows: Vec<Vec<String>> = values.iter().map(TextRow::row).collect();
                table(&mut buffer, T::HEADER, &rows);
            }
        }
        self.raw(&buffer)
    }

    /// One item of a stream (`op watch`), written and flushed at once: a
    /// line of compact JSON in JSON mode (JSON Lines), its text form
    /// otherwise. Whether stdout is still open.
    pub(crate) fn stream_item<T: Serialize + TextRender>(
        &self,
        value: &T,
    ) -> Result<bool, CliError> {
        let mut buffer = Vec::new();
        match self.mode {
            Output::Json | Output::Brief => compact_into(&mut buffer, value),
            Output::Text => value.render(&mut buffer).map_err(CliError::Stdout)?,
        }
        self.raw_open(&buffer)
    }

    /// A result that has no text form beyond a note: `value` on stdout in
    /// JSON mode, `note` on stderr in text mode.
    pub(crate) fn done<T: Serialize>(&self, value: &T, note: &str) -> Result<(), CliError> {
        if self.is_json() {
            self.json_only(value)
        } else {
            self.note(note);
            Ok(())
        }
    }

    /// A line on stderr, in both modes, unless `-q`.
    #[expect(clippy::print_stderr, reason = "the CLI's notes go to stderr")]
    pub(crate) fn note(&self, text: &str) {
        if !self.quiet {
            eprintln!("{text}");
        }
    }

    /// Bytes on stdout as they are. A closed stdout (`reef ws list | head`)
    /// is not a failure.
    pub(crate) fn raw(&self, bytes: &[u8]) -> Result<(), CliError> {
        self.raw_open(bytes).map(|_| ())
    }

    /// [`Out::raw`], and whether stdout is still open, so a command
    /// writing in pieces (`file read`) stops once nobody reads.
    #[expect(
        clippy::unused_self,
        reason = "every result goes through an `Out`, raw bytes too"
    )]
    pub(crate) fn raw_open(&self, bytes: &[u8]) -> Result<bool, CliError> {
        let mut stdout = io::stdout().lock();
        match stdout.write_all(bytes).and_then(|()| stdout.flush()) {
            Ok(()) => Ok(true),
            Err(error) if error.kind() == io::ErrorKind::BrokenPipe => Ok(false),
            Err(error) => Err(CliError::Stdout(error)),
        }
    }

    /// A JSON value on stdout in JSON mode; nothing in text mode, where
    /// stdout belongs to something else (`ws run`'s command).
    pub(crate) fn json_only<T: Serialize>(&self, value: &T) -> Result<(), CliError> {
        if !self.is_json() {
            return Ok(());
        }
        let mut buffer = Vec::new();
        self.json_into(&mut buffer, value);
        self.raw(&buffer)
    }

    /// A line of another program's output (`store build`'s command): to
    /// stdout or stderr as it came in text mode, to stderr in JSON mode,
    /// where stdout holds the one JSON value. Nothing with `-q`.
    #[expect(clippy::print_stderr, reason = "the build's own stderr")]
    pub(crate) fn passthrough(&self, line: &str, stderr: bool) {
        if self.quiet {
            return;
        }
        if stderr || self.is_json() {
            eprintln!("{line}");
        } else {
            let mut text = line.to_owned();
            text.push('\n');
            // A closed stdout is not worth stopping the build's output for.
            let _ = self.raw(text.as_bytes());
        }
    }

    /// A line on stderr, in both modes, even with `-q`: a failure that is
    /// not the command's result, such as `ws run`'s release after its
    /// command exited.
    #[expect(clippy::print_stderr, reason = "the CLI reports its own failures")]
    #[expect(
        clippy::unused_self,
        reason = "every message goes through an `Out`, failures too"
    )]
    pub(crate) fn error_line(&self, text: &str) {
        eprintln!("{text}");
    }

    /// `error: <chain>` on stderr, then `hint:` and `next:` lines when the
    /// error has advice; in JSON mode also the error object on stdout.
    pub(crate) fn error(&self, error: &CliError) {
        let message = ErrorChain(error).to_string();
        let advice = error.advice();
        self.error_line(&format!("error: {message}"));
        if let Some(advice) = &advice {
            if !advice.hint.is_empty() {
                self.error_line(&format!("hint: {}", advice.hint));
            }
            if !advice.next.is_empty() {
                self.error_line(&format!("next: {}", shell_line(&advice.next)));
            }
        }
        if self.is_json() {
            self.print_error(error_object(error));
        }
    }

    /// clap's rendering of a usage error on stderr, which already ends in
    /// the usage line, then `hint:` and `next:` lines for a known `fix`; in
    /// JSON mode also the error object, with `message` as its first line
    /// and the fix as its advice, else the usage line as its hint.
    #[expect(clippy::print_stderr, reason = "the CLI reports its own failures")]
    pub(crate) fn usage_error(
        &self,
        rendered: &str,
        message: &str,
        usage: Option<&str>,
        fix: Option<&Advice>,
    ) {
        eprint!("{rendered}");
        if let Some(fix) = fix {
            self.error_line(&format!("hint: {}", fix.hint));
            self.error_line(&format!("next: {}", shell_line(&fix.next)));
        }
        if self.is_json() {
            let advice = fix.cloned().or_else(|| usage.map(Advice::hint));
            self.print_error(object(
                "invalid_argument",
                "",
                message,
                advice.as_ref(),
                &BTreeMap::new(),
            ));
        }
    }

    /// `value` as this mode writes JSON: pretty, or compact when brief.
    fn json_into<T: Serialize + ?Sized>(&self, buffer: &mut Vec<u8>, value: &T) {
        if self.mode == Output::Brief {
            compact_into(buffer, value);
        } else {
            json_into(buffer, value);
        }
    }

    fn print_error(&self, error: ErrorObject) {
        let mut buffer = Vec::new();
        self.json_into(&mut buffer, &ErrorDocument { error });
        // Already failing; a closed stdout changes nothing.
        let _ = self.raw(&buffer);
    }
}

/// The JSON error object of a failed command, which `reef mcp` returns as a
/// tool's error result too (R2.13).
pub(crate) fn error_object(error: &CliError) -> ErrorObject {
    object(
        kind_text(error.kind()),
        error.reason(),
        &ErrorChain(error).to_string(),
        error.advice().as_ref(),
        error.metadata(),
    )
}

/// The object `reef-daemon`'s offline commands print too (R2.05).
fn object(
    kind: &str,
    reason: &str,
    message: &str,
    advice: Option<&Advice>,
    metadata: &BTreeMap<String, String>,
) -> ErrorObject {
    ErrorObject {
        kind: kind.to_owned(),
        reason: reason.to_owned(),
        message: message.to_owned(),
        hint: advice.map_or_else(String::new, |advice| advice.hint.clone()),
        next: advice.map_or_else(Vec::new, |advice| advice.next.clone()),
        metadata: metadata.clone(),
    }
}

fn json_into<T: Serialize + ?Sized>(buffer: &mut Vec<u8>, value: &T) {
    // The output types are plain structs of strings and numbers; they
    // always serialize.
    if serde_json::to_writer_pretty(&mut *buffer, value).is_ok() {
        buffer.push(b'\n');
    }
}

fn compact_into<T: Serialize + ?Sized>(buffer: &mut Vec<u8>, value: &T) {
    if serde_json::to_writer(&mut *buffer, value).is_ok() {
        buffer.push(b'\n');
    }
}

/// A brief list: `[`, one compact element per line, `]`; `[]` when empty.
fn lines_into<B: Serialize>(buffer: &mut Vec<u8>, values: impl IntoIterator<Item = B>) {
    let mut values = values.into_iter().peekable();
    if values.peek().is_none() {
        buffer.extend_from_slice(b"[]\n");
        return;
    }
    buffer.extend_from_slice(b"[\n");
    while let Some(value) = values.next() {
        // As in `json_into`: output types always serialize.
        let _ = serde_json::to_writer(&mut *buffer, &value);
        buffer.extend_from_slice(if values.peek().is_some() {
            b",\n"
        } else {
            b"\n"
        });
    }
    buffer.extend_from_slice(b"]\n");
}

/// `rows` under `header`, each column padded to its widest cell with
/// two-space gutters and no trailing spaces. Cells are ASCII (ids, states,
/// paths are passed through as they are and only pad by `chars`).
fn table(w: &mut Vec<u8>, header: &[&str], rows: &[Vec<String>]) {
    let mut widths: Vec<usize> = header.iter().map(|cell| cell.chars().count()).collect();
    for row in rows {
        for (width, cell) in widths.iter_mut().zip(row) {
            *width = (*width).max(cell.chars().count());
        }
    }
    let mut line = |cells: &mut dyn Iterator<Item = &str>| {
        let mut text = String::new();
        for (index, (cell, width)) in cells.zip(&widths).enumerate() {
            if index > 0 {
                text.push_str("  ");
            }
            text.push_str(cell);
            text.extend(std::iter::repeat_n(' ', width - cell.chars().count()));
        }
        w.extend_from_slice(text.trim_end().as_bytes());
        w.push(b'\n');
    };
    line(&mut header.iter().copied());
    for row in rows {
        line(&mut row.iter().map(String::as_str));
    }
}

/// An argv as one line a POSIX shell reads back as the same words: words
/// with anything but `[A-Za-z0-9@%+=:,./_-]` single-quoted.
pub(crate) fn shell_line(argv: &[String]) -> String {
    argv.iter()
        .map(|word| {
            let plain = !word.is_empty()
                && word
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"@%+=:,./_-".contains(&b));
            if plain {
                word.clone()
            } else {
                format!("'{}'", word.replace('\'', r"'\''"))
            }
        })
        .collect::<Vec<_>>()
        .join(" ")
}

/// `key: value` lines with the values aligned.
pub(crate) fn block(w: &mut dyn Write, pairs: &[(&str, String)]) -> io::Result<()> {
    let width = pairs.iter().map(|(key, _)| key.len()).max().unwrap_or(0);
    for (key, value) in pairs {
        writeln!(w, "{key}:{:pad$} {value}", "", pad = width - key.len())?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn tables_align_columns_without_trailing_space() {
        let mut buffer = Vec::new();
        table(
            &mut buffer,
            &["ID", "STATE", "PATH"],
            &[
                vec!["abcd".into(), "active".into(), "/ws/1".into()],
                vec!["ef".into(), String::new(), "/ws/22".into()],
            ],
        );
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "ID    STATE   PATH\nabcd  active  /ws/1\nef            /ws/22\n"
        );
    }

    #[test]
    fn brief_lists_one_compact_element_per_line() {
        let mut buffer = Vec::new();
        lines_into(&mut buffer, [json!({"a": 1}), json!({"b": [1, 2]})]);
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "[\n{\"a\":1},\n{\"b\":[1,2]}\n]\n"
        );
        let mut buffer = Vec::new();
        lines_into(&mut buffer, Vec::<serde_json::Value>::new());
        assert_eq!(String::from_utf8(buffer).unwrap(), "[]\n");
        let mut buffer = Vec::new();
        compact_into(&mut buffer, &json!({"a": {"b": 1}}));
        assert_eq!(String::from_utf8(buffer).unwrap(), "{\"a\":{\"b\":1}}\n");
        // What a full list parses as, a brief one does too.
        let mut buffer = Vec::new();
        lines_into(&mut buffer, [1, 2, 3]);
        let parsed: Vec<u32> = serde_json::from_slice(&buffer).unwrap();
        assert_eq!(parsed, [1, 2, 3]);
    }

    #[test]
    fn shell_lines_quote_only_what_needs_it() {
        let argv = |words: &[&str]| words.iter().map(|w| (*w).to_owned()).collect::<Vec<_>>();
        assert_eq!(
            shell_line(&argv(&[
                "reef", "store", "build", "deps", "--rev", "abc123"
            ])),
            "reef store build deps --rev abc123"
        );
        assert_eq!(
            shell_line(&argv(&["reef", "repo", "register", "/a b/it's", ""])),
            r"reef repo register '/a b/it'\''s' ''"
        );
    }

    #[test]
    fn blocks_align_values() {
        let mut buffer = Vec::new();
        block(
            &mut buffer,
            &[("id", "abcd".into()), ("state", "active".into())],
        )
        .unwrap();
        assert_eq!(
            String::from_utf8(buffer).unwrap(),
            "id:    abcd\nstate: active\n"
        );
    }
}
