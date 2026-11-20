//! A seeded stress test of a stack of workspaces (R3.11, ADR 0084).
//!
//! Each seed builds a stack of two or three workspaces, each on the one
//! below, and runs scenes against it. A scene is a random script of agent
//! writes, snapshots, and describes and edits by change id, with the
//! daemon's hold hooks (`Scheduler::hold_commits`, `hold_follows`,
//! `hold_scans`) taken and released between them, so snapshots, writes and
//! their follow-ups interleave in ways a timer never would. Another jj
//! client on the registered root loads the head and commits later, so the
//! daemon merges its operation with the ones it wrote meanwhile. Half the
//! seeds end with the bottom change superseded by a new change, which
//! rebases the workspaces above onto it while they write; in half of
//! those a snapshot of the superseded change's workspace is held between
//! its scan and its commit when the supersede starts. After each scene
//! every workspace must hold, with no change divergent:
//!
//! - every file its agent wrote, with the bytes written, on disk and in
//!   its change (no lost edit);
//! - every file of the changes below it, on disk before any snapshot of it
//!   is asked for, and in its commit (no missed file);
//! - one visible commit of its change, the one its row names, also before
//!   any snapshot of it is asked for.
//!
//! A request that fails, or does not answer within [`HANG`], fails the seed
//! too. So does a supersede that does not move the workspace above: the
//! script gives it no reason to leave one (a divergent change, too many to
//! move), and one left with no reason named is never right.
//!
//! A seed's script is a function of the seed. Where a step lands against
//! requests still running depends on the scheduler: a step that does not
//! reach a hold is given [`SETTLE`] to finish before the next. Every
//! interleaving must pass, so a failure is real whichever one ran; the
//! failure prints the script.
//!
//! Knobs, as environment variables:
//!
//! - `REEF_STACK_STRESS_SEEDS`: how many seeds (default [`SEEDS`]);
//! - `REEF_STACK_STRESS_FIRST`: the first seed (default 1);
//! - `REEF_STACK_STRESS_JOBS`: daemons running seeds at once (default
//!   [`JOBS`]).

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::any::Any;
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::path::Path;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, CreateWorkspaceRequest, EditFilesRequest, FileEdit, ReadFileRequest,
    create_workspace_request, edit_files_request, file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;
use tokio::task::JoinHandle;

/// Seeds a plain test run covers.
const SEEDS: u64 = 12;
/// Daemons a plain test run uses.
const JOBS: u64 = 3;
/// Scenes per seed.
const SCENES: usize = 3;
/// How long a started request gets to finish, or to reach a hold, before
/// the next step.
const SETTLE: Duration = Duration::from_millis(40);
/// How long a workspace gets to follow the changes below it.
const FOLLOW: Duration = Duration::from_secs(20);
/// How long a request gets once every hold is released.
const HANG: Duration = Duration::from_secs(60);

/// `SplitMix64`: a seed's script needs no more.
struct Rng(u64);

impl Rng {
    fn next(&mut self) -> u64 {
        self.0 = self.0.wrapping_add(0x9e37_79b9_7f4a_7c15);
        let mut z = self.0;
        z = (z ^ (z >> 30)).wrapping_mul(0xbf58_476d_1ce4_e5b9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94d0_49bb_1331_11eb);
        z ^ (z >> 31)
    }

    fn below(&mut self, n: usize) -> usize {
        usize::try_from(self.next() % u64::try_from(n).unwrap()).unwrap()
    }
}

/// One workspace of the stack and what must be in it.
struct Member {
    ws: v1::Workspace,
    /// What its agent wrote, by path.
    written: BTreeMap<String, String>,
    /// What edits by change id put in its change, by path.
    added: BTreeMap<String, String>,
    /// Its change was superseded: the members above are no longer on it.
    superseded: bool,
}

/// The stack, bottom first, and the change that superseded the bottom
/// member's, with its one file.
struct Stack {
    /// The commit the bottom member's change is on.
    base: String,
    members: Vec<Member>,
    successor: Option<(String, String, String)>,
}

/// What the seeds of one daemon exercised.
#[derive(Default)]
struct Covered {
    /// Merges of two operation heads: another client's operation beside
    /// the daemon's.
    merges: usize,
    /// Supersedes, each of which moved the workspaces above.
    supersedes: usize,
    failures: Vec<(u64, String)>,
}

impl Stack {
    /// What workspace `at` must hold of the changes below it: each change,
    /// with its files.
    fn below(&self, at: usize) -> Vec<(&str, Vec<(&String, &String)>)> {
        let successor = self
            .successor
            .iter()
            .filter(|_| at > 0)
            .map(|(change, path, contents)| (change.as_str(), vec![(path, contents)]));
        let members = self.members[..at]
            .iter()
            .filter(|member| !member.superseded)
            .map(|member| (member.ws.change_id.as_str(), member.files().collect()));
        successor.chain(members).collect()
    }
}

impl Member {
    fn files(&self) -> impl Iterator<Item = (&String, &String)> {
        self.written.iter().chain(&self.added)
    }
}

#[derive(Clone, Copy, Debug)]
enum Step {
    /// The agent of workspace `.0` writes a file.
    Write(usize),
    /// A snapshot of workspace `.0` is asked for.
    Snapshot(usize),
    /// The change of workspace `.0` is described, by change id.
    Describe(usize),
    /// A file is added to the change of workspace `.0`, by change id.
    Add(usize),
    /// Snapshots of workspace `.0` wait between scan and commit.
    HoldCommit(usize),
    /// Writes wait between their write and their follow-ups.
    HoldFollows,
    /// Snapshots wait before their scan.
    HoldScans,
    /// Another jj client loads the head; it commits when released.
    HoldOther,
    /// The oldest hold is released.
    Release,
    /// The bottom change is superseded by a new change on its parent.
    Supersede,
}

/// What a finished request did to the model.
enum Done {
    Nothing,
    Added(usize, String, String),
    /// The successor and its file.
    Superseded(String, String, String),
}

struct Scene<'a> {
    daemon: &'a TestDaemon,
    repo: &'a SyntheticRepo,
    stack: &'a mut Stack,
    rng: &'a mut Rng,
    /// Names what this seed writes.
    tag: String,
    counter: usize,
    holds: Vec<(Step, Box<dyn Any + Send>)>,
    /// The supersede has been asked for.
    superseding: bool,
    tasks: Vec<(Step, JoinHandle<Result<Done, String>>)>,
    log: String,
}

impl Scene<'_> {
    fn pick(&mut self) -> Step {
        let member = self.rng.below(self.stack.members.len());
        let held = |holds: &[(Step, Box<dyn Any + Send>)], want: fn(&Step) -> bool| {
            holds.iter().any(|(step, _)| want(step))
        };
        loop {
            let step = match self.rng.below(100) {
                0..28 => Step::Write(member),
                28..49 => Step::Snapshot(member),
                49..58 => Step::Describe(member),
                58..67 => Step::Add(member),
                67..76 => Step::HoldCommit(member),
                76..83 => Step::HoldFollows,
                83..86 => Step::HoldScans,
                86..93 => Step::HoldOther,
                _ => Step::Release,
            };
            let possible = match step {
                Step::HoldCommit(_) => {
                    !held(&self.holds, |step| matches!(step, Step::HoldCommit(_)))
                }
                Step::HoldFollows => !held(&self.holds, |step| matches!(step, Step::HoldFollows)),
                Step::HoldScans => !held(&self.holds, |step| matches!(step, Step::HoldScans)),
                Step::HoldOther => !held(&self.holds, |step| matches!(step, Step::HoldOther)),
                Step::Release => !self.holds.is_empty(),
                // A final state: no write names a superseded change.
                Step::Describe(at) | Step::Add(at) => at > 0 || !self.superseding,
                _ => true,
            };
            if possible {
                return step;
            }
        }
    }

    fn held(&self) -> usize {
        let scheduler = self.daemon.handle.state().manager.scheduler();
        scheduler.held_commits() + scheduler.held_follows() + scheduler.held_scans()
    }

    /// Starts `request` and gives it [`SETTLE`] to finish or reach a hold.
    async fn start(
        &mut self,
        step: Step,
        request: impl Future<Output = Result<Done, String>> + Send + 'static,
    ) {
        let before = self.held();
        let task = tokio::spawn(request);
        let begun = Instant::now();
        while !task.is_finished() && self.held() <= before && begun.elapsed() < SETTLE {
            tokio::time::sleep(Duration::from_millis(1)).await;
        }
        self.tasks.push((step, task));
    }

    async fn run(&mut self, step: Step) {
        writeln!(self.log, "  {step:?}").unwrap();
        self.counter += 1;
        let name = format!("{}-{}", self.tag, self.counter);
        let client = self.daemon.client.clone();
        let repo = self.repo.id.clone();
        let scheduler = self.daemon.handle.state().manager.scheduler();
        match step {
            Step::Write(at) => {
                let member = &mut self.stack.members[at];
                // Mostly a new file; sometimes one it wrote before, again.
                let again = self.rng.below(10) < 3 && !member.written.is_empty();
                let path = if again {
                    let nth = self.rng.below(member.written.len());
                    member.written.keys().nth(nth).unwrap().clone()
                } else {
                    format!("w{at}/{name}.txt")
                };
                write(&member.ws, &path, &name);
                member.written.insert(path, name);
            }
            Step::Snapshot(at) => {
                let id = self.stack.members[at].ws.id.clone();
                self.start(step, async move {
                    let snapshot = client.snapshot_workspace(&repo, &id, "").await;
                    snapshot.map(|_| Done::Nothing).map_err(text)
                })
                .await;
            }
            Step::Describe(at) => {
                let change = self.stack.members[at].ws.change_id.clone();
                self.start(step, async move {
                    let request = v1::DescribeChangeRequest {
                        repo,
                        revision: change,
                        description: format!("{name}\n"),
                        ..v1::DescribeChangeRequest::default()
                    };
                    let described = client.describe_change(request).await;
                    described.map(|_| Done::Nothing).map_err(text)
                })
                .await;
            }
            Step::Add(at) => {
                let change = self.stack.members[at].ws.change_id.clone();
                let path = format!("e{at}/{name}.txt");
                let target = edit_files_request::Target::Revision(change);
                self.start(step, async move {
                    let edited = add(&client, repo, target, &path, &name).await;
                    edited.map(|_| Done::Added(at, path, name))
                })
                .await;
            }
            Step::HoldCommit(at) => {
                let id = self.stack.members[at].ws.id.parse().unwrap();
                let hold = scheduler.hold_commits(id).await;
                self.holds.push((step, Box::new(hold)));
            }
            Step::HoldFollows => {
                let hold = scheduler.hold_follows().await;
                self.holds.push((step, Box::new(hold)));
            }
            Step::HoldScans => {
                let hold = scheduler.hold_scans().await;
                self.holds.push((step, Box::new(hold)));
            }
            Step::HoldOther => {
                let (root, on) = (self.repo.root.clone(), self.stack.base.clone());
                let load = move || OtherClient::load(&root, &name, &on);
                let other = tokio::task::spawn_blocking(load).await.unwrap();
                self.holds.push((step, Box::new(other)));
            }
            Step::Supersede => {
                self.superseding = true;
                let change = self.stack.members[0].ws.change_id.clone();
                let base = self.stack.base.clone();
                self.start(step, supersede(client, repo, change, base, name))
                    .await;
            }
            Step::Release => {
                drop(self.holds.remove(0));
                // What the hold held runs on before the next step.
                tokio::time::sleep(SETTLE).await;
            }
        }
    }

    /// Runs the scene's script. With `supersede`, the bottom change is
    /// superseded part-way, half the time while a snapshot of its workspace
    /// waits between its scan and its commit.
    async fn play(&mut self, supersede: bool) {
        let steps = 6 + self.rng.below(6);
        let supersede_at = self.rng.below(steps);
        let racing_a_snapshot = self.rng.below(2) == 0;
        for nth in 0..steps {
            if !supersede || nth != supersede_at {
                let step = self.pick();
                self.run(step).await;
                continue;
            }
            if racing_a_snapshot {
                // The superseded change's commit moves under the supersede.
                self.run(Step::Write(0)).await;
                let held = |(step, _): &(Step, _)| matches!(step, Step::HoldCommit(_));
                if !self.holds.iter().any(held) {
                    self.run(Step::HoldCommit(0)).await;
                }
                self.run(Step::Snapshot(0)).await;
            }
            self.run(Step::Supersede).await;
        }
    }

    /// Releases every hold and waits for every request.
    async fn finish(&mut self) -> Result<(), String> {
        self.holds.clear();
        for (step, task) in std::mem::take(&mut self.tasks) {
            let done = tokio::time::timeout(HANG, task)
                .await
                .map_err(|_| format!("{step:?} did not answer in {HANG:?}"))?
                .map_err(|error| format!("{step:?} panicked: {error}"))?
                .map_err(|error| format!("{step:?} failed: {error}"))?;
            match done {
                Done::Nothing => {}
                Done::Added(at, path, contents) => {
                    self.stack.members[at].added.insert(path, contents);
                }
                Done::Superseded(successor, path, contents) => {
                    self.stack.members[0].superseded = true;
                    self.stack.successor = Some((successor, path, contents));
                }
            }
        }
        Ok(())
    }
}

/// Adds `path`, holding `contents`, to `target`; returns the change.
async fn add(
    client: &Client,
    repo: String,
    target: edit_files_request::Target,
    path: &str,
    contents: &str,
) -> Result<String, String> {
    let request = EditFilesRequest {
        repo,
        target: Some(target),
        edits: vec![FileEdit {
            path: path.to_owned(),
            action: Some(file_edit::Action::Content(contents.as_bytes().to_vec())),
            executable: None,
        }],
        ..EditFilesRequest::default()
    };
    let edited = client.edit_files(request).await.map_err(text)?;
    Ok(edited.change.unwrap().change_id)
}

/// Supersedes `change` by a new change on `base` holding one file.
async fn supersede(
    client: Client,
    repo: String,
    change: String,
    base: String,
    name: String,
) -> Result<Done, String> {
    let path = format!("sup/{name}.txt");
    let target = edit_files_request::Target::NewOn(base);
    let successor = add(&client, repo.clone(), target, &path, &name).await?;
    let request = v1::TransitionChangeRequest {
        repo,
        revision: change,
        to: v1::LifecycleState::Superseded.into(),
        superseded_by: successor.clone(),
        ..v1::TransitionChangeRequest::default()
    };
    let superseded = client.transition_change(request).await.map_err(text)?;
    // The workspace above is a dependent (ADR 0084, decision 6).
    let moved = &superseded.moved;
    if moved.is_empty() || moved.iter().any(|moved| !moved.skipped.is_empty()) {
        return Err(format!("it did not move every dependent: {moved:?}"));
    }
    Ok(Done::Superseded(successor, path, name))
}

#[allow(
    clippy::needless_pass_by_value,
    reason = "the function `map_err` takes"
)]
fn text(error: ClientError) -> String {
    format!("{error:?}")
}

fn write(ws: &v1::Workspace, path: &str, contents: &str) {
    let path = Path::new(&ws.path).join(path);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, contents).unwrap();
}

fn on_disk(ws: &v1::Workspace, path: &str) -> Option<String> {
    std::fs::read_to_string(Path::new(&ws.path).join(path)).ok()
}

/// `path` at `revision`; `None` when the commit has none.
async fn committed(
    client: &Client,
    repo: &SyntheticRepo,
    revision: &str,
    path: &str,
) -> Result<Option<String>, String> {
    let request = ReadFileRequest {
        repo: repo.id.clone(),
        revision: revision.to_owned(),
        path: path.to_owned(),
        skip_snapshot: true,
        ..ReadFileRequest::default()
    };
    match client.read_file_to_end(request).await {
        Ok(response) => Ok(Some(String::from_utf8(response.content).unwrap())),
        Err(ClientError::Rpc { reason, .. }) if reason == "PATH_NOT_FOUND" => Ok(None),
        Err(error) => Err(format!("cannot read {path} at {revision}: {error:?}")),
    }
}

async fn row(client: &Client, repo: &SyntheticRepo, ws: &v1::Workspace) -> v1::Workspace {
    let got = client.inspect_workspace(&repo.id, &ws.id, false).await;
    got.unwrap().workspace.unwrap()
}

/// `Err` when a member's change has diverged. Else what is still behind, if
/// anything: a row not yet on its change's one visible commit.
async fn behind(
    client: &Client,
    repo: &SyntheticRepo,
    stack: &Stack,
) -> Result<Option<String>, String> {
    let request = v1::ListChangesRequest {
        repo: repo.id.clone(),
        revset: "mutable()".to_owned(),
        skip_snapshot: true,
        ..v1::ListChangesRequest::default()
    };
    let changes = client.list_changes(request).await.map_err(text)?;
    let mut behind = None;
    for (at, member) in stack.members.iter().enumerate() {
        let now = row(client, repo, &member.ws).await;
        let commits: Vec<&str> = changes
            .iter()
            .filter_map(|entry| entry.change.as_ref())
            .filter(|change| change.change_id == member.ws.change_id)
            .map(|change| change.commit_id.as_str())
            .collect();
        let state = format!(
            "its change {} has commits {commits:?}; its row is on change {} at {}, divergent: {}",
            member.ws.change_id, now.change_id, now.commit_id, now.divergent
        );
        if now.divergent || now.change_id != member.ws.change_id || commits.len() != 1 {
            return Err(format!("workspace {at} diverged: {state}"));
        }
        if commits != [&now.commit_id] {
            behind = Some(format!("workspace {at} is behind: {state}"));
        }
    }
    Ok(behind)
}

/// The first file a change below workspace `at` has committed that the
/// workspace's directory does not hold.
async fn missing_below(
    client: &Client,
    repo: &SyntheticRepo,
    stack: &Stack,
    at: usize,
) -> Result<Option<String>, String> {
    for (change, files) in stack.below(at) {
        for (path, contents) in files {
            let theirs = committed(client, repo, change, path).await?;
            if theirs.as_ref() == Some(contents)
                && on_disk(&stack.members[at].ws, path).as_ref() != Some(contents)
            {
                return Ok(Some(format!(
                    "workspace {at} misses {path}, committed below it"
                )));
            }
        }
    }
    Ok(None)
}

/// Every workspace follows the changes below it, with no snapshot of it
/// asked for (#202): its row reaches its change's commit and its directory
/// holds their files, within [`FOLLOW`]. A change that diverges fails at
/// once.
async fn follows(client: &Client, repo: &SyntheticRepo, stack: &Stack) -> Result<(), String> {
    let begun = Instant::now();
    loop {
        let mut waiting = behind(client, repo, stack).await?;
        for at in 1..stack.members.len() {
            if waiting.is_none() {
                waiting = missing_below(client, repo, stack, at).await?;
            }
        }
        let Some(waiting) = waiting else {
            return Ok(());
        };
        if begun.elapsed() > FOLLOW {
            return Err(format!("{waiting}, after {FOLLOW:?}"));
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// The scene's end state: see the module docs.
async fn check(client: &Client, repo: &SyntheticRepo, stack: &Stack) -> Result<(), String> {
    follows(client, repo, stack).await?;
    for member in &stack.members {
        let snapshot = client.snapshot_workspace(&repo.id, &member.ws.id, "").await;
        snapshot.map_err(|error| format!("the closing snapshot failed: {error:?}"))?;
    }
    follows(client, repo, stack).await?;
    for (at, member) in stack.members.iter().enumerate() {
        let commit = row(client, repo, &member.ws).await.commit_id;
        let own = ("lost", member.files().collect::<Vec<_>>());
        let below = stack
            .below(at)
            .into_iter()
            .map(|(_, files)| ("missed", files));
        for (what, files) in below.chain([own]) {
            for (path, contents) in files {
                let disk = on_disk(&member.ws, path);
                if disk.as_ref() != Some(contents) {
                    return Err(format!(
                        "workspace {at} {what} {path} on disk: {disk:?}, not {contents:?}"
                    ));
                }
                let stored = committed(client, repo, &commit, path).await?;
                if stored.as_ref() != Some(contents) {
                    return Err(format!(
                        "workspace {at} {what} {path} in its commit {commit}: {stored:?}, not {contents:?}"
                    ));
                }
            }
        }
    }
    Ok(())
}

async fn create(client: &Client, repo: &SyntheticRepo, at: usize, on: Option<String>) -> Member {
    let request = CreateWorkspaceRequest {
        repo: repo.id.clone(),
        owner: format!("agent-{at}@test"),
        start: on.map(create_workspace_request::Start::NewOn),
        ..CreateWorkspaceRequest::default()
    };
    let ws = client.create_workspace(request).await.unwrap();
    Member {
        ws: ws.workspace.unwrap(),
        written: BTreeMap::new(),
        added: BTreeMap::new(),
        superseded: false,
    }
}

/// One seed: a stack, then [`SCENES`] scenes, each checked. In half the
/// seeds the last scene supersedes the bottom change part-way. Returns
/// whether it did.
async fn seed(daemon: &TestDaemon, repo: &SyntheticRepo, seed: u64) -> Result<bool, String> {
    let client = &daemon.client;
    let mut rng = Rng(seed);
    let depth = 2 + rng.below(2);
    let supersedes = rng.below(2) == 0;
    let mut stack = Stack {
        base: String::new(),
        members: Vec::new(),
        successor: None,
    };
    for at in 0..depth {
        let on = match stack.members.last() {
            Some(below) => Some(row(client, repo, &below.ws).await.commit_id),
            None => None,
        };
        let mut member = create(client, repo, at, on).await;
        let (path, contents) = (format!("w{at}/s{seed}-0.txt"), format!("s{seed}-0"));
        write(&member.ws, &path, &contents);
        member.written.insert(path, contents);
        client
            .snapshot_workspace(&repo.id, &member.ws.id, "")
            .await
            .unwrap();
        stack.members.push(member);
    }
    let request = v1::GetChangeRequest {
        repo: repo.id.clone(),
        revision: stack.members[0].ws.change_id.clone(),
        skip_snapshot: true,
        ..v1::GetChangeRequest::default()
    };
    let bottom = client.get_change(request).await.unwrap().change.unwrap();
    stack.base.clone_from(&bottom.parent_commit_ids[0]);
    if supersedes {
        // A proposal needs a destination.
        bookmark_main(repo, &stack.base).await;
    }
    let mut log = format!("seed {seed}, {depth} workspaces\n");
    let mut failure = None;
    for scene in 0..SCENES {
        writeln!(log, " scene {scene}").unwrap();
        let last = scene + 1 == SCENES;
        if last && supersedes {
            // Only a proposed change can be superseded.
            let request = v1::TransitionChangeRequest {
                repo: repo.id.clone(),
                revision: stack.members[0].ws.change_id.clone(),
                to: v1::LifecycleState::Proposed.into(),
                ..v1::TransitionChangeRequest::default()
            };
            client.transition_change(request).await.unwrap();
        }
        let mut scene = Scene {
            daemon,
            repo,
            stack: &mut stack,
            rng: &mut rng,
            tag: format!("s{seed}"),
            counter: scene * 100,
            holds: Vec::new(),
            superseding: false,
            tasks: Vec::new(),
            log: String::new(),
        };
        scene.play(last && supersedes).await;
        let finished = scene.finish().await;
        log.push_str(&scene.log);
        let checked = match finished {
            Ok(()) => check(client, repo, &stack).await,
            Err(error) => Err(error),
        };
        if let Err(error) = checked {
            failure = Some(error);
            break;
        }
    }
    for member in &stack.members {
        // Kept small: later seeds list every change.
        let released = client
            .release_workspace(&repo.id, &member.ws.id, false, true)
            .await;
        if failure.is_none() {
            released.map_err(|error| format!("cannot release: {error:?}"))?;
        }
    }
    match failure {
        Some(error) => Err(format!("{error}\n{log}")),
        None => Ok(supersedes),
    }
}

/// How many operations of `repo` merge two operation heads.
async fn merges(client: &Client, repo: &SyntheticRepo) -> usize {
    let request = v1::ListOperationsRequest {
        repo: repo.id.clone(),
        ..v1::ListOperationsRequest::default()
    };
    let operations = client.list_operations_limited(request, 0).await.unwrap();
    operations
        .items
        .iter()
        .filter(|operation| operation.parent_ids.len() > 1)
        .count()
}

/// A transaction of another jj client on the registered root, loaded at
/// the head now, that sets bookmark `name` on `commit`.
fn bookmark_set(root: &Path, name: &str, commit: &str) -> jj_lib::transaction::Transaction {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories, default_workspace_loader_factory,
    };
    let workspace = default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &test_settings(),
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        jj_lib::ref_name::RefName::new(name),
        jj_lib::op_store::RefTarget::normal(
            jj_lib::backend::CommitId::try_from_hex(commit).unwrap(),
        ),
    );
    tx
}

/// Sets the `main` bookmark from outside the daemon, as `jj bookmark set`.
async fn bookmark_main(repo: &SyntheticRepo, commit: &str) {
    let (root, commit) = (repo.root.clone(), commit.to_owned());
    let set = move || {
        let tx = bookmark_set(&root, "main", &commit);
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    };
    tokio::task::spawn_blocking(set).await.unwrap();
}

/// Another jj client on the registered root: it loaded the head when made,
/// and commits a bookmark when dropped. Its operation is concurrent with
/// every operation the daemon wrote in between, so the daemon's next load
/// merges the two operation heads.
struct OtherClient {
    go: std::sync::mpsc::Sender<()>,
    done: Option<std::thread::JoinHandle<()>>,
}

impl OtherClient {
    fn load(root: &Path, name: &str, commit: &str) -> Self {
        let (root, name, commit) = (root.to_owned(), name.to_owned(), commit.to_owned());
        let (go, told) = std::sync::mpsc::channel();
        let (tell, loaded) = std::sync::mpsc::channel();
        let done = std::thread::spawn(move || {
            let tx = bookmark_set(&root, &name, &commit);
            tell.send(()).unwrap();
            if told.recv().is_ok() {
                pollster::block_on(tx.commit("another client's bookmark")).unwrap();
            }
        });
        loaded.recv().unwrap();
        Self {
            go,
            done: Some(done),
        }
    }
}

impl Drop for OtherClient {
    fn drop(&mut self) {
        let _ = self.go.send(());
        if let Some(done) = self.done.take() {
            done.join().unwrap();
        }
    }
}

fn knob(name: &str, default: u64) -> u64 {
    match std::env::var(name) {
        Ok(value) => value
            .parse()
            .unwrap_or_else(|_| panic!("{name}={value} is not a number")),
        Err(_) => default,
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn a_stack_survives_random_interleavings() {
    let seeds = knob("REEF_STACK_STRESS_SEEDS", SEEDS);
    let first = knob("REEF_STACK_STRESS_FIRST", 1);
    let jobs = knob("REEF_STACK_STRESS_JOBS", JOBS).clamp(1, seeds.max(1));
    let begun = Instant::now();
    let runs = (0..jobs).map(|job| async move {
        let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
            no_automatic_snapshots(config);
        })
        .await;
        let mut covered = Covered::default();
        for seed_number in (first..first + seeds).filter(|seed| seed % jobs == job) {
            match seed(&daemon, &repo, seed_number).await {
                Ok(superseded) => covered.supersedes += usize::from(superseded),
                Err(error) => covered.failures.push((seed_number, error)),
            }
        }
        covered.merges = merges(&daemon.client, &repo).await;
        daemon.stop().await;
        covered
    });
    let covered = futures::future::join_all(runs).await;
    let count = |of: fn(&Covered) -> usize| covered.iter().map(of).sum::<usize>();
    let (merged, supersedes) = (count(|job| job.merges), count(|job| job.supersedes));
    let mut failures: Vec<(u64, String)> =
        covered.into_iter().flat_map(|job| job.failures).collect();
    failures.sort();
    println!(
        "stack stress: {seeds} seeds from {first}, {} failed, in {:?}; {merged} merges of operation heads, {supersedes} supersedes",
        failures.len(),
        begun.elapsed()
    );
    let report: Vec<String> = failures
        .iter()
        .map(|(seed, error)| format!("seed {seed}: {error}"))
        .collect();
    assert!(failures.is_empty(), "{}", report.join("\n"));
}
