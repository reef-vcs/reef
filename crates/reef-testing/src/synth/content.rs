//! File contents: pseudo-source text and incompressible binary.

use rand::Rng;

use super::dist::{SynthRng, below, index};
use super::names;

/// Separators between words, repeated to weight the common ones.
const SEPARATORS: &[&str] = &[
    " ", " ", " ", " ", " ", "_", "_", ".", ".", "(", ") ", ", ", " = ", "::", "; ", " -> ", ": ",
    "[", "] ", " { ", " }",
];

/// Shortest and longest generated line, excluding the newline.
const MIN_LINE: u64 = 20;
const MAX_LINE: usize = 80;

/// Newline-terminated pseudo-source of exactly `bytes` bytes (for
/// `bytes > 0`): lines of 20–80 characters built from the word list and
/// punctuation, so it compresses like code. The last line is cut short to
/// hit the size.
pub(crate) fn text(rng: &mut SynthRng, bytes: u64) -> Vec<u8> {
    let len = usize::try_from(bytes).unwrap_or(usize::MAX);
    let mut out = Vec::with_capacity(len + MAX_LINE + 1);
    while out.len() < len {
        push_line(rng, &mut out);
    }
    out.truncate(len);
    if let Some(last) = out.last_mut() {
        *last = b'\n';
    }
    out
}

/// Appends one newline-terminated line to `out`.
pub(crate) fn push_line(rng: &mut SynthRng, out: &mut Vec<u8>) {
    let start = out.len();
    let indent = [0, 0, 4, 4, 8, 12][index(rng, 6)];
    out.resize(start + indent, b' ');
    let target = start + usize::try_from(MIN_LINE + below(rng, 61)).unwrap_or(MAX_LINE);
    loop {
        out.extend_from_slice(names::word(rng).as_bytes());
        if out.len() >= target {
            break;
        }
        out.extend_from_slice(SEPARATORS[index(rng, SEPARATORS.len())].as_bytes());
    }
    out.truncate(start + MAX_LINE.min(out.len() - start));
    out.push(b'\n');
}

/// `bytes` bytes straight from the random stream.
pub(crate) fn binary(rng: &mut SynthRng, bytes: u64) -> Vec<u8> {
    let mut out = vec![0; usize::try_from(bytes).unwrap_or(usize::MAX)];
    rng.fill_bytes(&mut out);
    out
}

#[cfg(test)]
mod tests {
    use rand::SeedableRng;

    use super::*;

    #[test]
    fn text_has_exact_size_and_trailing_newline() {
        let mut rng = SynthRng::seed_from_u64(3);
        for bytes in [1, 2, 19, 80, 81, 2048, 100_000] {
            let text = text(&mut rng, bytes);
            assert_eq!(text.len() as u64, bytes);
            assert_eq!(text.last(), Some(&b'\n'));
            assert!(text.iter().all(u8::is_ascii));
        }
    }

    #[test]
    fn lines_are_at_most_eighty_chars() {
        let mut rng = SynthRng::seed_from_u64(4);
        let mut out = Vec::new();
        for _ in 0..1000 {
            push_line(&mut rng, &mut out);
        }
        for line in out.split(|&b| b == b'\n').filter(|l| !l.is_empty()) {
            assert!(
                (20..=80).contains(&line.len()),
                "{}",
                String::from_utf8_lossy(line)
            );
        }
    }
}
