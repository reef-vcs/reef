//! The pull-request hint (#258): from a remote's push URL, the host and
//! repository, and for hosts Reef knows the command that opens a pull
//! request (as an argv, never a shell line: the title and body come from a
//! change's description) and a page that does.
//!
//! `gh pr create` and `glab mr create` both get `--repo` and the head
//! branch: an agent runs `next` from a Reef workspace, which has no `.git`,
//! so neither can infer them.

use std::fmt::Write as _;

use reef_api::v1;

/// The most characters of a pull request's title.
pub const MAX_TITLE_CHARS: usize = 256;

/// Self-hosted hosts the daemon's `[publish]` names.
#[derive(Clone, Copy, Debug, Default)]
pub struct HostLists<'a> {
    /// Treated as GitHub.
    pub github: &'a [String],
    /// Treated as GitLab.
    pub gitlab: &'a [String],
}

/// A remote URL's host and path, without user information or `.git`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RemoteLocation {
    /// The host, lowercase, without a port.
    pub host: String,
    /// The path's segments.
    pub segments: Vec<String>,
}

impl RemoteLocation {
    /// The path, `owner/name`.
    pub fn path(&self) -> String {
        self.segments.join("/")
    }
}

/// Parses `url` (`https://`, `ssh://`, `git://`, or scp-like `git@host:path`)
/// into its host and path; `None` for a local path, a file URL, or a path
/// with a segment outside `[A-Za-z0-9._-]`.
pub fn parse_remote(url: &str) -> Option<RemoteLocation> {
    let url = url.trim();
    let (authority, path) = if let Some((scheme, rest)) = url.split_once("://") {
        if !matches!(
            scheme,
            "https" | "http" | "ssh" | "git" | "git+ssh" | "ssh+git"
        ) {
            return None;
        }
        rest.split_once('/')?
    } else {
        // scp-like: `[user@]host:path`, with no `/` before the colon.
        let (authority, path) = url.split_once(':')?;
        if authority.contains('/') || authority.is_empty() {
            return None;
        }
        (authority, path)
    };
    let host = authority
        .rsplit_once('@')
        .map_or(authority, |(_, host)| host);
    let host = host.split(':').next()?.to_ascii_lowercase();
    let host_ok = !host.is_empty()
        && host
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-'));
    if !host_ok {
        return None;
    }
    let path = path.trim_matches('/');
    let path = path.strip_suffix(".git").unwrap_or(path);
    let segments: Vec<String> = path.split('/').map(str::to_owned).collect();
    let segment_ok = |segment: &String| {
        !segment.is_empty()
            && segment != "."
            && segment != ".."
            && segment
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
    };
    if segments.len() < 2 || !segments.iter().all(segment_ok) {
        return None;
    }
    Some(RemoteLocation { host, segments })
}

/// A description's pull-request title (its first line, trimmed, at most
/// [`MAX_TITLE_CHARS`]) and body (what follows the first blank line, with
/// trailing whitespace trimmed), without control characters but newline
/// and tab: `next` reaches a terminal, where an escape sequence would act.
pub fn title_and_body(description: &str) -> (String, String) {
    let description: String = description
        .chars()
        .filter(|c| !c.is_control() || matches!(c, '\n' | '\t'))
        .collect();
    let mut lines = description.lines();
    let title: String = lines
        .next()
        .unwrap_or_default()
        .trim()
        .chars()
        .take(MAX_TITLE_CHARS)
        .collect();
    let rest: Vec<&str> = lines.collect();
    let start = rest
        .iter()
        .position(|line| line.trim().is_empty())
        .map_or(rest.len(), |blank| blank + 1);
    let body = rest[start.min(rest.len())..].join("\n");
    (title, body.trim_end().to_owned())
}

/// The hint for `branch` on the remote at `push_url`; `None` when the URL
/// names no host Reef can parse.
pub fn hint(
    push_url: &str,
    branch: &str,
    base: Option<&str>,
    description: &str,
    lists: HostLists<'_>,
) -> Option<v1::PullRequestHint> {
    let location = parse_remote(push_url)?;
    let host = location.host.clone();
    let path = location.path();
    let (title, body) = title_and_body(description);
    // A listed `host:port` matches the host: the push URL's port is git's
    // (often ssh's), not the web UI's.
    let listed = |list: &[String]| {
        list.iter().any(|name| {
            let name = name
                .rsplit_once(':')
                .map_or(name.as_str(), |(bare, _)| bare);
            name.eq_ignore_ascii_case(&host)
        })
    };
    let base_owned = base.map(str::to_owned).unwrap_or_default();
    let mut hint = v1::PullRequestHint {
        host_kind: v1::GitHostKind::Unspecified.into(),
        host: host.clone(),
        repository: path.clone(),
        base: base_owned.clone(),
        argv: Vec::new(),
        url: String::new(),
    };
    if host == "github.com" || listed(lists.github) {
        hint.host_kind = v1::GitHostKind::Github.into();
        let repo = if host == "github.com" {
            path.clone()
        } else {
            format!("{host}/{path}")
        };
        let mut argv = vec![
            "gh".to_owned(),
            "pr".to_owned(),
            "create".to_owned(),
            "--repo".to_owned(),
            repo,
            "--head".to_owned(),
            branch.to_owned(),
        ];
        if !base_owned.is_empty() {
            argv.extend(["--base".to_owned(), base_owned.clone()]);
        }
        argv.extend(["--title".to_owned(), title, "--body".to_owned(), body]);
        hint.argv = argv;
        hint.url = if base_owned.is_empty() {
            format!("https://{host}/{path}/pull/new/{branch}")
        } else {
            format!("https://{host}/{path}/compare/{base_owned}...{branch}?expand=1")
        };
    } else if host == "gitlab.com" || listed(lists.gitlab) {
        hint.host_kind = v1::GitHostKind::Gitlab.into();
        let repo = if host == "gitlab.com" {
            path.clone()
        } else {
            format!("https://{host}/{path}")
        };
        let mut argv = vec![
            "glab".to_owned(),
            "mr".to_owned(),
            "create".to_owned(),
            "--repo".to_owned(),
            repo,
            "--source-branch".to_owned(),
            branch.to_owned(),
        ];
        if !base_owned.is_empty() {
            argv.extend(["--target-branch".to_owned(), base_owned.clone()]);
        }
        argv.extend([
            "--title".to_owned(),
            title,
            "--description".to_owned(),
            body,
            "--yes".to_owned(),
        ]);
        hint.argv = argv;
        let mut url = format!(
            "https://{host}/{path}/-/merge_requests/new?merge_request%5Bsource_branch%5D={branch}"
        );
        if !base_owned.is_empty() {
            let _ = write!(url, "&merge_request%5Btarget_branch%5D={base_owned}");
        }
        hint.url = url;
    } else if host == "bitbucket.org" {
        hint.host_kind = v1::GitHostKind::Bitbucket.into();
        hint.url = format!("https://bitbucket.org/{path}/pull-requests/new?source={branch}");
        if !base_owned.is_empty() {
            let _ = write!(hint.url, "&dest={base_owned}");
        }
    } else if host == "codeberg.org" {
        hint.host_kind = v1::GitHostKind::Forgejo.into();
        hint.url = if base_owned.is_empty() {
            format!("https://codeberg.org/{path}/compare/{branch}")
        } else {
            format!("https://codeberg.org/{path}/compare/{base_owned}...{branch}")
        };
    }
    Some(hint)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn github(url: &str, base: Option<&str>) -> v1::PullRequestHint {
        hint(
            url,
            "reef/abc",
            base,
            "Fix it\n\nWhy.\n",
            HostLists::default(),
        )
        .unwrap()
    }

    #[test]
    fn github_forms() {
        for url in [
            "https://github.com/o/r.git",
            "https://user:token@github.com/o/r",
            "ssh://git@github.com/o/r.git",
            "git@github.com:o/r.git",
        ] {
            let found = github(url, Some("main"));
            assert_eq!(found.host_kind, i32::from(v1::GitHostKind::Github), "{url}");
            assert_eq!(found.repository, "o/r", "{url}");
            assert_eq!(
                found.argv,
                [
                    "gh", "pr", "create", "--repo", "o/r", "--head", "reef/abc", "--base", "main",
                    "--title", "Fix it", "--body", "Why."
                ],
                "{url}"
            );
            assert_eq!(
                found.url,
                "https://github.com/o/r/compare/main...reef/abc?expand=1"
            );
            assert!(!found.host.contains("token"));
        }
        let no_base = github("git@github.com:o/r.git", None);
        assert!(!no_base.argv.contains(&"--base".to_owned()));
        assert_eq!(no_base.url, "https://github.com/o/r/pull/new/reef/abc");
    }

    #[test]
    fn self_hosted_and_others() {
        let lists = HostLists {
            github: &["ghe.example.com".to_owned()],
            gitlab: &["git.example.com".to_owned()],
        };
        let ghe = hint("https://ghe.example.com/o/r", "b", None, "T", lists).unwrap();
        assert_eq!(ghe.argv[4], "ghe.example.com/o/r");
        let gitlab = hint(
            "git@gitlab.com:group/sub/r.git",
            "b",
            Some("main"),
            "T",
            lists,
        )
        .unwrap();
        assert_eq!(gitlab.host_kind, i32::from(v1::GitHostKind::Gitlab));
        assert_eq!(gitlab.argv[4], "group/sub/r");
        assert!(gitlab.url.contains("source_branch%5D=b"));
        let hosted = hint("https://git.example.com/g/r", "b", None, "T", lists).unwrap();
        assert_eq!(hosted.argv[4], "https://git.example.com/g/r");
        let bitbucket = hint("git@bitbucket.org:o/r.git", "b", Some("main"), "T", lists).unwrap();
        assert_eq!(bitbucket.argv, Vec::<String>::new());
        assert_eq!(
            bitbucket.url,
            "https://bitbucket.org/o/r/pull-requests/new?source=b&dest=main"
        );
        let codeberg = hint("https://codeberg.org/o/r", "b", Some("main"), "T", lists).unwrap();
        assert_eq!(codeberg.url, "https://codeberg.org/o/r/compare/main...b");
        let unknown = hint("https://example.org/o/r", "b", None, "T", lists).unwrap();
        assert_eq!(unknown.host_kind, i32::from(v1::GitHostKind::Unspecified));
        assert!(unknown.argv.is_empty() && unknown.url.is_empty());
    }

    #[test]
    fn unparseable() {
        for url in [
            "/tmp/remote.git",
            "file:///tmp/remote.git",
            "https://github.com/o/r;rm -rf",
            "https://github.com/only",
            "https://github.com/o/$(x)",
        ] {
            assert!(parse_remote(url).is_none(), "{url}");
        }
    }

    #[test]
    fn titles_and_bodies() {
        assert_eq!(
            title_and_body("  Title \nmore\n\nBody line\n\"quoted\" $x\n\n"),
            ("Title".to_owned(), "Body line\n\"quoted\" $x".to_owned())
        );
        assert_eq!(title_and_body("Only"), ("Only".to_owned(), String::new()));
        let long = "x".repeat(300);
        assert_eq!(title_and_body(&long).0.len(), MAX_TITLE_CHARS);
        // An escape sequence never reaches the terminal through `next`.
        assert_eq!(
            title_and_body("Fix \u{1b}[2Jit\n\nBody\twith\u{7} tab\n"),
            ("Fix [2Jit".to_owned(), "Body\twith tab".to_owned())
        );
    }

    #[test]
    fn listed_hosts_with_ports() {
        let lists = HostLists {
            github: &["ghe.example.com:8443".to_owned()],
            gitlab: &[],
        };
        let ghe = hint(
            "ssh://git@ghe.example.com:2222/o/r.git",
            "b",
            None,
            "T",
            lists,
        )
        .unwrap();
        assert_eq!(ghe.host_kind, i32::from(v1::GitHostKind::Github));
    }
}
