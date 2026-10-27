//! `reef here` (R1.26, ADR 0025): one call that answers "where am I". The
//! workspace is the one `REEF_WORKSPACE` names (set by `ws run`), else the
//! active one whose directory contains the current directory. It is read
//! without a snapshot or a disk walk unless `--snapshot`, and the answer
//! ends in the command that makes sense next. Outside every workspace it
//! says so and exits 0: not being in one is an answer, not an error.

use std::io::{self, Write};
use std::path::Path;

use reef_api::v1::{self, GetWorkspaceRequest};
use reef_client::json::{Repository, Workspace, millis_of};
use serde::Serialize;

use super::out::{TextRender, block, shell_line};
use super::render::{lease_text, now_ms, span};
use super::repo::missing;
use super::scope::{current_dir, divergent_log, is_workspace_not_found};
use super::{CliError, Ctx};

/// Operation ids are shown by this many characters, as `ws inspect` does.
const OP_WIDTH: usize = 12;
/// Change ids are shown by this many characters of their reverse hex.
const CHANGE_WIDTH: usize = 12;

/// `reef here`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Snapshot the workspace first, so the answer covers the files on disk.
    #[arg(long)]
    snapshot: bool,
}

/// Inside a workspace: the workspace, then what to do.
#[derive(Debug, Serialize)]
struct Inside {
    in_workspace: bool,
    #[serde(flatten)]
    workspace: Workspace,
    /// Milliseconds since the last snapshot, for the text form.
    #[serde(skip)]
    snapshot_age_ms: Option<i64>,
    hint: String,
    next: Vec<String>,
}

/// Outside every workspace: the directory, the registered repository that
/// contains it if any, and how to get a workspace.
#[derive(Debug, Serialize)]
struct Outside {
    in_workspace: bool,
    directory: String,
    repo: Option<Repository>,
    hint: String,
    /// `ws run` with no command: the one `next` that is a prefix, since
    /// the command is the agent's (ADR 0025).
    next: Vec<String>,
}

pub(crate) async fn run(ctx: &Ctx, args: Args) -> Result<(), CliError> {
    let from_env = std::env::var("REEF_WORKSPACE")
        .ok()
        .filter(|id| !id.trim().is_empty());
    let cwd = current_dir()?;
    let target = match &from_env {
        Some(id) => id.clone(),
        None => cwd.display().to_string(),
    };
    let request = GetWorkspaceRequest {
        repo: String::new(),
        workspace: target,
        snapshot_first: args.snapshot,
        skip_disk_usage: true,
    };
    match ctx.call(ctx.client.get_workspace(request)).await {
        Ok(response) => {
            let ws = response.workspace.ok_or_else(|| missing("workspace"))?;
            ctx.out.object(&inside(&ws, from_env.is_some()))
        }
        Err(CliError::Daemon(error)) if from_env.is_none() && is_workspace_not_found(&error) => {
            let repos = ctx.call(ctx.client.list_repositories()).await?;
            ctx.out.object(&outside(&cwd, &repos))
        }
        Err(error) => Err(error),
    }
}

/// The answer for `ws`; `in_run` when `ws run` started this process, which
/// releases the workspace itself.
fn inside(ws: &v1::Workspace, in_run: bool) -> Inside {
    let workspace = Workspace::from(ws);
    let (hint, next) = advise(&workspace, in_run);
    Inside {
        in_workspace: true,
        snapshot_age_ms: ws
            .last_snapshot_at
            .as_ref()
            .map(|at| now_ms() - millis_of(at)),
        workspace,
        hint,
        next,
    }
}

/// What to do in `ws`, by its state: release a healthy one when done (or
/// nothing, inside `ws run`), renew one in its grace period (R2.11), list a divergent one's commits (R2.27),
/// inspect a flagged one, make another if it is gone.
fn advise(ws: &Workspace, in_run: bool) -> (String, Vec<String>) {
    let argv = |words: &[&str]| words.iter().map(|word| (*word).to_owned()).collect();
    let lease_state = ws
        .lease
        .as_ref()
        .map_or("expired", |lease| lease.state.as_str());
    match (ws.state.as_str(), lease_state) {
        ("active", "active" | "in_grace") if ws.divergent => (
            "Its change is divergent: two or more commits of one change. List them, then abandon the one to drop (reef abandon <commit id>).".to_owned(),
            divergent_log(&ws.id, &ws.change_id),
        ),
        ("active", "active" | "in_grace") if ws.attention.is_some() => (
            "It needs attention; inspect it before going on.".to_owned(),
            argv(&["reef", "ws", "inspect", &ws.id]),
        ),
        ("active", "in_grace") if !in_run => (
            "Its lease ran out; renew it to keep working here, or release it. It is reclaimed when the grace period ends."
                .to_owned(),
            argv(&["reef", "ws", "renew", &ws.id]),
        ),
        ("active", "active" | "in_grace") if in_run => (
            "Work here; `ws run` snapshots and releases this workspace when your command exits."
                .to_owned(),
            Vec::new(),
        ),
        ("active", "active") => (
            "Work here; release it when done (a final snapshot is taken first).".to_owned(),
            argv(&["reef", "ws", "release", &ws.id]),
        ),
        ("releasing", _) => (
            "It is being released; its directory is about to go.".to_owned(),
            Vec::new(),
        ),
        (state, _) => (
            format!(
                "This workspace is {}; its snapshots are kept but it cannot be used. Create a new one.",
                if state == "active" { "expired" } else { state }
            ),
            argv(&["reef", "ws", "create", "--repo", &ws.repo_id]),
        ),
    }
}

/// The answer outside every workspace, with the registered repository
/// whose root contains `dir` (the deepest, as the daemon resolves it).
fn outside(dir: &Path, repos: &[v1::Repository]) -> Outside {
    let repo = repos
        .iter()
        .filter(|repo| dir.starts_with(&repo.root))
        .max_by_key(|repo| Path::new(&repo.root).components().count())
        .map(Repository::from);
    let hint = match &repo {
        Some(repo) => format!(
            "Not in a workspace; this is repository {}. Run your command in one: reef ws run -- <command>.",
            repo.id
        ),
        None => "Not in a workspace, and no registered repository contains this directory; \
                 `reef ws run -- <command>` in a git or jj checkout registers it."
            .to_owned(),
    };
    Outside {
        in_workspace: false,
        directory: dir.display().to_string(),
        repo,
        hint,
        next: ["reef", "ws", "run", "--"].map(str::to_owned).to_vec(),
    }
}

/// `active, 27m left, heartbeat every 1m`, `expired 3m ago`, or
/// `ended (released)`.
fn lease_line(ws: &Workspace) -> String {
    let now = now_ms();
    let text = lease_text(ws.lease.as_ref(), now);
    match &ws.lease {
        Some(lease) if lease.state == "active" || lease.state == "in_grace" => {
            let every = lease
                .heartbeat_every_ms
                .map(|ms| format!(", heartbeat every {}", span(i64::try_from(ms).unwrap_or(0))))
                .unwrap_or_default();
            let state = lease.state.replace('_', " ");
            format!("{state}, {text}{every}")
        }
        _ => text,
    }
}

impl TextRender for Inside {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let ws = &self.workspace;
        let mut state = ws.state.clone();
        if ws.divergent {
            state.push_str(", divergent");
        }
        if let Some(attention) = &ws.attention {
            state.push_str(", attention: ");
            state.push_str(attention);
        }
        let snapshot = match (self.snapshot_age_ms, &ws.last_snapshot_op) {
            (Some(age), Some(op)) => format!(
                "{} ago (op {})",
                span(age),
                op.chars().take(OP_WIDTH).collect::<String>()
            ),
            (Some(age), None) => format!("{} ago", span(age)),
            (None, _) => "none yet".to_owned(),
        };
        let mut pairs = vec![
            ("workspace", ws.id.clone()),
            ("path", ws.path.clone()),
            (
                "change",
                ws.change_id_reverse_hex
                    .chars()
                    .take(CHANGE_WIDTH)
                    .collect(),
            ),
            ("state", state),
            ("lease", lease_line(ws)),
            ("last snapshot", snapshot),
        ];
        if !ws.stores.is_empty() {
            let names: Vec<&str> = ws.stores.iter().map(|store| store.name.as_str()).collect();
            pairs.push(("stores", names.join(", ")));
        }
        pairs.push(("hint", self.hint.clone()));
        if !self.next.is_empty() {
            pairs.push(("next", shell_line(&self.next)));
        }
        block(w, &pairs)
    }
}

impl TextRender for Outside {
    fn render(&self, w: &mut dyn Write) -> io::Result<()> {
        let repo = self.repo.as_ref().map_or_else(
            || "none registered here".to_owned(),
            |repo| format!("{} ({})", repo.id, repo.root),
        );
        block(
            w,
            &[
                ("workspace", "none".to_owned()),
                ("directory", self.directory.clone()),
                ("repo", repo),
                ("hint", self.hint.clone()),
                ("next", format!("{} <command>", shell_line(&self.next))),
            ],
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn workspace(state: &str, lease: Option<&str>) -> Workspace {
        let state = match state {
            "active" => v1::WorkspaceState::Active,
            "released" => v1::WorkspaceState::Released,
            other => panic!("no state {other}"),
        };
        let ws = v1::Workspace {
            id: "0123456789abcdef".into(),
            repo_id: "fedcba9876543210".into(),
            state: state.into(),
            lease: lease.map(|_| v1::Lease::default()),
            ..v1::Workspace::default()
        };
        let mut ws = Workspace::from(&ws);
        if let (Some(lease), Some(state)) = (ws.lease.as_mut(), lease) {
            state.clone_into(&mut lease.state);
        }
        ws
    }

    #[test]
    fn next_follows_the_state() {
        let (_, next) = advise(&workspace("active", Some("active")), false);
        assert_eq!(next, ["reef", "ws", "release", "0123456789abcdef"]);
        let (hint, next) = advise(&workspace("active", Some("active")), true);
        assert!(next.is_empty() && hint.contains("ws run"), "{hint}");
        let (hint, next) = advise(&workspace("active", Some("in_grace")), false);
        assert_eq!(next, ["reef", "ws", "renew", "0123456789abcdef"]);
        assert!(hint.contains("renew"), "{hint}");
        let (hint, next) = advise(&workspace("active", Some("in_grace")), true);
        assert!(next.is_empty() && hint.contains("ws run"), "{hint}");
        let (_, next) = advise(&workspace("released", Some("ended")), false);
        assert_eq!(next, ["reef", "ws", "create", "--repo", "fedcba9876543210"]);
        let (hint, next) = advise(&workspace("active", Some("expired")), true);
        assert_eq!(next[..3], ["reef", "ws", "create"]);
        assert!(hint.contains("expired"), "{hint}");
        let mut divergent = workspace("active", Some("active"));
        divergent.divergent = true;
        divergent.change_id = "01".into();
        let (hint, next) = advise(&divergent, true);
        assert_eq!(
            next,
            [
                "reef",
                "log",
                "--workspace",
                "0123456789abcdef",
                "-r",
                "change_id(zy)"
            ]
        );
        assert!(hint.contains("reef abandon"), "{hint}");
        let mut flagged = workspace("active", Some("active"));
        flagged.attention = Some("store-missing".into());
        let (_, next) = advise(&flagged, true);
        assert_eq!(next, ["reef", "ws", "inspect", "0123456789abcdef"]);
    }

    #[test]
    fn outside_names_the_containing_repository() {
        let repos = [
            v1::Repository {
                id: "aaaa000000000001".into(),
                root: "/src".into(),
                ..v1::Repository::default()
            },
            v1::Repository {
                id: "aaaa000000000002".into(),
                root: "/src/inner".into(),
                ..v1::Repository::default()
            },
        ];
        let here = outside(Path::new("/src/inner/lib"), &repos);
        assert_eq!(here.repo.unwrap().id, "aaaa000000000002");
        assert_eq!(here.next, ["reef", "ws", "run", "--"]);
        assert!(outside(Path::new("/elsewhere"), &repos).repo.is_none());
        // A sibling whose name shares a prefix is not inside.
        assert!(outside(Path::new("/srcx"), &repos).repo.is_none());
    }

    #[test]
    fn text_fits_twelve_lines() {
        let mut ws = workspace("active", Some("active"));
        ws.divergent = true;
        ws.attention = Some("store-missing".into());
        let (hint, next) = advise(&ws, false);
        let here = Inside {
            in_workspace: true,
            workspace: ws,
            snapshot_age_ms: Some(12_000),
            hint,
            next,
        };
        let mut buffer = Vec::new();
        here.render(&mut buffer).unwrap();
        let text = String::from_utf8(buffer).unwrap();
        assert!(text.lines().count() <= 12, "{text}");
        assert!(
            text.contains("state:         active, divergent, attention: store-missing"),
            "{text}"
        );
        assert!(text.contains("last snapshot: 12s ago"), "{text}");
    }
}
