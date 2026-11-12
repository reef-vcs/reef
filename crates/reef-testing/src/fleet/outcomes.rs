//! Each test's result, read from a build log, so a ticket run five at once
//! can be compared with the same patch run alone (gate G9).
//!
//! The parser knows the formats the pinned repositories print: cargo's
//! libtest lines, `go test`'s per-package lines, pytest's `-rA` summary,
//! TAP (`node --test`), and Vitest's verbose reporter. It reads every line
//! with every format; a line one format does not recognize is skipped.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// One test's result.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    /// It passed.
    Pass,
    /// It failed.
    Fail,
    /// It was skipped or ignored.
    Skip,
}

/// Every test result in `log`, keyed by a name that includes its format, so
/// two formats cannot collide.
pub fn parse(log: &str) -> BTreeMap<String, Status> {
    let log = strip_ansi(log);
    let mut results = BTreeMap::new();
    // libtest prints `Running unittests src/lib.rs (target/.../deps/x-hash)`
    // before each binary's tests; the binary's crate name keeps two crates'
    // `tests::it_works` apart.
    let mut binary = String::new();
    for line in log.lines() {
        let line = line.trim_end();
        if let Some(name) = cargo_binary(line) {
            binary = name;
            continue;
        }
        let found = if let Some((name, status)) = cargo(line) {
            Some((format!("cargo:{binary}:{name}"), status))
        } else if let Some((name, status)) = nextest(line) {
            Some((format!("nextest:{name}"), status))
        } else if let Some((name, status)) = go(line) {
            Some((format!("go:{name}"), status))
        } else if let Some((name, status)) = pytest(line) {
            Some((format!("pytest:{name}"), status))
        } else if let Some((name, status)) = tap(line) {
            Some((format!("tap:{name}"), status))
        } else {
            vitest(line).map(|(name, status)| (format!("vitest:{name}"), status))
        };
        // Some suites repeat a test's full name (Vitest's `describe.each`):
        // the second is `name #2`, in the order they print.
        if let Some((name, status)) = found {
            let mut key = name.clone();
            let mut n = 1;
            while results.contains_key(&key) {
                n += 1;
                key = format!("{name} #{n}");
            }
            results.insert(key, status);
        }
    }
    results
}

/// `log` without ANSI escape sequences (`ESC [ ... letter`), which a
/// colored tool writes even when its output is a file.
pub fn strip_ansi(log: &str) -> String {
    let mut out = String::with_capacity(log.len());
    let mut chars = log.chars().peekable();
    while let Some(c) = chars.next() {
        if c == '\u{1b}' && chars.peek() == Some(&'[') {
            chars.next();
            for c in chars.by_ref() {
                if c.is_ascii_alphabetic() {
                    break;
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// nextest: `        PASS [   0.011s] (16/28) crate tests::name`, `FAIL`,
/// `SKIP`, and `TIMEOUT`, `SIGSEGV` and the like as failures. `SLOW` and
/// `START` lines are progress, not results.
fn nextest(line: &str) -> Option<(String, Status)> {
    let line = line.trim_start();
    let (word, rest) = line.split_once(' ')?;
    let status = match word {
        "PASS" | "LEAK" => Status::Pass,
        "SKIP" => Status::Skip,
        "FAIL" | "TIMEOUT" | "ABORT" | "SIGSEGV" | "SIGABRT" | "SIGKILL" => Status::Fail,
        _ => return None,
    };
    let rest = rest.trim_start().strip_prefix('[')?;
    let (_, rest) = rest.split_once("] ")?;
    // The `(n/m)` counter is absent from some output modes.
    let rest = match rest.strip_prefix('(').and_then(|r| r.split_once(") ")) {
        Some((_, after)) => after,
        None => rest,
    };
    let name = rest.trim();
    (!name.is_empty()).then(|| (name.to_owned(), status))
}

/// `     Running unittests src/lib.rs (target/debug/deps/c1-0123abcd)` or
/// `     Running tests/it.rs (target/debug/deps/it-0123abcd)`: the binary's
/// name without its hash.
fn cargo_binary(line: &str) -> Option<String> {
    let rest = line.trim_start().strip_prefix("Running ")?;
    let path = rest.rsplit_once('(')?.1.strip_suffix(')')?;
    let file = path.rsplit('/').next()?;
    let name = match file.rsplit_once('-') {
        Some((name, hash)) if hash.chars().all(|c| c.is_ascii_hexdigit()) => name,
        _ => file,
    };
    Some(name.to_owned())
}

/// `test tests::it_works ... ok`, `... FAILED`, `... ignored` (and
/// `ignored, <reason>`).
fn cargo(line: &str) -> Option<(String, Status)> {
    let rest = line.strip_prefix("test ")?;
    let (name, result) = rest.split_once(" ... ")?;
    let status = match result.split([',', ' ']).next()? {
        "ok" => Status::Pass,
        "FAILED" => Status::Fail,
        "ignored" => Status::Skip,
        _ => return None,
    };
    Some((name.trim().to_owned(), status))
}

/// `ok  \tgithub.com/cli/cli/pkg/x\t0.123s`, `FAIL\tgithub.com/x\t0.1s`,
/// `?   \tgithub.com/x\t[no test files]` (a package, not a test).
fn go(line: &str) -> Option<(String, Status)> {
    let mut fields = line.split('\t');
    let head = fields.next()?.trim();
    let package = fields.next()?.trim();
    if package.is_empty() || package.contains(' ') || !package.contains('/') {
        return None;
    }
    let status = match head {
        "ok" => Status::Pass,
        "FAIL" => Status::Fail,
        _ => return None,
    };
    Some((package.to_owned(), status))
}

/// pytest's `-rA` short summary: `PASSED tests/test_x.py::test_y`,
/// `FAILED tests/test_x.py::test_y - AssertionError`, `SKIPPED [1] ...`.
fn pytest(line: &str) -> Option<(String, Status)> {
    let (word, rest) = line.split_once(' ')?;
    let status = match word {
        "PASSED" | "XFAIL" => Status::Pass,
        "FAILED" | "ERROR" | "XPASS" => Status::Fail,
        "SKIPPED" => Status::Skip,
        _ => return None,
    };
    let name = rest.split(" - ").next()?.trim();
    (name.contains("::") || word == "SKIPPED").then(|| (name.to_owned(), status))
}

/// TAP, as `node --test` prints it: `ok 1 - name`, `not ok 2 - name`, and
/// `# SKIP` directives. Subtests are indented; their names are kept.
fn tap(line: &str) -> Option<(String, Status)> {
    let line = line.trim_start();
    let (failed, rest) = match line.strip_prefix("not ok ") {
        Some(rest) => (true, rest),
        None => (false, line.strip_prefix("ok ")?),
    };
    let (number, name) = rest.split_once(" - ")?;
    if !number.chars().all(|c| c.is_ascii_digit()) {
        return None;
    }
    let (name, directive) = match name.split_once(" # ") {
        Some((name, directive)) => (name, Some(directive)),
        None => (name, None),
    };
    let status = if directive.is_some_and(|d| d.to_ascii_uppercase().starts_with("SKIP")) {
        Status::Skip
    } else if failed {
        Status::Fail
    } else {
        Status::Pass
    };
    Some((name.trim().to_owned(), status))
}

/// Vitest's verbose reporter: `   ✓ src/a.test.ts > group > name 3ms`,
/// `   × ...`, `   ↓ ...` (skipped).
fn vitest(line: &str) -> Option<(String, Status)> {
    let line = line.trim_start();
    let mut chars = line.chars();
    let status = match chars.next()? {
        '✓' => Status::Pass,
        '×' | '✗' => Status::Fail,
        '↓' => Status::Skip,
        _ => return None,
    };
    let rest = chars.as_str().trim();
    if !rest.contains(" > ") {
        // A file's line (`✓ src/a.test.ts (5 tests) 12ms`), not a test.
        return None;
    }
    // Drop a trailing duration (`12ms`, `1.2s`), and only a duration: a
    // name may end in a word like `items`.
    let is_duration = |word: &str| {
        let number = word
            .strip_suffix("ms")
            .or_else(|| word.strip_suffix('s'))
            .unwrap_or("");
        !number.is_empty()
            && number.chars().all(|c| c.is_ascii_digit() || c == '.')
            && number.chars().next().is_some_and(|c| c.is_ascii_digit())
    };
    let name = match rest.rsplit_once(' ') {
        Some((name, last)) if is_duration(last) => name,
        _ => rest,
    };
    // Some test names end in a space, which a duration's separator hides:
    // trim either way so both spellings match.
    Some((name.trim_end().to_owned(), status))
}

/// How many compilation units cargo compiled (its `Compiling` lines).
/// `None` when the log has no cargo output at all.
pub fn units_compiled(log: &str) -> Option<u32> {
    let log = strip_ansi(log);
    let mut cargo = false;
    let mut count = 0_u32;
    for line in log.lines() {
        let line = line.trim_start();
        if line.starts_with("Compiling ") {
            count += 1;
            cargo = true;
        } else if line.starts_with("Finished ") || line.starts_with("Fresh ") {
            cargo = true;
        }
    }
    cargo.then_some(count)
}

/// Whether the log shows a write failing for disk.
pub fn enospc(log: &str) -> bool {
    // Only an error line: a test's name or its expected output may mention
    // either string.
    strip_ansi(log).lines().any(|line| {
        let lower = line.to_ascii_lowercase();
        (line.contains("No space left on device") || line.contains("ENOSPC"))
            && (lower.contains("error") || lower.contains("os error 28"))
            && cargo(line).is_none()
            && pytest(line).is_none()
            && tap(line).is_none()
            && nextest(line).is_none()
    })
}

/// How two runs' outcomes differ: tests in one and not the other, and tests
/// whose status changed. Empty when they match.
pub fn differences(
    reference: &BTreeMap<String, Status>,
    other: &BTreeMap<String, Status>,
) -> Vec<String> {
    let mut out = Vec::new();
    for (name, status) in reference {
        match other.get(name) {
            None => out.push(format!("{name}: missing (was {status:?})")),
            Some(now) if now != status => out.push(format!("{name}: {status:?} -> {now:?}")),
            Some(_) => {}
        }
    }
    for name in other.keys().filter(|name| !reference.contains_key(*name)) {
        out.push(format!("{name}: new"));
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_cargo_with_the_binary_name() {
        let log = "\
   Compiling c1 v0.1.0 (/ws/c1)
   Compiling c2 v0.1.0 (/ws/c2)
    Finished `test` profile [unoptimized + debuginfo] target(s) in 1.20s
     Running unittests src/lib.rs (target/debug/deps/c1-0f1e2d3c4b5a6978)
test tests::value_is_one ... ok
test tests::slow ... ignored, takes a minute
     Running unittests src/lib.rs (target/debug/deps/c2-a1b2c3d4e5f60718)
test tests::value_is_one ... FAILED
";
        let results = parse(log);
        assert_eq!(results["cargo:c1:tests::value_is_one"], Status::Pass);
        assert_eq!(results["cargo:c1:tests::slow"], Status::Skip);
        assert_eq!(results["cargo:c2:tests::value_is_one"], Status::Fail);
        assert_eq!(units_compiled(log), Some(2));
    }

    #[test]
    fn reads_go_packages() {
        let log = "ok  \tgithub.com/cli/cli/v2/pkg/text\t0.012s\nFAIL\tgithub.com/cli/cli/v2/api\t0.2s\n?   \tgithub.com/cli/cli/v2/cmd/gh\t[no test files]\nok  \tgithub.com/cli/cli/v2/pkg/x\t(cached)\n";
        let results = parse(log);
        assert_eq!(results.len(), 3);
        assert_eq!(results["go:github.com/cli/cli/v2/api"], Status::Fail);
        assert_eq!(results["go:github.com/cli/cli/v2/pkg/x"], Status::Pass);
        assert_eq!(units_compiled(log), None);
    }

    #[test]
    fn reads_pytest_tap_and_vitest() {
        let log = "\
PASSED tests/test_a.py::test_one
FAILED tests/test_a.py::test_two - AssertionError: 1 != 2
ok 1 - adds
not ok 2 - subtracts
ok 3 - later # SKIP not yet
   ✓ src/a.test.ts > math > adds 3ms
   × src/a.test.ts > math > breaks 1.2s
 ✓ src/a.test.ts (2 tests) 12ms
";
        let results = parse(log);
        assert_eq!(results["pytest:tests/test_a.py::test_one"], Status::Pass);
        assert_eq!(results["pytest:tests/test_a.py::test_two"], Status::Fail);
        assert_eq!(results["tap:adds"], Status::Pass);
        assert_eq!(results["tap:subtracts"], Status::Fail);
        assert_eq!(results["tap:later"], Status::Skip);
        assert_eq!(results["vitest:src/a.test.ts > math > adds"], Status::Pass);
        assert_eq!(
            results["vitest:src/a.test.ts > math > breaks"],
            Status::Fail
        );
        assert_eq!(results.len(), 7);
    }

    #[test]
    fn a_trailing_space_in_a_name_matches_with_or_without_a_duration() {
        let fast = parse("   ✓ a.test.ts > on freedraw \n");
        let slow = parse("   ✓ a.test.ts > on freedraw  620ms\n");
        assert_eq!(fast, slow);
        assert!(fast.contains_key("vitest:a.test.ts > on freedraw"));
    }

    #[test]
    fn repeated_names_are_numbered() {
        let results = parse("   ✓ a.test.ts > same 1ms\n   × a.test.ts > same 2ms\n");
        assert_eq!(results["vitest:a.test.ts > same"], Status::Pass);
        assert_eq!(results["vitest:a.test.ts > same #2"], Status::Fail);
    }

    #[test]
    fn differences_name_missing_new_and_changed_tests() {
        let reference = parse("test a ... ok\ntest b ... ok\n");
        let other = parse("test a ... FAILED\ntest c ... ok\n");
        assert_eq!(
            differences(&reference, &other),
            vec![
                "cargo::a: Pass -> Fail".to_owned(),
                "cargo::b: missing (was Pass)".to_owned(),
                "cargo::c: new".to_owned(),
            ]
        );
        assert_eq!(differences(&reference, &reference), Vec::<String>::new());
    }

    #[test]
    fn sees_enospc_only_in_errors() {
        assert!(enospc(
            "error: failed to write: No space left on device (os error 28)"
        ));
        assert!(!enospc("all good"));
        assert!(!enospc("test fs::reports_ENOSPC_cleanly ... ok"));
        assert!(!enospc(
            "PASSED tests/test_disk.py::test_no_space_left_on_device"
        ));
    }

    #[test]
    fn strips_colors_and_reads_nextest() {
        let log = "\u{1b}[1m\u{1b}[32m   Compiling\u{1b}[0m c1 v0.1.0\n        \u{1b}[32mPASS\u{1b}[0m [   0.011s] (16/28) reef-testing fleet::a\n        FAIL [   1.2s] reef-lib tests::b\n        SLOW [> 60.000s] reef-vcs x\n";
        let results = parse(log);
        assert_eq!(results["nextest:reef-testing fleet::a"], Status::Pass);
        assert_eq!(results["nextest:reef-lib tests::b"], Status::Fail);
        assert_eq!(results.len(), 2);
        assert_eq!(units_compiled(log), Some(1));
    }

    #[test]
    fn vitest_keeps_words_that_end_in_s() {
        let results =
            parse("   ✓ a.test.ts > renders items\n   ✓ a.test.ts > renders forms 12ms\n");
        assert!(results.contains_key("vitest:a.test.ts > renders items"));
        assert!(results.contains_key("vitest:a.test.ts > renders forms"));
    }
}
