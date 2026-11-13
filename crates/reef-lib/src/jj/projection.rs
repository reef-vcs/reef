//! The landed history projection's summary (R3.16, ADR 0075).
//!
//! A landing onto a branch whose policy says `[history] message =
//! "summary"` writes each landed change with [`summarize`]d description:
//! the change's own text, normalized, with the trailers the projection owns
//! removed from its trailer block and the projection's appended to it. The
//! daemon computes the trailers; [`super::land`] writes the description and
//! checks that no tree changed.

/// The longest trailer value [`summarize`] writes, in bytes.
pub const MAX_TRAILER_VALUE: usize = 200;

/// The prefix of the trailer keys the projection owns, compared without
/// regard to case: an agent's `Reef-*` trailers are always removed.
pub const OWNED_PREFIX: &str = "reef-";

/// Whether `c` is an invisible formatting character that can make text
/// read differently from what it is: bidirectional embeddings, overrides
/// and isolates, zero-width characters, and the byte order mark.
pub fn invisible(c: char) -> bool {
    matches!(
        c,
        '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2069}' | '\u{FEFF}'
    )
}

/// `text` without control characters other than newline and tab, nor
/// [`invisible`] ones, with `\r\n` and lone `\r` as `\n`, each line's
/// trailing whitespace removed, and no leading or trailing blank line.
/// Empty when nothing is left.
pub fn normalize(text: &str) -> String {
    let unified = text.replace("\r\n", "\n").replace('\r', "\n");
    let cleaned: String = unified
        .chars()
        .filter(|c| (!c.is_control() || *c == '\n' || *c == '\t') && !invisible(*c))
        .collect();
    let lines: Vec<&str> = cleaned.lines().map(str::trim_end).collect();
    let first = lines.iter().position(|line| !line.is_empty());
    let last = lines.iter().rposition(|line| !line.is_empty());
    match (first, last) {
        (Some(first), Some(last)) => lines[first..=last].join("\n"),
        _ => String::new(),
    }
}

/// The key of a trailer line (`Key: value`, `Key : value`, `Key\t: value`),
/// as git reads one, which is looser than jj's parser.
fn trailer_key(line: &str) -> Option<&str> {
    let colon = line.find(':')?;
    let key = line[..colon].trim_end_matches([' ', '\t']);
    (!key.is_empty() && key.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')).then_some(key)
}

/// Whether the last paragraph of `body` (normalized, non-empty) is a
/// trailer block, as `jj_lib::trailer::parse_description_trailers` decides.
fn has_trailer_block(body: &str) -> bool {
    !jj_lib::trailer::parse_description_trailers(body).is_empty()
}

/// `value` as one trailer value: no control or [`invisible`] characters,
/// trimmed, at most
/// [`MAX_TRAILER_VALUE`] bytes (cut at a character boundary).
pub fn trailer_value(value: &str) -> String {
    let cleaned: String = value
        .chars()
        .filter(|c| !c.is_control() && !invisible(*c))
        .collect();
    let mut value = cleaned.trim().to_owned();
    if value.len() > MAX_TRAILER_VALUE {
        let mut end = MAX_TRAILER_VALUE;
        while !value.is_char_boundary(end) {
            end -= 1;
        }
        value.truncate(end);
        value.truncate(value.trim_end().len());
    }
    value
}

/// The summary of `description` (R3.16): [`normalize`]d; every trailer line
/// of its last paragraph (after its title) whose key starts with `Reef-`,
/// or is one of `strip` (both without regard to case), removed with its
/// continuation lines, whether or not jj reads the paragraph as a trailer
/// block, since git's reading is looser; and
/// `trailers` (key, value) appended to that block, or after a blank line
/// when there is none. Values pass through [`trailer_value`]; an empty one
/// is left out. Ends with one newline, or is empty when there is nothing.
pub fn summarize(description: &str, strip: &[&str], trailers: &[(&str, String)]) -> String {
    let normalized = normalize(description);
    let owned = |key: &str| {
        key.to_ascii_lowercase().starts_with(OWNED_PREFIX)
            || strip.iter().any(|other| key.eq_ignore_ascii_case(other))
    };
    let mut lines: Vec<&str> = normalized.lines().collect();
    if let Some(blank) = lines.iter().rposition(|line| line.is_empty()) {
        let block = lines.split_off(blank + 1);
        let mut kept: Vec<&str> = Vec::with_capacity(block.len());
        let mut dropping = false;
        for line in block {
            if line.starts_with([' ', '\t']) {
                if !dropping {
                    kept.push(line);
                }
                continue;
            }
            dropping = trailer_key(line).is_some_and(owned);
            if !dropping {
                kept.push(line);
            }
        }
        if kept.is_empty() {
            // The paragraph held only owned trailers: drop it and the blank
            // line before it.
            lines.pop();
            while lines.last().is_some_and(|line| line.is_empty()) {
                lines.pop();
            }
        } else {
            lines.extend(kept);
        }
    }
    let body = lines.join("\n");
    let added: Vec<String> = trailers
        .iter()
        .filter_map(|(key, value)| {
            let value = trailer_value(value);
            (!value.is_empty()).then(|| format!("{key}: {value}"))
        })
        .collect();
    let mut text = if added.is_empty() {
        body
    } else if body.is_empty() {
        added.join("\n")
    } else if has_trailer_block(&body) {
        format!("{body}\n{}", added.join("\n"))
    } else {
        format!("{body}\n\n{}", added.join("\n"))
    };
    if !text.is_empty() {
        text.push('\n');
    }
    text
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ids() -> Vec<(&'static str, String)> {
        vec![
            ("Reef-Change", "0123abcd".to_owned()),
            ("Reef-Landing", "lnd_0000000000000001".to_owned()),
        ]
    }

    #[test]
    fn normalizes() {
        assert_eq!(normalize(""), "");
        assert_eq!(normalize(" \n\t\n"), "");
        assert_eq!(
            normalize("\n\nFix it  \r\n\r\nBody\x1b[31m red\x07\t\n\n"),
            "Fix it\n\nBody[31m red"
        );
        assert_eq!(normalize("a\rb"), "a\nb");
        assert_eq!(normalize("a\u{202E}b\u{200B}c\u{2066}d\u{FEFF}"), "abcd");
    }

    #[test]
    fn appends_after_a_blank_line_without_a_block() {
        assert_eq!(
            summarize("Fix it", &[], &ids()),
            "Fix it\n\nReef-Change: 0123abcd\nReef-Landing: lnd_0000000000000001\n"
        );
        assert_eq!(
            summarize("Fix it\n\nBody text here.\n", &[], &ids()),
            "Fix it\n\nBody text here.\n\nReef-Change: 0123abcd\nReef-Landing: lnd_0000000000000001\n"
        );
        assert_eq!(summarize("Fix it\n\n\n", &[], &[]), "Fix it\n");
        assert_eq!(summarize("", &[], &[]), "");
    }

    #[test]
    fn extends_the_block_and_keeps_other_trailers() {
        let text = "Fix it\n\nBody.\n\nCo-Authored-By: C <c@d>\nSigned-off-by: A B <a@b>\n";
        assert_eq!(
            summarize(text, &[], &ids()),
            "Fix it\n\nBody.\n\nCo-Authored-By: C <c@d>\nSigned-off-by: A B <a@b>\nReef-Change: 0123abcd\nReef-Landing: lnd_0000000000000001\n"
        );
    }

    #[test]
    fn removes_owned_trailers() {
        let text = "Fix it\n\nSigned-off-by: A B <a@b>\nREEF-landing: lnd_forged\nreef-agent: x\n  continued\nReviewed-by: Mallory <m@x>\n";
        assert_eq!(
            summarize(text, &[], &ids()),
            "Fix it\n\nSigned-off-by: A B <a@b>\nReviewed-by: Mallory <m@x>\nReef-Change: 0123abcd\nReef-Landing: lnd_0000000000000001\n"
        );
        assert_eq!(
            summarize(
                text,
                &["Reviewed-by"],
                &[("Reviewed-by", "Alice <a@x>".to_owned())]
            ),
            "Fix it\n\nSigned-off-by: A B <a@b>\nReviewed-by: Alice <a@x>\n"
        );
        // A block of owned trailers only goes, with its blank line.
        assert_eq!(
            summarize("Fix it\n\nSigned-off-by: A <a>\nReef-Change: x\n", &[], &[]),
            "Fix it\n\nSigned-off-by: A <a>\n"
        );
        assert_eq!(
            summarize("Fix it\n\nReef-Change: x\nReef-Batch: y\n", &[], &[]),
            "Fix it\n"
        );
        // Owned lines go from a last paragraph jj would not read as a
        // trailer block too: git may.
        assert_eq!(
            summarize("Fix it\n\nReef-Change: x\nand some prose\n", &[], &[]),
            "Fix it\n\nand some prose\n"
        );
        // git accepts whitespace before the colon, and tab continuations.
        assert_eq!(
            summarize(
                "Fix it\n\nSigned-off-by: A <a>\nReef-Landing\t: lnd_forged\n\tmore\nReviewed-by \t: M <m>\n",
                &["Reviewed-by"],
                &[]
            ),
            "Fix it\n\nSigned-off-by: A <a>\n"
        );
        assert_eq!(
            summarize("Fix it\n\nReef-Landing\t: x\n", &[], &[]),
            "Fix it\n"
        );
        // The title paragraph is never a trailer block.
        assert_eq!(summarize("Reef-Change: x", &[], &[]), "Reef-Change: x\n");
    }

    #[test]
    fn cleans_trailer_values() {
        let long = "é".repeat(150);
        let value = trailer_value(&long);
        assert!(value.len() <= MAX_TRAILER_VALUE && value.chars().all(|c| c == 'é'));
        assert_eq!(trailer_value(" a\nb\x1b "), "ab");
        assert_eq!(
            summarize("Fix it", &[], &[("Reef-Agent", "\n".to_owned())]),
            "Fix it\n"
        );
    }
}
