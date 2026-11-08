//! Path patterns: `globset` globs over repository-relative paths with `/`
//! separators, matched ASCII case-insensitively (S-95).
//!
//! `*` and `?` never cross `/`, `**` does, `{a,b}` alternates, and a
//! pattern that names a directory (`docs`) matches only that path, not its
//! contents (`docs/**` does).

use globset::{GlobBuilder, GlobSet, GlobSetBuilder};

/// One pattern compiled as the file's globs are, or why it does not
/// compile.
pub fn compile(pattern: &str) -> Result<globset::Glob, String> {
    if pattern.is_empty() {
        return Err("an empty pattern matches nothing".to_owned());
    }
    if pattern.starts_with('/') {
        return Err(format!(
            "{pattern:?} starts with `/`; patterns are relative to the repository's root"
        ));
    }
    GlobBuilder::new(pattern)
        .literal_separator(true)
        .case_insensitive(true)
        .backslash_escape(true)
        .empty_alternates(true)
        .build()
        .map_err(|error| error.kind().to_string())
}

/// A compiled list of patterns.
#[derive(Clone, Debug)]
pub struct Patterns {
    set: GlobSet,
    len: usize,
}

impl Patterns {
    /// `patterns`, each checked with [`compile`].
    pub fn new<'a>(patterns: impl IntoIterator<Item = &'a str>) -> Result<Patterns, String> {
        let mut builder = GlobSetBuilder::new();
        let mut len = 0;
        for pattern in patterns {
            builder.add(compile(pattern)?);
            len += 1;
        }
        let set = builder.build().map_err(|error| error.to_string())?;
        Ok(Patterns { set, len })
    }

    /// No patterns.
    pub fn empty() -> Patterns {
        Patterns {
            set: GlobSet::empty(),
            len: 0,
        }
    }

    /// Whether there are none.
    pub fn is_empty(&self) -> bool {
        self.len == 0
    }

    /// Whether some pattern matches `path`.
    pub fn matches(&self, path: &str) -> bool {
        self.len > 0 && self.set.is_match(path)
    }
}

/// `paths` and `except`: what a table applies to.
#[derive(Clone, Debug)]
pub struct PathFilter {
    /// The paths it covers; none for every path.
    pub paths: Patterns,
    /// The paths it leaves out.
    pub except: Patterns,
}

impl PathFilter {
    /// Whether `path` is covered: matched by `paths` (or `paths` is empty)
    /// and not by `except`.
    pub fn covers(&self, path: &str) -> bool {
        (self.paths.is_empty() || self.paths.matches(path)) && !self.except.matches(path)
    }

    /// Whether a table with this filter applies to a change touching
    /// `touched`: always with neither list, else when some touched path is
    /// covered.
    pub fn applies<'a>(&self, touched: impl IntoIterator<Item = &'a String>) -> bool {
        if self.paths.is_empty() && self.except.is_empty() {
            return true;
        }
        touched.into_iter().any(|path| self.covers(path))
    }

    /// The touched paths covered, in order.
    pub fn covered<'a>(&self, touched: &'a [String]) -> Vec<&'a String> {
        touched.iter().filter(|path| self.covers(path)).collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn patterns(list: &[&str]) -> Patterns {
        Patterns::new(list.iter().copied()).unwrap()
    }

    #[test]
    fn a_star_stays_in_its_directory_and_two_stars_do_not() {
        let star = patterns(&["crates/*.rs"]);
        assert!(star.matches("crates/a.rs"));
        assert!(!star.matches("crates/x/a.rs"));
        let stars = patterns(&["crates/**"]);
        assert!(stars.matches("crates/x/y/a.rs"));
        assert!(stars.matches("crates/a.rs"));
        let any = patterns(&["**/*.pem"]);
        assert!(any.matches("key.pem"));
        assert!(any.matches("a/b/key.pem"));
        assert!(patterns(&["{a,b}/*"]).matches("b/x"));
    }

    #[test]
    fn a_directory_name_matches_only_itself() {
        let docs = patterns(&["docs"]);
        assert!(docs.matches("docs"));
        assert!(!docs.matches("docs/a.md"));
    }

    #[test]
    fn matching_folds_ascii_case() {
        let secrets = patterns(&["secrets/**"]);
        assert!(secrets.matches("Secrets/a.pem"));
        assert!(secrets.matches("SECRETS/x/y"));
    }

    #[test]
    fn except_removes_paths_and_no_lists_apply_always() {
        let filter = PathFilter {
            paths: patterns(&["crates/**"]),
            except: patterns(&["crates/**/*.md"]),
        };
        assert!(filter.covers("crates/a/src/lib.rs"));
        assert!(!filter.covers("crates/a/README.md"));
        let touched = vec!["crates/a/README.md".to_owned()];
        assert!(!filter.applies(&touched));
        let always = PathFilter {
            paths: Patterns::empty(),
            except: Patterns::empty(),
        };
        assert!(always.applies(&Vec::new()));
    }

    #[test]
    fn bad_patterns_are_refused() {
        assert!(compile("a/[").is_err());
        assert!(compile("").is_err());
        assert!(compile("/abs").is_err());
    }
}
