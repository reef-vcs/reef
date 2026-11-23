//! The shell's links and which `reef` starts a daemon (R3.39).

use std::path::{Path, PathBuf};

use reef_client::web::WebPath;

/// The scheme and authority every deep link starts with.
pub const PREFIX: &str = "reef://local";
const MAX_LINK_BYTES: usize = 300;

/// `reef://local/<web path>`: a page of the local daemon's web UI to open in
/// the window, and nothing else. No user, port, query, fragment or escape:
/// the path is a [`WebPath`], so a link from any page only navigates the
/// person's own window on the daemon's origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DeepLink(pub WebPath);

impl DeepLink {
    /// `url`, if it is one.
    pub fn parse(url: &str) -> Option<DeepLink> {
        if url.len() > MAX_LINK_BYTES {
            return None;
        }
        let path = url.strip_prefix(PREFIX)?;
        WebPath::parse(path).map(DeepLink)
    }
}

/// Where a `reef` someone installed lives, for an app started from Finder,
/// which gets no shell `PATH`: Homebrew on Apple silicon and Intel,
/// `install.sh`, and `cargo install`.
pub fn installed_dirs(home: &Path) -> Vec<PathBuf> {
    vec![
        PathBuf::from("/opt/homebrew/bin"),
        PathBuf::from("/usr/local/bin"),
        home.join(".local/bin"),
        home.join(".cargo/bin"),
    ]
}

/// The `reef` whose `reef daemon start` starts a daemon: an installed one
/// first, else the app's own (`bundled`, the directory holding it). An
/// installed CLI starts its own daemon whenever none runs, so a newer
/// bundled daemon must not migrate the state directory under it (an older
/// daemon refuses a newer schema).
pub fn starter(
    installed: &[PathBuf],
    bundled: Option<&Path>,
    exists: impl Fn(&Path) -> bool,
) -> Option<PathBuf> {
    installed
        .iter()
        .map(|dir| dir.join("reef"))
        .chain(bundled.map(|dir| dir.join("reef")))
        .find(|reef| exists(reef))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deep_links() {
        for (link, path) in [
            ("reef://local", ""),
            ("reef://local/", "/"),
            (
                "reef://local/repos/a3dda866c7e8f343/items/help:a3dda866c7e8f343:cmt_1",
                "/repos/a3dda866c7e8f343/items/help:a3dda866c7e8f343:cmt_1",
            ),
            (
                "reef://local/repos/r/proposals/abc",
                "/repos/r/proposals/abc",
            ),
        ] {
            assert_eq!(
                DeepLink::parse(link).map(|link| link.0.as_str().to_owned()),
                Some(path.to_owned()),
                "{link}"
            );
        }
        for bad in [
            "reef://other/repos",
            "reef://local@evil/x",
            "reef://local:8080/x",
            "reef://localhost/x",
            "reef://local/a?x=1",
            "reef://local/a#b",
            "reef://local/a/../b",
            "reef://local/%2e%2e",
            "reef://local//evil.example",
            "http://local/x",
            "REEF://local/x",
        ] {
            assert_eq!(DeepLink::parse(bad), None, "{bad}");
        }
        assert_eq!(
            DeepLink::parse(&format!("reef://local/{}", "a".repeat(300))),
            None
        );
    }

    #[test]
    fn an_installed_reef_starts_the_daemon_before_the_apps() {
        let installed = installed_dirs(Path::new("/Users/p"));
        let bundled = Path::new("/Applications/Reef.app/Contents/MacOS");
        let brew =
            |path: &Path| path == Path::new("/opt/homebrew/bin/reef") || path.starts_with(bundled);
        assert_eq!(
            starter(&installed, Some(bundled), brew),
            Some(PathBuf::from("/opt/homebrew/bin/reef"))
        );
        let only_app = |path: &Path| path.starts_with(bundled);
        assert_eq!(
            starter(&installed, Some(bundled), only_app),
            Some(bundled.join("reef"))
        );
        assert_eq!(starter(&installed, None, |_| false), None);
    }
}
