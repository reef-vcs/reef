//! Symbol strings as `reef diff --symbols` shows them (R2.15): without the
//! scheme and package, and without the leading namespaces that spell the
//! file's path, which the line above already names.
//!
//! The client links no `reef-symbols` (`crate_graph` keeps it out), so this
//! reads SCIP's grammar only as far as it needs: four space-separated fields
//! with doubled spaces as literal ones, then descriptors.

/// Whether SCIP writes `c` in a bare name.
fn is_identifier_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, '_' | '+' | '-' | '$')
}

/// `name` as SCIP writes a namespace descriptor: bare, or in backticks with
/// backticks doubled, then `/`.
fn namespace(name: &str) -> String {
    if !name.is_empty() && name.chars().all(is_identifier_char) {
        format!("{name}/")
    } else {
        format!("`{}`/", name.replace('`', "``"))
    }
}

/// The descriptors of a global symbol string: what follows its scheme and
/// three package fields. `None` for a local symbol or one that does not
/// have them.
fn descriptors(symbol: &str) -> Option<&str> {
    if symbol.starts_with("local ") {
        return None;
    }
    let bytes = symbol.as_bytes();
    let (mut fields, mut i) = (0, 0);
    while fields < 4 {
        match bytes.get(i)? {
            b' ' if bytes.get(i + 1) == Some(&b' ') => i += 2,
            b' ' => {
                fields += 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    symbol.get(i..)
}

/// `symbol` short: its descriptors without the leading namespaces that
/// spell `path`'s components (or its directory's, as Go's tags do) and
/// without a final `.`. A symbol that spells no path, such as a
/// package-scoped SCIP one, keeps its namespaces; a local one is unchanged.
pub fn short_symbol(symbol: &str, path: &str) -> String {
    let Some(mut rest) = descriptors(symbol) else {
        return symbol.to_owned();
    };
    let parts: Vec<&str> = path.split('/').filter(|p| !p.is_empty()).collect();
    for spelled in [&parts[..], &parts[..parts.len().saturating_sub(1)]] {
        let prefix: String = spelled.iter().map(|p| namespace(p)).collect();
        if !spelled.is_empty()
            && let Some(stripped) = rest.strip_prefix(prefix.as_str())
            && !stripped.is_empty()
        {
            rest = stripped;
            break;
        }
    }
    rest.strip_suffix('.').unwrap_or(rest).to_owned()
}

/// Bytes after which a suffix of a symbol string may begin.
const BOUNDARIES: &[u8] = b" /#.`";

/// Whether an `--expand` entry names a change, as the daemon decides it:
/// it equals the change's symbol, old symbol or name, or is a suffix of its
/// symbol beginning after a descriptor boundary, with or without the final
/// `.`.
pub fn matches_expand(entry: &str, symbol: &str, old_symbol: Option<&str>, name: &str) -> bool {
    if entry.is_empty() {
        return false;
    }
    if entry == symbol || entry == name || old_symbol == Some(entry) {
        return true;
    }
    let dotted = format!("{entry}.");
    [entry, dotted.as_str()].into_iter().any(|suffix| {
        symbol.len() > suffix.len()
            && symbol.ends_with(suffix)
            && BOUNDARIES.contains(&symbol.as_bytes()[symbol.len() - suffix.len() - 1])
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Test 22 of R2.15.
    #[test]
    fn short_symbols_drop_what_the_path_says() {
        assert_eq!(
            short_symbol(
                "reef-tags . . . src/`parser.py`/Parser#parse().",
                "src/parser.py"
            ),
            "Parser#parse()"
        );
        assert_eq!(
            short_symbol(
                "reef-tags . . . pkg/store/Store#Get().",
                "pkg/store/store.go"
            ),
            "Store#Get()"
        );
        assert_eq!(
            short_symbol("reef-tags . . . `a.py`/LIMIT.", "a.py"),
            "LIMIT"
        );
        // Package-scoped: no namespace spells the path.
        assert_eq!(
            short_symbol(
                "scip-go gomod example.com/m v1 `example.com/m/x`/T#M().",
                "x/a.go"
            ),
            "`example.com/m/x`/T#M()"
        );
        // A scheme field with a doubled space.
        assert_eq!(short_symbol("my  scheme . . . `a.py`/f().", "a.py"), "f()");
        assert_eq!(short_symbol("local 4", "a.py"), "local 4");
    }
}
