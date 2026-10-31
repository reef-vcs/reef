//! Reading what a shell tool call ran. Not a shell parser: enough to see
//! `reef ws run` in `cd x && reef ws run -- sh -c '...'` or `git` in
//! `"$(git log)"`, which is all the benchmarks ask of it.

/// Shell words of `command`, split into simple commands at `;`, `&`, `|`,
/// parentheses, backquotes, `$`, and newlines, with quotes stripped from
/// each word's ends.
pub fn simple_commands(command: &str) -> Vec<Vec<&str>> {
    command
        .split([';', '&', '|', '(', ')', '`', '$', '\n'])
        .map(|segment| {
            segment
                .split_whitespace()
                .map(|word| word.trim_matches(['\'', '"', '\\']))
                .filter(|word| !word.is_empty())
                .collect::<Vec<_>>()
        })
        .filter(|words| !words.is_empty())
        .collect()
}

/// A simple command's words from the program it runs: past variable
/// assignments, wrappers such as `sudo` and `timeout 60`, and `sh -c`.
pub fn program<'s, 'a>(mut words: &'s [&'a str]) -> &'s [&'a str] {
    loop {
        words = match words {
            [word, rest @ ..] if word.contains('=') && !word.starts_with('-') => rest,
            [
                "sudo" | "env" | "time" | "exec" | "command" | "nohup",
                rest @ ..,
            ]
            | ["timeout", _, rest @ ..] => rest,
            ["sh" | "bash" | "zsh", flag, rest @ ..]
                if flag.starts_with('-') && flag.ends_with('c') =>
            {
                rest
            }
            _ => return words,
        };
    }
}

/// Whether `word` runs `name`, by name or path.
pub fn program_is(word: &str, name: &str) -> bool {
    word.rsplit('/').next() == Some(name)
}

/// The words that are not options or option values, up to a `--`.
pub fn positional<'a>(words: &[&'a str], value_options: &[&str]) -> Vec<&'a str> {
    let mut out = Vec::new();
    let mut words = words.iter();
    while let Some(word) = words.next() {
        if *word == "--" {
            break;
        }
        if word.starts_with('-') {
            if !word.contains('=') && value_options.contains(word) {
                words.next();
            }
            continue;
        }
        out.push(*word);
    }
    out
}

/// For each simple command in `command` that runs `name`, the words after
/// it.
pub fn invocations<'a>(command: &'a str, name: &str) -> Vec<Vec<&'a str>> {
    simple_commands(command)
        .iter()
        .filter_map(|words| match program(words) {
            [first, rest @ ..] if program_is(first, name) => Some(rest.to_vec()),
            _ => None,
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn finds_programs_behind_wrappers() {
        assert_eq!(
            invocations("cd x && REEF_OUTPUT=text reef status", "reef"),
            [vec!["status"]]
        );
        assert_eq!(
            invocations("bash -lc 'git log -1'; echo git", "git"),
            [vec!["log", "-1"]]
        );
        assert_eq!(invocations("cat reef/git", "git"), Vec::<Vec<&str>>::new());
    }
}
