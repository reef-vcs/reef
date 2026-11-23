//! A path of the web UI (R2.18, R3.39): the one grammar the daemon's
//! `CreateWebSession` and the desktop shell's links share.

use std::fmt;

/// The longest path a session opens at.
pub const MAX_PATH_BYTES: usize = 256;

/// A path of the web UI to open: empty for its start page, else `/` and
/// letters, digits and `/._-:`, with no empty, `.` or `..` segment after the
/// first `/`, at most [`MAX_PATH_BYTES`]. It has no query, fragment, host or
/// escape, so it names a page of the daemon's own origin and nothing else.
/// `:` is in it for inbox item keys (`/repos/<repo>/items/<key>`).
#[derive(Clone, Debug, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct WebPath(String);

impl WebPath {
    /// The start page.
    pub fn start() -> WebPath {
        WebPath(String::new())
    }

    /// `path`, if it is one.
    pub fn parse(path: &str) -> Option<WebPath> {
        is_app_path(path).then(|| WebPath(path.to_owned()))
    }

    /// An inbox item's page (`web/src/views/ItemLink.tsx`), which goes on
    /// to the item's own: what a banner, a deep link and a push open.
    /// `None` for a repository or key that cannot be in a path.
    pub fn item(repo: &str, key: &str) -> Option<WebPath> {
        WebPath::parse(&format!("/repos/{repo}/items/{key}"))
    }

    /// The path, for `CreateWebSession`.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl fmt::Display for WebPath {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(&self.0)
    }
}

/// Whether `path` is a [`WebPath`].
pub fn is_app_path(path: &str) -> bool {
    if path.is_empty() {
        return true;
    }
    let Some(rest) = path.strip_prefix('/') else {
        return false;
    };
    path.len() <= MAX_PATH_BYTES
        && rest
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/._-:".contains(&b))
        && (rest.is_empty()
            || rest
                .split('/')
                .all(|segment| !matches!(segment, "" | "." | "..")))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_paths() {
        for good in [
            "",
            "/",
            "/approve/apr_0123456789abcdef",
            "/passkeys",
            "/repos/abc/smartlog",
            "/repos/a3dda866c7e8f343/items/help:a3dda866c7e8f343:cmt_12",
        ] {
            assert!(is_app_path(good), "{good}");
        }
        for bad in [
            "approve",
            "//evil.example",
            "/a//b",
            "/a/../b",
            "/./a",
            "/a?x=1",
            "/a#b",
            "/a b",
            "/a%2f",
            "/a/",
            "\\evil",
            "http://evil.example",
        ] {
            assert!(!is_app_path(bad), "{bad}");
        }
        assert!(!is_app_path(&format!("/{}", "a".repeat(256))));
    }

    #[test]
    fn an_item_is_a_path() {
        let path = WebPath::item("a3dda866c7e8f343", "proposal:a3dda866c7e8f343:abc").unwrap();
        assert_eq!(
            path.as_str(),
            "/repos/a3dda866c7e8f343/items/proposal:a3dda866c7e8f343:abc"
        );
        assert!(WebPath::item("r", "key with space").is_none());
    }
}
