//! Whether a trial succeeded, judged from the daemon's records and the
//! snapshot's contents rather than from what the agent says.

use serde::{Deserialize, Serialize};

use super::Scenario;

/// The fields of one `reef ws list --all -o json` entry the judge reads.
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct WorkspaceRecord {
    /// The workspace id, 16 hex digits.
    pub id: String,
    /// `active`, `released`, or `reclaimed`.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub state: String,
    /// The workspace's commit: after a release, its final snapshot.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub commit_id: String,
    /// The change id in hex.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub change_id: String,
    /// The change id as jj prints it (`k`–`z`).
    #[serde(default, deserialize_with = "null_as_empty")]
    pub change_id_reverse_hex: String,
    /// RFC 3339.
    #[serde(default, deserialize_with = "null_as_empty")]
    pub created_at: String,
}

/// A string field the daemon may send as `null`, as empty: one `null` must
/// not fail the whole list.
fn null_as_empty<'de, D: serde::Deserializer<'de>>(deserializer: D) -> Result<String, D::Error> {
    Ok(Option::<String>::deserialize(deserializer)?.unwrap_or_default())
}

/// The judge's finding.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    /// Whether the agent did what the prompt asked, as far as the daemon
    /// can tell.
    pub success: bool,
    /// The workspace the verdict is about, if one was identified.
    pub workspace: Option<String>,
    /// One line on why.
    pub reason: String,
}

impl Verdict {
    fn fail(workspace: Option<&str>, reason: impl Into<String>) -> Self {
        Verdict {
            success: false,
            workspace: workspace.map(str::to_owned),
            reason: reason.into(),
        }
    }
}

/// The shortest change id prefix scenario D accepts, as jj's shortest
/// unique prefixes are in a small repository's first changes.
const MIN_CHANGE_PREFIX: usize = 8;

/// Judges a trial. `final_text` is the agent's last answer, `workspaces`
/// what `reef ws list --all` reported after the agent exited, `base_readme`
/// the README before the agent started, and `readme_at` reads `README.md`
/// at a commit (`None` if the commit or the file is missing).
///
/// - A, B, C: the last non-empty line of the answer names a listed
///   workspace (the last listed id on it, so surrounding words are fine), and `README.md` in that workspace's commit is the base
///   README with something appended.
/// - D: the answer names the workspace `reef ws run` started the agent in
///   (the oldest listed) and its change, by hex or jj id, at least eight
///   characters of it.
pub fn judge(
    scenario: Scenario,
    final_text: &str,
    workspaces: &[WorkspaceRecord],
    base_readme: &str,
    readme_at: impl Fn(&str) -> Option<String>,
) -> Verdict {
    if scenario == Scenario::D {
        return judge_oriented(final_text, workspaces);
    }
    let Some(last_line) = final_text
        .lines()
        .rev()
        .find(|line| !line.trim().is_empty())
    else {
        return Verdict::fail(None, "the agent gave no answer");
    };
    // The last listed id on the line, so `Workspace: <id> (released).` counts.
    let named = last_line
        .split(|c: char| !c.is_ascii_alphanumeric())
        .rev()
        .find_map(|word| workspaces.iter().find(|ws| ws.id == word));
    let Some(workspace) = named else {
        return Verdict::fail(
            None,
            format!(
                "the last line ({}) names no workspace the daemon lists",
                truncate(last_line.trim(), 60)
            ),
        );
    };
    let id = Some(workspace.id.as_str());
    let Some(readme) = readme_at(&workspace.commit_id) else {
        return Verdict::fail(
            id,
            format!("no README.md in commit {}", workspace.commit_id),
        );
    };
    let base = base_readme.trim_end();
    if readme.trim_end().len() > base.len() && readme.starts_with(base) {
        Verdict {
            success: true,
            workspace: id.map(str::to_owned),
            reason: format!("README.md grew in {} ({})", workspace.id, workspace.state),
        }
    } else if readme.trim_end() == base {
        Verdict::fail(id, "README.md is unchanged in the workspace's commit")
    } else {
        Verdict::fail(id, "README.md changed other than by an appended line")
    }
}

fn judge_oriented(final_text: &str, workspaces: &[WorkspaceRecord]) -> Verdict {
    let Some(workspace) = workspaces
        .iter()
        .min_by(|a, b| a.created_at.cmp(&b.created_at))
    else {
        return Verdict::fail(None, "the daemon lists no workspace");
    };
    let id = Some(workspace.id.as_str());
    if !final_text.contains(&workspace.id) {
        return Verdict::fail(id, "the answer does not name the workspace");
    }
    let names_change = final_text
        .split(|c: char| !c.is_ascii_alphanumeric())
        .filter(|word| word.len() >= MIN_CHANGE_PREFIX)
        .any(|word| {
            let word = word.to_ascii_lowercase();
            workspace.change_id.starts_with(&word)
                || workspace.change_id_reverse_hex.starts_with(&word)
        });
    if names_change {
        Verdict {
            success: true,
            workspace: id.map(str::to_owned),
            reason: "the answer names the workspace and its change".to_owned(),
        }
    } else {
        Verdict::fail(id, "the answer names the workspace but not its change")
    }
}

fn truncate(text: &str, max_chars: usize) -> String {
    match text.char_indices().nth(max_chars) {
        Some((at, _)) => format!("{}…", &text[..at]),
        None => text.to_owned(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn record(id: &str, commit: &str, created_at: &str) -> WorkspaceRecord {
        WorkspaceRecord {
            id: id.to_owned(),
            state: "released".to_owned(),
            commit_id: commit.to_owned(),
            change_id: "73d60f3dfa6012f18e1747234dc3c4b7".to_owned(),
            change_id_reverse_hex: "swmtzkwmkptzyxkyrlysvsxwvmnwnvos".to_owned(),
            created_at: created_at.to_owned(),
        }
    }

    fn readmes(commit: &str) -> Option<String> {
        match commit {
            "grown" => Some("# Title\n\nBody\nAppended by the agent\n".to_owned()),
            "same" => Some("# Title\n\nBody\n".to_owned()),
            "edited" => Some("# Other\n\nBody\nmore\n".to_owned()),
            _ => None,
        }
    }

    const BASE: &str = "# Title\n\nBody\n";

    #[test]
    fn last_line_names_a_grown_workspace() {
        let workspaces = [record("0123456789abcdef", "grown", "1")];
        for answer in [
            "Done.\n0123456789abcdef",
            "Done.\n\n`0123456789abcdef`\n",
            "Done.\nWorkspace id: 0123456789abcdef.",
            "Done.\n0123456789abcdef (released).",
        ] {
            let verdict = judge(Scenario::A, answer, &workspaces, BASE, readmes);
            assert!(verdict.success, "{answer:?}: {verdict:?}");
            assert_eq!(verdict.workspace.as_deref(), Some("0123456789abcdef"));
        }
    }

    #[test]
    fn failures_say_why() {
        let grown = [record("0123456789abcdef", "grown", "1")];
        let verdict = judge(
            Scenario::B,
            "0123456789abcdef\nthanks",
            &grown,
            BASE,
            readmes,
        );
        assert!(!verdict.success);
        assert!(verdict.reason.contains("names no workspace"), "{verdict:?}");

        let verdict = judge(Scenario::C, "", &grown, BASE, readmes);
        assert_eq!(verdict.reason, "the agent gave no answer");

        for (commit, reason) in [
            ("same", "unchanged"),
            ("edited", "other than"),
            ("missing", "no README.md"),
        ] {
            let workspaces = [record("0123456789abcdef", commit, "1")];
            let verdict = judge(Scenario::A, "0123456789abcdef", &workspaces, BASE, readmes);
            assert!(!verdict.success);
            assert!(verdict.reason.contains(reason), "{commit}: {verdict:?}");
        }
    }

    /// `reef ws list --all` as the daemon printed it after a trial on
    /// `same-file` (owner and path replaced).
    const RECORDED: &str = include_str!("../../onboarding/fixtures/ws-list-released.json");

    #[test]
    fn judges_a_recorded_end_state() {
        let workspaces: Vec<WorkspaceRecord> = serde_json::from_str(RECORDED).unwrap();
        let commit = "7206a045dd2223e9e891eaac9b4986b23578c472";
        assert_eq!(workspaces[0].commit_id, commit);
        let readme_at = |at: &str| (at == commit).then(|| format!("{BASE}appended by fake\n"));
        let verdict = judge(
            Scenario::A,
            "Done.\n7770cce017a81eaf",
            &workspaces,
            BASE,
            readme_at,
        );
        assert!(verdict.success, "{verdict:?}");
        assert_eq!(
            verdict.reason,
            "README.md grew in 7770cce017a81eaf (released)"
        );
        let answer = "workspace: 7770cce017a81eaf\nchange: mnnmvzwzxyxx";
        assert!(judge(Scenario::D, answer, &workspaces, "", readme_at).success);
    }

    #[test]
    fn a_null_field_does_not_lose_the_list() {
        let recorded = RECORDED.replace(
            "\"created_at\": \"2026-10-24T06:07:42.633Z\"",
            "\"created_at\": null",
        );
        assert!(recorded.contains("\"created_at\": null"));
        let workspaces: Vec<WorkspaceRecord> = serde_json::from_str(&recorded).unwrap();
        assert_eq!(workspaces[0].created_at, "");
        assert_eq!(workspaces[0].id, "7770cce017a81eaf");
    }

    #[test]
    fn oriented_names_the_first_workspace_and_its_change() {
        let workspaces = [
            record("ffffffffffffffff", "x", "2026-10-23T02:00:00Z"),
            record("0123456789abcdef", "x", "2026-10-23T01:00:00Z"),
        ];
        for answer in [
            "workspace: 0123456789abcdef\nchange: swmtzkwm",
            "You are in 0123456789abcdef on change 73d60f3dfa60.",
            "workspace: 0123456789abcdef\nchange: SWMTZKWMKPTZ",
        ] {
            let verdict = judge(Scenario::D, answer, &workspaces, "", readmes);
            assert!(verdict.success, "{answer:?}: {verdict:?}");
        }
        for answer in [
            "workspace: ffffffffffffffff\nchange: swmtzkwm",
            "workspace: 0123456789abcdef\nchange: swmtz",
            "workspace: 0123456789abcdef",
        ] {
            let verdict = judge(Scenario::D, answer, &workspaces, "", readmes);
            assert!(!verdict.success, "{answer:?}: {verdict:?}");
        }
        let verdict = judge(Scenario::D, "anything", &[], "", readmes);
        assert_eq!(verdict.reason, "the daemon lists no workspace");
    }
}
