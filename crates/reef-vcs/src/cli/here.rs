//! `reef here` (R1.26, ADR 0025): one call that answers "where am I". The
//! workspace is the one `REEF_WORKSPACE` names (set by `ws run`), else the
//! active one whose directory contains the current directory. It is read
//! through the status index (#262: a snapshot only of paths written since
//! the last one), without a disk walk, and the answer ends in the command
//! that makes sense next: the workspace's health first, then the next step
//! of its change's lifecycle, which the daemon names (`step`, ADR 0080).
//! Outside every workspace it says so and exits 0: not being in one is an
//! answer, not an error.

use std::fmt::Write as _;
use std::io::{self, Write};
use std::path::Path;

use reef_api::v1::{self, GetWorkspaceRequest};
use reef_client::json::{DiskStatus, Repository, Workspace, human_bytes, millis_of};
use serde::Serialize;

use super::out::{TextRender, block, shell_line};
use super::render::{lease_text, now_ms, span};
use super::repo::missing;
use super::scope::{current_dir, divergent_log, is_workspace_not_found};
use super::{CliError, Ctx, Global};

/// Operation ids are shown by this many characters, as `ws inspect` does.
const OP_WIDTH: usize = 12;
/// Change ids are shown by this many characters of their reverse hex.
const CHANGE_WIDTH: usize = 12;

/// `reef here`.
#[derive(Debug, clap::Args)]
pub(crate) struct Args {
    /// Snapshot the workspace first, walking every file, instead of reading
    /// only the paths the watcher reported.
    #[arg(long)]
    snapshot: bool,
}

/// Inside a workspace: the workspace, then what to do.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.Inside")]
pub(crate) struct Inside {
    #[schemars(extend("const" = true))]
    in_workspace: bool,
    /// Whether a program holds it through `ws provider` (R2.26), and
    /// releases it itself.
    held: bool,
    #[serde(flatten)]
    workspace: Workspace,
    /// Milliseconds since the last snapshot, for the text form.
    #[serde(skip)]
    snapshot_age_ms: Option<i64>,
    /// How full the disk is (#272): present only when its level is not
    /// `ok`, as is the workspace's own `disk`.
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_status: Option<DiskStatus>,
    /// A landing destination of the repository whose push to its git
    /// remote needs a person (R3.17): diverged, refused, or retrying on
    /// failed authentication. Absent otherwise.
    #[serde(skip_serializing_if = "Option::is_none")]
    push: Option<PushAttention>,
    hint: String,
    next: Vec<String>,
}

/// A destination whose push needs a person (`reef landing pushes`).
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.PushAttention")]
pub(crate) struct PushAttention {
    destination: String,
    git_remote: String,
    state: String,
    reason: String,
}

impl PushAttention {
    /// The first of `pushes` that needs a person.
    fn first(pushes: &[v1::DestinationPush]) -> Option<PushAttention> {
        pushes
            .iter()
            .find(|push| match push.state() {
                v1::PushState::Diverged | v1::PushState::Refused => true,
                v1::PushState::Retrying => push.reason == reef_api::error::GIT_AUTH_FAILED,
                _ => false,
            })
            .map(|push| PushAttention {
                destination: push.destination.clone(),
                git_remote: push.git_remote.clone(),
                state: push
                    .state()
                    .as_str_name()
                    .trim_start_matches("PUSH_STATE_")
                    .to_ascii_lowercase(),
                reason: push.reason.clone(),
            })
    }

    fn text(&self) -> String {
        format!(
            "{} is not pushed to {}/{}: {} ({}); `reef landing pushes`",
            self.destination, self.git_remote, self.destination, self.state, self.reason
        )
    }
}

/// Outside every workspace: the directory, the registered repository that
/// contains it if any, and how to get a workspace.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "cli.Outside")]
pub(crate) struct Outside {
    #[schemars(extend("const" = false))]
    in_workspace: bool,
    directory: String,
    repo: Option<Repository>,
    hint: String,
    /// `ws run` with no command: the one `next` that is a prefix, since
    /// the command is the agent's (ADR 0025).
    next: Vec<String>,
}

pub(crate) async fn run(ctx: &Ctx, global: &Global, args: Args) -> Result<(), CliError> {
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
        at_operation: String::new(),
        sync_first: !args.snapshot,
    };
    match ctx.call(ctx.client.get_workspace(request)).await {
        Ok(response) => {
            let ws = response.workspace.ok_or_else(|| missing("workspace"))?;
            // A holder record is this machine's; a remote daemon's
            // workspaces have none here.
            let held = !super::remote_mode()
                && global
                    .state_dir()
                    .is_ok_and(|dir| super::holder::current(&dir, &ws).is_some());
            let disk = response.disk_status.as_deref().map(DiskStatus::from);
            // A push that needs a person is the repository's news, not the
            // workspace's: one read of the daemon's rows, no git. An older
            // daemon has no ListPushes, and then there is no line.
            let pushes = ctx
                .call(ctx.client.list_pushes(&ws.repo_id))
                .await
                .unwrap_or_default();
            let mut here = inside(&ws, from_env.is_some(), held, disk.as_ref());
            here.push = PushAttention::first(&pushes);
            ctx.out.object_with(&here, BriefInside::from)
        }
        Err(CliError::Daemon(error)) if from_env.is_none() && is_workspace_not_found(&error) => {
            let repos = ctx.call(ctx.client.list_repositories()).await?;
            ctx.out.object(&outside(&cwd, &repos))
        }
        Err(error) => Err(error),
    }
}

/// The answer for `ws`; `in_run` when `ws run` started this process, and
/// `held` when a `ws provider` holder holds it: both release it
/// themselves. `disk` is the machine's disk status, shown (with the
/// workspace's own figure) only when its level is not `ok` (#272).
fn inside(ws: &v1::Workspace, in_run: bool, held: bool, disk: Option<&DiskStatus>) -> Inside {
    let mut workspace = Workspace::from(ws);
    let disk = disk.filter(|disk| disk.level != "ok").cloned();
    let (mut hint, next) = advise_with_disk(&workspace, in_run, held, disk.as_ref());
    if let Some(overlap) = overlap_hint(&workspace) {
        hint.push(' ');
        hint.push_str(&overlap);
    }
    if disk.is_none() {
        workspace.disk = None;
    }
    Inside {
        in_workspace: true,
        held,
        push: None,
        snapshot_age_ms: ws
            .last_snapshot_at
            .as_ref()
            .map(|at| now_ms() - millis_of(at)),
        workspace,
        disk_status: disk,
        hint,
        next,
    }
}

/// The sentence `hint` gains when another open change changes this
/// workspace's paths, naming the newest.
fn overlap_hint(ws: &Workspace) -> Option<String> {
    let first = ws.overlaps.first()?;
    let mut who = super::render::overlap_who(first);
    if let Some(initial) = who.get_mut(0..1) {
        initial.make_ascii_uppercase();
    }
    let path = first.paths.first().map_or("-", String::as_str);
    let change = super::render::overlap_change(first);
    let others = match ws.overlap_count.saturating_sub(1) {
        0 => String::new(),
        1 => ", and 1 other change overlaps (`reef status` lists them)".to_owned(),
        n => format!(", and {n} other changes overlap (`reef status` lists them)"),
    };
    Some(format!(
        "{who} also changes {path}; `reef diff -r {change}` shows its change{others}. Conflicts do not block, so carry on or coordinate."
    ))
}

/// [`advise`], with the disk's word where it matters (#272): a `low` disk
/// after divergence, attention and an expiring lease; then being behind
/// trunk; then missing dependencies (#195); then a `warn` disk.
fn advise_with_disk(
    ws: &Workspace,
    in_run: bool,
    held: bool,
    disk: Option<&DiskStatus>,
) -> (String, Vec<String>) {
    let usable = matches!(ws.state.as_str(), "active")
        && ws
            .lease
            .as_ref()
            .is_some_and(|lease| lease.state == "active" || lease.state == "in_grace");
    let urgent = ws.divergent
        || ws.attention.is_some()
        || ws
            .lease
            .as_ref()
            .is_some_and(|lease| lease.state == "in_grace" && !in_run);
    if !usable || urgent {
        return advise(ws, in_run, held);
    }
    if let Some(disk) = disk.filter(|disk| disk.level == "low") {
        return disk_advice(ws, disk, true);
    }
    if ws.step.as_deref() == Some("resolve") || behind_and_rebase_fits(ws) {
        return advise(ws, in_run, held);
    }
    if let Some(advice) = dependency_advice(ws) {
        return advice;
    }
    if let Some(disk) = disk.filter(|disk| disk.level == "warn") {
        let (hint, next) = disk_advice(ws, disk, false);
        if !next.is_empty() {
            return (hint, next);
        }
        let (rest, next) = advise(ws, in_run, held);
        return (format!("{hint} {rest}"), next);
    }
    advise(ws, in_run, held)
}

/// What a `low` or `warn` disk asks for: release the largest idle
/// workspace, else list them.
fn disk_advice(ws: &Workspace, disk: &DiskStatus, low: bool) -> (String, Vec<String>) {
    let argv = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let space = disk.tightest().map_or_else(String::new, |volume| {
        format!(
            " ({} free, floor {})",
            human_bytes(volume.usable_free_bytes()),
            human_bytes(volume.floor_bytes)
        )
    });
    let mut hint = if low {
        format!(
            "The disk is nearly full{space}: new workspaces are refused. Release workspaces you are done with; a release snapshots first, so nothing is lost."
        )
    } else if disk.reasons.iter().any(|reason| reason == "over_budget")
        && !disk
            .reasons
            .iter()
            .any(|reason| reason == "free_near_floor")
    {
        format!(
            "Reef holds {} of disk, over its budget of {}.",
            human_bytes(disk.reef_bytes),
            human_bytes(disk.budget_bytes)
        )
    } else {
        format!("Free disk space is getting low{space}.")
    };
    if let Some(target) = ws.disk.as_ref().and_then(|figure| {
        figure
            .largest
            .iter()
            .find(|dir| dir.path == "target" || dir.path.ends_with("/target"))
            .filter(|dir| dir.bytes > RUST_HINT_BYTES)
    }) {
        let _ = write!(
            hint,
            " {} is {} of Rust build output; CARGO_INCREMENTAL=0 and line-tables-only debug info cut it about fourfold.",
            target.path,
            human_bytes(target.bytes)
        );
    }
    let next = match &disk.largest_idle {
        Some(idle) if idle.workspace != ws.id => argv(&["reef", "ws", "release", &idle.workspace]),
        _ if low => argv(&["reef", "ws", "list"]),
        _ => Vec::new(),
    };
    (hint, next)
}

/// A `target` directory larger than this gets the Rust hint.
const RUST_HINT_BYTES: u64 = 2 << 30;

/// #195: the first directory whose declared dependencies are not
/// installed in this workspace, and the command that installs them.
fn dependency_advice(ws: &Workspace) -> Option<(String, Vec<String>)> {
    let dir = ws.disk.as_ref()?.missing_dependencies.first()?;
    let root = Path::new(&ws.path).join(dir);
    let has = |name: &str| root.join(name).exists();
    // `next` runs from wherever `reef here` ran: the directory in full.
    let full = root.display().to_string();
    let dir = &full;
    let argv = |words: &[&str]| {
        words
            .iter()
            .map(|word| (*word).to_owned())
            .collect::<Vec<_>>()
    };
    let (lockfile, installed, next) = if has("pnpm-lock.yaml") {
        (
            "pnpm-lock.yaml",
            "node_modules",
            argv(&["pnpm", "install", "--frozen-lockfile", "--dir", dir]),
        )
    } else if has("yarn.lock") {
        // Yarn 2 and later read `.yarnrc.yml`; classic Yarn's flag differs.
        let frozen = if has(".yarnrc.yml") {
            "--immutable"
        } else {
            "--frozen-lockfile"
        };
        (
            "yarn.lock",
            "node_modules",
            argv(&["yarn", "--cwd", dir, "install", frozen]),
        )
    } else if has("bun.lock") || has("bun.lockb") {
        (
            "bun.lock",
            "node_modules",
            argv(&["bun", "install", "--frozen-lockfile", "--cwd", dir]),
        )
    } else if has("package-lock.json") || has("npm-shrinkwrap.json") {
        (
            "package-lock.json",
            "node_modules",
            argv(&["npm", "ci", "--prefix", dir]),
        )
    } else if has("uv.lock") {
        (
            "uv.lock",
            ".venv",
            argv(&["uv", "sync", "--frozen", "--directory", dir]),
        )
    } else {
        return None;
    };
    let missing = ws.disk.as_ref()?.missing_dependencies.first()?;
    let place = if missing == "." {
        "This workspace".to_owned()
    } else {
        missing.clone()
    };
    Some((
        format!(
            "{place} has {lockfile} but no {installed}: each workspace installs its own. Install them, or declare a shared store in reef.toml (`reef store --help`)."
        ),
        next,
    ))
}

/// What to do in `ws`, by its state: release a healthy one when done (or
/// nothing, inside `ws run`), renew one in its grace period (R2.11), list a divergent one's commits (R2.27),
/// inspect a flagged one, make another if it is gone.
fn advise(ws: &Workspace, in_run: bool, held: bool) -> (String, Vec<String>) {
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
        ("active", "active" | "in_grace") if ws.step.as_deref() == Some("resolve") => {
            let files = files(ws.conflicted_path_count);
            (
                format!(
                    "Its change has conflicts in {files}; `reef conflict show` shows each side. Edit the files to resolve them; a snapshot records the result."
                ),
                argv(&["reef", "conflict", "show"]),
            )
        }
        ("active", "active" | "in_grace") if behind_and_rebase_fits(ws) => {
            let behind = ws.behind_trunk.unwrap_or_default();
            let count = match behind {
                1 => "1 commit".to_owned(),
                1000.. => "over 1,000 commits".to_owned(),
                n => format!("{n} commits"),
            };
            (
                format!(
                    "Its change is {count} behind trunk (as of Reef's last fetch); `reef rebase` fetches and brings it onto trunk's head, files included."
                ),
                argv(&["reef", "rebase", "--workspace", &ws.id]),
            )
        }
        ("active", "active" | "in_grace") => lifecycle_advice(ws, in_run, held),
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

/// Whether the change is behind trunk and a `reef rebase` fits its step:
/// not while it is proposed, queued, declined, done or conflicted, since a
/// rebase makes a proposal amended and a queued change cannot move.
fn behind_and_rebase_fits(ws: &Workspace) -> bool {
    let fits = match ws.step.as_deref() {
        Some("land" | "wait" | "revise" | "done" | "resolve") => false,
        Some("propose") => ws
            .lifecycle
            .as_ref()
            .is_none_or(|lifecycle| lifecycle.state != "proposed"),
        _ => true,
    };
    fits && ws.behind_trunk.is_some_and(|n| n > 0)
}

/// `1 file` or `N files`.
fn files(count: u32) -> String {
    match count {
        1 => "1 file".to_owned(),
        n => format!("{n} files"),
    }
}

/// What the change's step asks for (#262, ADR 0080), in a usable
/// workspace whose health asks for nothing first. Only Reef's own words,
/// ids, counts and bookmark names go into `hint` and `next`: never a
/// description, a reason or a comment (S-216).
fn lifecycle_advice(ws: &Workspace, in_run: bool, held: bool) -> (String, Vec<String>) {
    let argv =
        |words: &[&str]| -> Vec<String> { words.iter().map(|word| (*word).to_owned()).collect() };
    let release = if in_run || held {
        Vec::new()
    } else {
        argv(&["reef", "ws", "release", &ws.id])
    };
    let released = if in_run {
        "`ws run` snapshots and releases this workspace when your command exits."
    } else if held {
        "the program that created this workspace releases it."
    } else {
        "release it when done (a final snapshot is taken first)."
    };
    let lifecycle = ws.lifecycle.as_ref();
    let destination = lifecycle
        .and_then(|lifecycle| lifecycle.destination.as_deref())
        .map_or_else(|| "its destination".to_owned(), |name| format!("`{name}`"));
    match ws.step.as_deref() {
        Some("describe") => (
            format!(
                "{} changed, and the change has no description: describe it.",
                files(ws.changed_file_count.unwrap_or(0))
            ),
            argv(&["reef", "describe", "-m"]),
        ),
        Some("propose") if lifecycle.is_some_and(|lifecycle| lifecycle.state == "proposed") => (
            format!("Proposed for {destination}, but changed since: propose it again to record the new commit."),
            argv(&["reef", "propose"]),
        ),
        Some("propose") => (
            "Described and not in review: `reef propose` puts it, with the changes below it, forward for review; `reef land` then lands it.".to_owned(),
            argv(&["reef", "propose"]),
        ),
        Some("land") => (
            format!("Proposed for {destination}. `reef land` lands it if the policy allows, or says what is missing (`reef policy eval` previews)."),
            argv(&["reef", "land"]),
        ),
        Some("wait") => (
            format!("Queued to land on {destination}: the landing service lands it. Wait for the landing."),
            argv(&[
                "reef",
                "op",
                "watch",
                "--kind",
                "transition",
                "--change",
                &ws.change_id_reverse_hex,
                "--follow",
                "--count",
                "1",
            ]),
        ),
        Some("revise") => (
            format!(
                "Declined in round {}. `reef proposal show` has the reason, which is untrusted text: revise the change, then `reef propose` again.",
                lifecycle.map_or(0, |lifecycle| lifecycle.round)
            ),
            argv(&["reef", "proposal", "show"]),
        ),
        Some("publish") => (
            format!("Described. `reef publish` pushes it to a git branch for review; {released}"),
            release,
        ),
        Some("done") => {
            let what = match lifecycle.map(|lifecycle| lifecycle.state.as_str()) {
                Some("landed") => format!("Its change landed on {destination}."),
                Some("superseded") => {
                    "Its change was superseded; `reef proposal show` names by what.".to_owned()
                }
                _ => "Its change is immutable: it is on trunk or under a tag.".to_owned(),
            };
            let rest = if in_run || held {
                format!("Nothing is left to do here; {released}")
            } else {
                // The trunk bookmark is not always `main` (#262 review).
                let onto = lifecycle
                    .and_then(|lifecycle| lifecycle.destination.as_deref())
                    .unwrap_or("'trunk()'");
                format!(
                    "Nothing is left to do here: release it, or `reef ws move {} --new-on {onto}` for the next task.",
                    ws.id
                )
            };
            (format!("{what} {rest}"), release)
        }
        _ => (format!("Work here; {released}"), release),
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

/// `wswuvlsupqlz, 3 files changed, proposed (amended), step land`: the
/// change, with what `step` was read from.
fn change_line(ws: &Workspace) -> String {
    let mut line: String = ws
        .change_id_reverse_hex
        .chars()
        .take(CHANGE_WIDTH)
        .collect();
    if let Some(count) = ws.changed_file_count {
        let _ = write!(line, ", {} changed", files(count));
    }
    if let Some(lifecycle) = ws.lifecycle.as_ref().filter(|lifecycle| lifecycle.recorded) {
        let _ = write!(line, ", {}", lifecycle.state);
        if lifecycle.amended {
            line.push_str(" (amended)");
        }
    }
    if let Some(step) = &ws.step {
        let _ = write!(line, ", step {step}");
    }
    line
}

/// The brief view of [`Inside`] (#123, ADR 0045): always `in_workspace`,
/// `id`, `path`, `change_id_reverse_hex`, `step` (when read) and `hint`;
/// `changed_file_count` when read, even 0; `next` when not empty; `state`
/// when not `active`; `held`, `divergent`, `attention` and
/// `conflicted_path_count` when set; `disk`, `disk_status`, `push`,
/// `overlaps` and `overlap_count` when present. Dropped: `repo_id`,
/// `owner`, `purpose`, `change_id`, `commit_id`, `backend`, `created_at`,
/// `last_snapshot_at`, `last_snapshot_op`, `lease`, `watcher`,
/// `watch_state`, `local_snapshot_count`, `stores`, `provenance`,
/// `behind_trunk` (the hint says it) and `lifecycle` (`step` says what to
/// do; `reef proposal show` has the rest).
///
/// Which keys this view keeps may change in a minor release.
#[derive(Debug, Serialize, schemars::JsonSchema)]
#[schemars(rename = "brief.cli.Inside")]
pub(crate) struct BriefInside<'a> {
    /// Always true.
    in_workspace: bool,
    /// Whether a `ws provider` holder releases it.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    held: bool,
    /// The workspace.
    id: &'a str,
    /// Its directory.
    path: &'a str,
    /// Its change, in jj's reverse hex.
    change_id_reverse_hex: &'a str,
    /// Its state, when not `active`.
    #[serde(skip_serializing_if = "Option::is_none")]
    state: Option<&'a str>,
    /// Its change has more than one commit.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    divergent: bool,
    /// Why it needs attention.
    #[serde(skip_serializing_if = "Option::is_none")]
    attention: Option<&'a str>,
    /// Paths its change changes.
    #[serde(skip_serializing_if = "Option::is_none")]
    changed_file_count: Option<u32>,
    /// What its change needs next (ADR 0080).
    #[serde(skip_serializing_if = "Option::is_none")]
    step: Option<&'a str>,
    /// Conflicted paths in its change.
    #[serde(skip_serializing_if = "is_zero")]
    conflicted_path_count: u32,
    /// Its disk figure, while the disk level is not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    disk: Option<reef_client::brief::Disk>,
    /// The machine's disk status, while its level is not `ok`.
    #[serde(skip_serializing_if = "Option::is_none")]
    disk_status: Option<&'a DiskStatus>,
    /// A destination whose push needs a person.
    #[serde(skip_serializing_if = "Option::is_none")]
    push: Option<&'a PushAttention>,
    /// Other open changes on its paths.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    overlaps: Vec<reef_client::brief::Overlap<'a>>,
    /// Such changes, in all.
    #[serde(skip_serializing_if = "is_zero")]
    overlap_count: u32,
    /// What to do, in a sentence or two.
    hint: &'a str,
    /// The command that does it.
    #[serde(skip_serializing_if = "<[_]>::is_empty")]
    next: &'a [String],
}

#[expect(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde's `skip_serializing_if` passes a reference"
)]
fn is_zero(value: &u32) -> bool {
    *value == 0
}

impl<'a> From<&'a Inside> for BriefInside<'a> {
    fn from(here: &'a Inside) -> Self {
        let ws = &here.workspace;
        Self {
            in_workspace: true,
            held: here.held,
            id: &ws.id,
            path: &ws.path,
            change_id_reverse_hex: &ws.change_id_reverse_hex,
            state: (ws.state != "active").then_some(ws.state.as_str()),
            divergent: ws.divergent,
            attention: ws.attention.as_deref(),
            changed_file_count: ws.changed_file_count,
            step: ws.step.as_deref(),
            conflicted_path_count: ws.conflicted_path_count,
            disk: ws.disk.as_ref().map(reef_client::brief::Disk::from),
            disk_status: here.disk_status.as_ref(),
            push: here.push.as_ref(),
            overlaps: ws
                .overlaps
                .iter()
                .map(reef_client::brief::Overlap::from)
                .collect(),
            overlap_count: ws.overlap_count,
            hint: &here.hint,
            next: &here.next,
        }
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
            ("change", change_line(ws)),
            ("state", state),
            ("lease", lease_line(ws)),
            ("last snapshot", snapshot),
        ];
        if !ws.stores.is_empty() {
            let names: Vec<&str> = ws.stores.iter().map(|store| store.name.as_str()).collect();
            pairs.push(("stores", names.join(", ")));
        }
        if let Some(disk) = &self.disk_status {
            let space = disk.tightest().map_or_else(String::new, |volume| {
                format!(
                    ", {} free (floor {})",
                    human_bytes(volume.usable_free_bytes()),
                    human_bytes(volume.floor_bytes)
                )
            });
            pairs.push(("disk", format!("{}{space}", disk.level)));
        }
        if let Some(push) = &self.push {
            pairs.push(("push", push.text()));
        }
        if let Some(first) = ws.overlaps.first() {
            let mut line = super::render::overlap_line(first);
            match ws.overlap_count.saturating_sub(1) {
                0 => {}
                1 => line.push_str(" (+1 more change)"),
                more => {
                    let _ = write!(line, " (+{more} more changes)");
                }
            }
            pairs.push(("overlaps", line));
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
    fn behind_trunk_suggests_a_rebase() {
        let mut ws = workspace("active", Some("active"));
        ws.behind_trunk = Some(3);
        let (hint, next) = advise(&ws, false, false);
        assert_eq!(next, ["reef", "rebase", "--workspace", "0123456789abcdef"]);
        assert!(hint.contains("3 commits behind"), "{hint}");
        ws.behind_trunk = Some(0);
        let (_, next) = advise(&ws, false, false);
        assert_eq!(next, ["reef", "ws", "release", "0123456789abcdef"]);
    }

    #[test]
    fn next_follows_the_state() {
        let (_, next) = advise(&workspace("active", Some("active")), false, false);
        assert_eq!(next, ["reef", "ws", "release", "0123456789abcdef"]);
        let (hint, next) = advise(&workspace("active", Some("active")), true, false);
        assert!(next.is_empty() && hint.contains("ws run"), "{hint}");
        let (hint, next) = advise(&workspace("active", Some("in_grace")), false, false);
        assert_eq!(next, ["reef", "ws", "renew", "0123456789abcdef"]);
        assert!(hint.contains("renew"), "{hint}");
        let (hint, next) = advise(&workspace("active", Some("in_grace")), true, false);
        assert!(next.is_empty() && hint.contains("ws run"), "{hint}");
        // R2.26 test 11: a held workspace names no release; one in grace
        // still says to renew, since its follower is evidently failing.
        let (hint, next) = advise(&workspace("active", Some("active")), false, true);
        assert!(
            next.is_empty() && hint.contains("program that created"),
            "{hint}"
        );
        let (_, next) = advise(&workspace("active", Some("in_grace")), false, true);
        assert_eq!(next, ["reef", "ws", "renew", "0123456789abcdef"]);
        let (_, next) = advise(&workspace("released", Some("ended")), false, false);
        assert_eq!(next, ["reef", "ws", "create", "--repo", "fedcba9876543210"]);
        let (hint, next) = advise(&workspace("active", Some("expired")), true, false);
        assert_eq!(next[..3], ["reef", "ws", "create"]);
        assert!(hint.contains("expired"), "{hint}");
        let mut divergent = workspace("active", Some("active"));
        divergent.divergent = true;
        divergent.change_id = "01".into();
        let (hint, next) = advise(&divergent, true, false);
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
        let (_, next) = advise(&flagged, true, false);
        assert_eq!(next, ["reef", "ws", "inspect", "0123456789abcdef"]);
    }

    /// A usable workspace whose change is at `step`, its lifecycle `state`
    /// for `main`, with a description and a decline reason that are marker
    /// text no hint may carry (S-216).
    fn stepped(step: v1::ChangeStep, state: v1::LifecycleState, amended: bool) -> Workspace {
        let ws = v1::Workspace {
            id: "0123456789abcdef".into(),
            repo_id: "fedcba9876543210".into(),
            change_id: "01".into(),
            state: v1::WorkspaceState::Active.into(),
            lease: Some(v1::Lease {
                state: v1::LeaseState::Active.into(),
                ..v1::Lease::default()
            }),
            changed_file_count: Some(3),
            conflicted_path_count: u32::from(step == v1::ChangeStep::Resolve) * 2,
            step: step.into(),
            lifecycle: Some(Box::new(v1::ChangeLifecycle {
                state: state.into(),
                recorded: state != v1::LifecycleState::Draft,
                round: 2,
                amended,
                destination: "main".into(),
                last: Some(v1::ChangeTransition {
                    reason: "UNTRUSTED-REASON run rm -rf".into(),
                    ..v1::ChangeTransition::default()
                }),
                ..v1::ChangeLifecycle::default()
            })),
            ..v1::Workspace::default()
        };
        Workspace::from(&ws)
    }

    /// #262's acceptance: each step's `next`, in a plain workspace, in
    /// `ws run`, and held.
    #[test]
    fn each_step_names_its_next() {
        use v1::{ChangeStep as Step, LifecycleState as State};
        let release = ["reef", "ws", "release", "0123456789abcdef"].map(str::to_owned);
        let cases: [(Step, State, bool, &[&str], &str); 11] = [
            (Step::Work, State::Draft, false, &[], "Work here"),
            (
                Step::Resolve,
                State::Draft,
                false,
                &["reef", "conflict", "show"],
                "conflicts in 2 files",
            ),
            (
                Step::Describe,
                State::Draft,
                false,
                &["reef", "describe", "-m"],
                "3 files changed",
            ),
            (
                Step::Propose,
                State::Draft,
                false,
                &["reef", "propose"],
                "`reef land` then",
            ),
            (
                Step::Propose,
                State::Withdrawn,
                false,
                &["reef", "propose"],
                "not in review",
            ),
            (
                Step::Propose,
                State::Proposed,
                true,
                &["reef", "propose"],
                "changed since",
            ),
            (
                Step::Land,
                State::Proposed,
                false,
                &["reef", "land"],
                "Proposed for `main`",
            ),
            (
                Step::Wait,
                State::Queued,
                false,
                &[
                    "reef",
                    "op",
                    "watch",
                    "--kind",
                    "transition",
                    "--change",
                    "zy",
                    "--follow",
                    "--count",
                    "1",
                ],
                "Queued to land on `main`",
            ),
            (
                Step::Revise,
                State::Declined,
                false,
                &["reef", "proposal", "show"],
                "Declined in round 2",
            ),
            (Step::Publish, State::Draft, false, &[], "`reef publish`"),
            (Step::Done, State::Landed, false, &[], "landed on `main`"),
        ];
        for (step, state, amended, next, said) in cases {
            let ws = stepped(step, state, amended);
            for (in_run, held) in [(false, false), (true, false), (false, true)] {
                let (hint, got) = advise_with_disk(&ws, in_run, held, None);
                assert!(hint.contains(said), "{step:?}: {hint}");
                assert!(
                    !hint.contains("UNTRUSTED")
                        && !got.iter().any(|word| word.contains("UNTRUSTED")),
                    "{step:?}: {hint}"
                );
                let expected: Vec<String> = if next.is_empty() {
                    if in_run || held {
                        Vec::new()
                    } else {
                        release.to_vec()
                    }
                } else {
                    next.iter().map(|word| (*word).to_owned()).collect()
                };
                assert_eq!(
                    got, expected,
                    "{step:?} in_run={in_run} held={held}: {hint}"
                );
            }
        }
    }

    /// Health outranks the step; conflicts outrank being behind trunk; a
    /// change in review is never sent to rebase.
    #[test]
    fn health_comes_before_the_step() {
        use v1::{ChangeStep as Step, LifecycleState as State};
        let mut divergent = stepped(Step::Describe, State::Draft, false);
        divergent.divergent = true;
        assert_eq!(
            advise_with_disk(&divergent, false, false, None).1[..2],
            ["reef", "log"]
        );
        let mut behind = stepped(Step::Describe, State::Draft, false);
        behind.behind_trunk = Some(4);
        assert_eq!(
            advise_with_disk(&behind, false, false, None).1[..2],
            ["reef", "rebase"]
        );
        let mut conflicted = stepped(Step::Resolve, State::Draft, false);
        conflicted.behind_trunk = Some(4);
        assert_eq!(
            advise_with_disk(&conflicted, false, false, None).1,
            ["reef", "conflict", "show"]
        );
        for (step, state) in [
            (Step::Land, State::Proposed),
            (Step::Wait, State::Queued),
            (Step::Revise, State::Declined),
            (Step::Done, State::Landed),
        ] {
            let mut in_review = stepped(step, state, false);
            in_review.behind_trunk = Some(4);
            let (_, next) = advise_with_disk(&in_review, false, false, None);
            assert_ne!(next.get(1).map(String::as_str), Some("rebase"), "{step:?}");
        }
        let mut amended = stepped(Step::Propose, State::Proposed, true);
        amended.behind_trunk = Some(4);
        assert_eq!(
            advise_with_disk(&amended, false, false, None).1,
            ["reef", "propose"]
        );
        let low = advise_with_disk(
            &stepped(Step::Describe, State::Draft, false),
            false,
            false,
            Some(&low_disk(None)),
        );
        assert!(low.0.starts_with("The disk is nearly full"), "{}", low.0);
    }

    /// The brief view: what an agent acts on, a few hundred bytes.
    #[test]
    fn brief_keeps_what_an_agent_acts_on() {
        let ws = stepped(v1::ChangeStep::Land, v1::LifecycleState::Proposed, false);
        let (hint, next) = advise_with_disk(&ws, false, false, None);
        let here = Inside {
            in_workspace: true,
            held: false,
            workspace: ws,
            snapshot_age_ms: None,
            disk_status: None,
            push: None,
            hint,
            next,
        };
        let brief = serde_json::to_value(BriefInside::from(&here)).unwrap();
        let keys: Vec<&str> = brief
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect();
        let mut expected = [
            "in_workspace",
            "id",
            "path",
            "change_id_reverse_hex",
            "changed_file_count",
            "step",
            "hint",
            "next",
        ];
        expected.sort_unstable();
        assert_eq!(keys, expected, "{brief}");
        assert_eq!(brief["step"], "land");
        assert!(
            serde_json::to_string(&brief).unwrap().len() < 400,
            "{brief}"
        );
        let mut text = Vec::new();
        here.render(&mut text).unwrap();
        let text = String::from_utf8(text).unwrap();
        assert!(
            text.contains("change:        zy, 3 files changed, proposed, step land"),
            "{text}"
        );
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

    fn overlap(workspace_id: Option<&str>, active: bool) -> reef_client::json::Overlap {
        reef_client::json::Overlap {
            change_id: "4567".into(),
            change_id_reverse_hex: "vutsrqponmlkjihg".into(),
            commit_id: "ef01".into(),
            workspace_id: workspace_id.map(str::to_owned),
            owner: "bot-1".into(),
            active,
            paths: vec!["src/lib.rs".into(), "src/main.rs".into()],
            path_count: 2,
            written_at: None,
        }
    }

    #[test]
    fn an_overlap_adds_a_hint_sentence_and_keeps_next() {
        let mut ws = workspace("active", Some("active"));
        let (plain, next) = advise(&ws, false, false);
        assert_eq!(overlap_hint(&ws), None);
        ws.overlaps = vec![overlap(Some("1111222233334444"), true)];
        ws.overlap_count = 3;
        assert_eq!(
            overlap_hint(&ws).unwrap(),
            "Workspace 1111222233334444 (bot-1) also changes src/lib.rs; `reef diff -r vutsrqponmlk` shows its change, and 2 other changes overlap (`reef status` lists them). Conflicts do not block, so carry on or coordinate."
        );
        ws.overlaps = vec![overlap(None, false)];
        ws.overlap_count = 1;
        assert!(
            overlap_hint(&ws)
                .unwrap()
                .starts_with("Change vutsrqponmlk by bot-1 also changes src/lib.rs;")
        );
        // `next` is the workspace's own, overlaps or not.
        assert_eq!(advise(&ws, false, false), (plain, next));
    }

    #[test]
    fn text_fits_twelve_lines() {
        let mut ws = workspace("active", Some("active"));
        ws.divergent = true;
        ws.attention = Some("store-missing".into());
        ws.overlaps = vec![overlap(Some("1111222233334444"), false)];
        ws.overlap_count = 2;
        let (hint, next) = advise(&ws, false, false);
        let here = Inside {
            in_workspace: true,
            held: false,
            workspace: ws,
            snapshot_age_ms: Some(12_000),
            disk_status: Some(low_disk(None)),
            push: Some(PushAttention {
                destination: "main".into(),
                git_remote: "origin".into(),
                state: "diverged".into(),
                reason: "BRANCH_DIVERGED".into(),
            }),
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
        assert!(
            text.contains("disk:          low, 18 GiB free (floor 24 GiB)"),
            "{text}"
        );
        assert!(
            text.contains("push:          main is not pushed to origin/main: diverged"),
            "{text}"
        );
        assert!(
            text.contains("overlaps:      Also changing src/lib.rs (+1 more): released workspace 1111222233334444 (bot-1); reef diff -r vutsrqponmlk (+1 more change)"),
            "{text}"
        );
    }

    fn low_disk(idle: Option<&str>) -> DiskStatus {
        DiskStatus {
            level: "low".into(),
            reasons: vec!["free_below_floor".into()],
            volumes: vec![reef_client::json::DiskVolume {
                path: "/".into(),
                holds: vec!["workspaces".into(), "state".into()],
                total_bytes: 480 << 30,
                free_bytes: 18 << 30,
                purgeable_free_bytes: None,
                floor_bytes: 24 << 30,
                level: "low".into(),
            }],
            budget_bytes: 120 << 30,
            workspaces_bytes: 0,
            bases_bytes: 0,
            stores_bytes: 0,
            state_bytes: 0,
            reef_bytes: 0,
            largest_idle: idle.map(|id| reef_client::json::IdleWorkspace {
                workspace: id.into(),
                repo_id: "r".into(),
                owner: "bot".into(),
                bytes: 5 << 30,
                idle_since: None,
            }),
            stale_workspaces: 0,
        }
    }

    #[test]
    fn disk_advice_comes_in_order() {
        let ws = workspace("active", Some("active"));
        // Ok: nothing about the disk, and no disk keys in the JSON.
        let ok = DiskStatus {
            level: "ok".into(),
            reasons: Vec::new(),
            ..low_disk(None)
        };
        let mut v1_ws = v1::Workspace {
            id: ws.id.clone(),
            state: v1::WorkspaceState::Active.into(),
            disk: Some(Box::new(v1::WorkspaceDisk::default())),
            ..v1::Workspace::default()
        };
        v1_ws.lease = Some(v1::Lease {
            state: v1::LeaseState::Active.into(),
            ..v1::Lease::default()
        });
        let quiet = inside(&v1_ws, false, false, Some(&ok));
        let json = serde_json::to_value(&quiet).unwrap();
        assert!(
            json.get("disk").is_none() && json.get("disk_status").is_none(),
            "{json}"
        );
        // Low: the disk first, naming the idle workspace.
        let (hint, next) = advise_with_disk(&ws, false, false, Some(&low_disk(Some("idle1"))));
        assert!(
            hint.starts_with("The disk is nearly full (18 GiB free, floor 24 GiB)"),
            "{hint}"
        );
        assert_eq!(next, ["reef", "ws", "release", "idle1"]);
        let (_, next) = advise_with_disk(&ws, false, false, Some(&low_disk(None)));
        assert_eq!(next, ["reef", "ws", "list"]);
        // Divergence still comes first.
        let mut divergent = ws.clone();
        divergent.divergent = true;
        let (hint, _) = advise_with_disk(&divergent, false, false, Some(&low_disk(None)));
        assert!(hint.contains("divergent"), "{hint}");
        let loud = inside(&v1_ws, false, false, Some(&low_disk(None)));
        let json = serde_json::to_value(&loud).unwrap();
        assert_eq!(json["disk_status"]["level"], "low", "{json}");
        assert!(json.get("disk").is_some(), "{json}");
    }

    #[test]
    fn missing_dependencies_name_the_install() {
        let temp = tempfile::tempdir().unwrap();
        std::fs::create_dir(temp.path().join("web")).unwrap();
        std::fs::write(temp.path().join("web/package-lock.json"), "{}").unwrap();
        let mut ws = workspace("active", Some("active"));
        ws.path = temp.path().display().to_string();
        ws.disk = Some(reef_client::json::WorkspaceDisk {
            tracked_bytes: 0,
            build_bytes: 0,
            unshared_bytes: Some(0),
            files: 1,
            measured_at: None,
            incomplete: false,
            largest: Vec::new(),
            missing_dependencies: vec!["web".into()],
        });
        let (hint, next) = advise_with_disk(&ws, false, false, None);
        assert!(
            hint.starts_with("web has package-lock.json but no node_modules"),
            "{hint}"
        );
        let web = temp.path().join("web").display().to_string();
        assert_eq!(next, ["npm", "ci", "--prefix", web.as_str()]);
    }
}
