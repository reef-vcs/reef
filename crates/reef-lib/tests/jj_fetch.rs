//! `reef_lib::jj::fetch` (#259): the plan, the fetch on a dropped
//! transaction, and the record, against a colocated repository whose
//! `origin` is a bare git repository advanced from a second clone; and the
//! record checked against jj-lib's own import of the same fetch (test 3).
//! Each test prints a note and returns when `git` is not on `PATH`.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]
#![allow(clippy::print_stderr, reason = "a skip note when git is missing")]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::Arc;

use jj_lib::git::{GitFetch, GitFetchRefExpression, GitImportOptions};
use jj_lib::object_id::ObjectId;
use jj_lib::ref_name::{RefName, RemoteName};
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use jj_lib::settings::UserSettings;
use jj_lib::str_util::{StringExpression, StringPattern};
use reef_lib::jj::fetch::{self, FetchError};
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::{OpKind, OpMeta, Repo};
use tempfile::TempDir;

fn has_git() -> bool {
    let found = Command::new("git").arg("--version").output().is_ok();
    if !found {
        eprintln!("skipping: git is not on PATH");
    }
    found
}

fn settings() -> UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

/// Runs git with an identity (CI runners have none).
fn git(dir: &Path, args: &[&str]) -> String {
    let output = Command::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_AUTHOR_NAME", "Robin")
        .env("GIT_AUTHOR_EMAIL", "robin@example.com")
        .env("GIT_COMMITTER_NAME", "Robin")
        .env("GIT_COMMITTER_EMAIL", "robin@example.com")
        .output()
        .unwrap();
    assert!(
        output.status.success(),
        "git {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

struct Fixture {
    _dir: TempDir,
    root: PathBuf,
    other: PathBuf,
    repo: Repo,
    base: String,
}

impl Fixture {
    /// A git checkout with `origin`, its jj side created as `reef init`
    /// does, and a second clone of `origin`. With `detached`, git's HEAD is
    /// detached, so no local bookmark is git's checked-out branch.
    async fn new(detached: bool) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("repo");
        std::fs::create_dir(&root).unwrap();
        let base = reef_testing::git::init_repo(&root, &[("README.md", "hello\n")]);
        let remote = dir.path().join("remote.git");
        git(dir.path(), &["init", "--bare", "--quiet", "remote.git"]);
        git(
            &root,
            &["remote", "add", "origin", remote.to_str().unwrap()],
        );
        git(&root, &["push", "--quiet", "origin", "main"]);
        git(&root, &["fetch", "--quiet", "origin"]);
        git(&remote, &["symbolic-ref", "HEAD", "refs/heads/main"]);
        if detached {
            std::fs::write(root.join(".git/HEAD"), format!("{base}\n")).unwrap();
        }
        let other = dir.path().join("other");
        git(
            dir.path(),
            &["clone", "--quiet", remote.to_str().unwrap(), "other"],
        );
        let mut repo = Repo::init_in_git_repo(&root, &settings(), None)
            .await
            .unwrap();
        // As registration does: git's refs are jj's bookmarks.
        repo.import_git_refs(&OpMeta::new(OpKind::ImportRefs, "reef: import git refs"))
            .await
            .unwrap();
        Fixture {
            _dir: dir,
            root,
            other,
            repo,
            base,
        }
    }

    fn advance(&self, file: &str) -> String {
        std::fs::write(self.other.join(file), "upstream\n").unwrap();
        git(&self.other, &["add", "-A"]);
        git(&self.other, &["commit", "--quiet", "-m", "upstream"]);
        git(&self.other, &["push", "--quiet", "origin", "HEAD:main"]);
        git(&self.other, &["rev-parse", "HEAD"])
    }

    fn head(&self) -> Arc<ReadonlyRepo> {
        self.repo.readonly()
    }
}

fn remote_main(repo: &ReadonlyRepo) -> Option<String> {
    repo.view()
        .get_remote_bookmark(RefName::new("main").to_remote_symbol(RemoteName::new("origin")))
        .target
        .as_normal()
        .map(ObjectId::hex)
}

fn local_main(repo: &ReadonlyRepo) -> Option<String> {
    repo.view()
        .get_local_bookmark(RefName::new("main"))
        .as_normal()
        .map(ObjectId::hex)
}

/// A callback that ignores git's output.
struct Quiet;

impl jj_lib::git::GitSubprocessCallback for Quiet {
    fn needs_progress(&self) -> bool {
        false
    }

    fn progress(&mut self, _: &jj_lib::git::GitProgress) -> std::io::Result<()> {
        Ok(())
    }

    fn local_sideband(
        &mut self,
        _: &[u8],
        _: Option<jj_lib::git::GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        Ok(())
    }

    fn remote_sideband(
        &mut self,
        _: &[u8],
        _: Option<jj_lib::git::GitSidebandLineTerminator>,
    ) -> std::io::Result<()> {
        Ok(())
    }
}

fn meta() -> OpMeta {
    OpMeta::new(OpKind::Fetch, "reef: fetch main from origin")
}

// 1, 2. The plan picks origin and trunk's branch; the fetch leaves the view
// alone; the record moves the remote-tracking bookmark in one operation;
// a second fetch records nothing; a branch the remote lacks is missing.
#[tokio::test]
async fn fetch_and_record() {
    if !has_git() {
        return;
    }
    let mut fixture = Fixture::new(true).await;
    let upstream = fixture.advance("up.txt");
    let head = fixture.head();
    let plan = fetch::plan_fetch(&head, "", &[]).unwrap();
    assert_eq!(plan.remote, "origin");
    assert_eq!(plan.branches, ["main"]);
    let fetched = fetch::fetch(&head, &plan, &|_| None).unwrap();
    assert_eq!(fetched.refs[0].commit.as_ref().unwrap().as_str(), upstream);
    assert_eq!(
        remote_main(&fixture.head()).as_deref(),
        Some(fixture.base.as_str())
    );

    let recorded = fixture.repo.record_fetch(&fetched, &meta()).await.unwrap();
    assert!(recorded.op_id.is_some());
    assert_eq!(
        remote_main(&fixture.head()).as_deref(),
        Some(upstream.as_str())
    );
    let again = fetch::fetch(&fixture.head(), &plan, &|_| None).unwrap();
    let recorded = fixture.repo.record_fetch(&again, &meta()).await.unwrap();
    assert!(recorded.op_id.is_none(), "nothing changed");

    let missing = fetch::plan_fetch(&fixture.head(), "", &["nope".to_owned()]).unwrap();
    let fetched = fetch::fetch(&fixture.head(), &missing, &|_| None).unwrap();
    assert!(fetched.refs[0].commit.is_none());
    let recorded = fixture.repo.record_fetch(&fetched, &meta()).await.unwrap();
    assert!(recorded.branches[0].missing);
    assert!(recorded.op_id.is_none());

    // Names refused as a publish refuses them.
    let refused = fetch::plan_fetch(&fixture.head(), "", &["a..b".to_owned()]).unwrap_err();
    assert!(matches!(refused, FetchError::Plan(_)), "{refused:?}");
    let refused = fetch::plan_fetch(&fixture.head(), "nope", &[]).unwrap_err();
    assert!(matches!(refused, FetchError::Plan(_)), "{refused:?}");
}

// 3. For the same fetched state, the record and jj-lib's own fetch import,
// committed from the same operation, give the same remote and local
// bookmarks, and the fetched commit is visible in both.
#[tokio::test]
async fn record_matches_jj() {
    if !has_git() {
        return;
    }
    let mut fixture = Fixture::new(true).await;
    let upstream = fixture.advance("up.txt");
    let before = fixture.head();
    let plan = fetch::plan_fetch(&before, "", &[]).unwrap();
    let fetched = fetch::fetch(&before, &plan, &|_| None).unwrap();
    fixture.repo.record_fetch(&fetched, &meta()).await.unwrap();
    let ours = fixture.head();

    let mut options = jj_lib::git::GitSettings::from_settings(before.settings())
        .unwrap()
        .to_subprocess_options();
    options.environment = HashMap::new();
    let import_options = GitImportOptions {
        abandon_unreachable_commits: false,
        record_synthetic_predecessors: false,
        remote_auto_track_bookmarks: HashMap::new(),
    };
    let mut tx = before.start_transaction();
    {
        let mut git_fetch = GitFetch::new(tx.repo_mut(), options, &import_options).unwrap();
        git_fetch
            .fetch(
                RemoteName::new("origin"),
                &GitFetchRefExpression {
                    bookmark: StringExpression::pattern(StringPattern::exact("main")),
                    tag: StringExpression::none(),
                },
                &mut Quiet,
                None,
            )
            .unwrap();
        git_fetch.import_refs().await.unwrap();
    }
    let theirs = tx.commit("jj's fetch").await.unwrap();

    assert_eq!(remote_main(&ours), remote_main(&theirs));
    assert_eq!(remote_main(&ours).as_deref(), Some(upstream.as_str()));
    assert_eq!(local_main(&ours), local_main(&theirs));
    let state = |repo: &ReadonlyRepo| {
        repo.view()
            .get_remote_bookmark(RefName::new("main").to_remote_symbol(RemoteName::new("origin")))
            .state
    };
    assert_eq!(state(&ours), state(&theirs));
    let upstream_id = jj_lib::backend::CommitId::try_from_hex(&upstream).unwrap();
    for repo in [&ours, &theirs] {
        assert!(repo.index().has_id(&upstream_id).await.unwrap());
    }
    let _ = &fixture.root;
}

// 4 (S-183). git's checked-out branch is never moved: the remote bookmark
// moves, the local one stays, and the record says so.
#[tokio::test]
async fn checked_out() {
    if !has_git() {
        return;
    }
    let mut fixture = Fixture::new(false).await;
    let upstream = fixture.advance("up.txt");
    let head = fixture.head();
    let plan = fetch::plan_fetch(&head, "", &[]).unwrap();
    let fetched = fetch::fetch(&head, &plan, &|_| None).unwrap();
    let recorded = fixture.repo.record_fetch(&fetched, &meta()).await.unwrap();
    assert!(!recorded.branches[0].local_moved);
    assert_eq!(
        remote_main(&fixture.head()).as_deref(),
        Some(upstream.as_str())
    );
    assert_eq!(
        local_main(&fixture.head()).as_deref(),
        Some(fixture.base.as_str())
    );
}

/// A commit "landed" on the local `main` (R3.17's tests): written with
/// git's plumbing on `parent` (the working tree is not touched), set as
/// git's `main`, and imported, as a landing leaves the destination ahead of
/// the remote.
async fn land_locally(fixture: &mut Fixture, parent: &str, file: &str) -> String {
    let index = fixture.root.join(".git/reef-test-index");
    let plumbing = |args: &[&str], input: Option<&str>| {
        use std::io::Write as _;
        let mut child = Command::new("git")
            .arg("-C")
            .arg(&fixture.root)
            .args(args)
            .env("GIT_INDEX_FILE", &index)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_AUTHOR_NAME", "Robin")
            .env("GIT_AUTHOR_EMAIL", "robin@example.com")
            .env("GIT_COMMITTER_NAME", "Robin")
            .env("GIT_COMMITTER_EMAIL", "robin@example.com")
            .stdin(std::process::Stdio::piped())
            .stdout(std::process::Stdio::piped())
            .spawn()
            .unwrap();
        child
            .stdin
            .take()
            .unwrap()
            .write_all(input.unwrap_or_default().as_bytes())
            .unwrap();
        let output = child.wait_with_output().unwrap();
        assert!(output.status.success(), "git {args:?}");
        String::from_utf8(output.stdout).unwrap().trim().to_owned()
    };
    plumbing(&["read-tree", parent], None);
    let blob = plumbing(&["hash-object", "-w", "--stdin"], Some("landed\n"));
    plumbing(
        &[
            "update-index",
            "--add",
            "--cacheinfo",
            &format!("100644,{blob},{file}"),
        ],
        None,
    );
    let tree = plumbing(&["write-tree"], None);
    let id = plumbing(
        &[
            "commit-tree",
            &tree,
            "-p",
            parent,
            "-m",
            &format!("Land {file}"),
        ],
        None,
    );
    let _ = std::fs::remove_file(&index);
    git(&fixture.root, &["update-ref", "refs/heads/main", &id]);
    fixture
        .repo
        .import_git_refs(&OpMeta::new(OpKind::ImportRefs, "reef: import git refs"))
        .await
        .unwrap();
    assert_eq!(local_main(&fixture.head()).as_deref(), Some(id.as_str()));
    id
}

/// Someone else pushes a commit writing `file` on top of the remote's `main`.
fn push_on_remote(fixture: &Fixture, file: &str) -> String {
    git(&fixture.other, &["fetch", "--quiet", "origin"]);
    git(
        &fixture.other,
        &["reset", "--quiet", "--hard", "origin/main"],
    );
    std::fs::write(fixture.other.join(file), "theirs\n").unwrap();
    git(&fixture.other, &["add", "-A"]);
    git(
        &fixture.other,
        &["commit", "--quiet", "-m", &format!("Theirs {file}")],
    );
    git(&fixture.other, &["push", "--quiet", "origin", "HEAD:main"]);
    git(&fixture.other, &["rev-parse", "HEAD"])
}

fn remote_state(repo: &ReadonlyRepo) -> jj_lib::op_store::RemoteRefState {
    repo.view()
        .get_remote_bookmark(RefName::new("main").to_remote_symbol(RemoteName::new("origin")))
        .state
}

async fn fetch_following(fixture: &mut Fixture) -> fetch::RecordedFetch {
    let head = fixture.head();
    let plan = fetch::plan_fetch(&head, "origin", &["main".to_owned()]).unwrap();
    let fetched = fetch::fetch(&head, &plan, &|_| None).unwrap();
    fixture
        .repo
        .record_fetch_following(&fetched, &["main".to_owned()], &meta())
        .await
        .unwrap()
}

// R3.17 tests 1 and 2: a landing destination's head is planned against its
// remote branch (up to date, fast forward, remote ahead, diverged; trunk and
// immutability are no reason to refuse), pushed, and recorded with the
// remote bookmark's tracking state kept and the local bookmark untouched.
#[tokio::test]
async fn landed_push_plans_and_records() {
    use reef_lib::jj::push::{self, LandedUpdate, PushError, PushMode};
    if !has_git() {
        return;
    }
    let mut fixture = Fixture::new(true).await;
    let plan = |repo: Arc<ReadonlyRepo>| async move {
        push::plan_landed_push(&repo, "main", "origin", &[], 1_000).await
    };
    let up = plan(fixture.head()).await.unwrap();
    assert_eq!(up.update, LandedUpdate::UpToDate);
    assert!(up.plan.is_none());
    assert!(matches!(
        push::plan_landed_push(&fixture.head(), "nope", "origin", &[], 1_000).await,
        Err(PushError::DestinationNotFound(_))
    ));
    assert!(matches!(
        push::plan_landed_push(&fixture.head(), "main", "https://h/x", &[], 1_000).await,
        Err(PushError::InvalidRemote(_))
    ));
    assert!(matches!(
        push::plan_landed_push(
            &fixture.head(),
            "main",
            "origin",
            &["fork".to_owned()],
            1_000
        )
        .await,
        Err(PushError::RemoteNotAllowed(_))
    ));

    let base = fixture.base.clone();
    let landed = land_locally(&mut fixture, &base, "a.txt").await;
    let tracking = remote_state(&fixture.head());
    let ahead = plan(fixture.head()).await.unwrap();
    assert_eq!(ahead.update, LandedUpdate::FastForward);
    let push_plan = ahead.plan.clone().unwrap();
    assert_eq!(push_plan.mode, PushMode::Landing);
    assert_eq!(push_plan.range.len(), 1);
    assert_eq!(push_plan.previous.as_ref().unwrap().as_str(), base);
    assert!(matches!(
        push::plan_landed_push(&fixture.head(), "main", "origin", &[], 0).await,
        Err(PushError::RangeTooLarge { .. })
    ));
    push::push(&fixture.head(), &push_plan, &|_| None).unwrap();
    let meta = OpMeta::new(OpKind::Publish, push::push_landed_description(&push_plan));
    assert_eq!(
        meta.description,
        "reef: push landed main to origin/main (1 commit)"
    );
    let recorded = fixture.repo.record_push(&push_plan, &meta).await.unwrap();
    assert!(recorded.op_id.is_some());
    assert!(recorded.local.is_none());
    let after = fixture.head();
    assert_eq!(remote_main(&after).as_deref(), Some(landed.as_str()));
    assert_eq!(local_main(&after).as_deref(), Some(landed.as_str()));
    assert_eq!(remote_state(&after), tracking, "tracking state kept");
    assert_eq!(
        git(&fixture.other, &["ls-remote", "origin", "refs/heads/main"])
            .split_whitespace()
            .next(),
        Some(landed.as_str())
    );
    assert_eq!(
        plan(after.clone()).await.unwrap().update,
        LandedUpdate::UpToDate
    );

    // Someone else pushes on top: a following fetch fast-forwards `main`.
    let theirs = push_on_remote(&fixture, "theirs.txt");
    let recorded = fetch_following(&mut fixture).await;
    assert!(recorded.branches[0].local_moved);
    assert_eq!(recorded.branches[0].left, None);
    assert_eq!(
        local_main(&fixture.head()).as_deref(),
        Some(theirs.as_str())
    );
    assert_eq!(
        plan(fixture.head()).await.unwrap().update,
        LandedUpdate::UpToDate
    );

    // A local landing and a foreign push on the same base: diverged, and the
    // following fetch leaves `main` exactly where it is, unconflicted.
    let mine = land_locally(&mut fixture, &theirs, "mine.txt").await;
    let foreign = push_on_remote(&fixture, "foreign.txt");
    let recorded = fetch_following(&mut fixture).await;
    assert_eq!(recorded.branches[0].left, Some(fetch::Left::Diverged));
    assert!(!recorded.branches[0].local_moved);
    let head = fixture.head();
    assert_eq!(local_main(&head).as_deref(), Some(mine.as_str()));
    assert_eq!(remote_main(&head).as_deref(), Some(foreign.as_str()));
    let diverged = plan(head.clone()).await.unwrap();
    assert_eq!(diverged.update, LandedUpdate::Diverged);
    assert!(diverged.plan.is_none());
    assert_eq!(
        diverged.remote_head.unwrap().commit.as_str(),
        foreign.as_str()
    );
}

// R3.17 test 3: a remote ahead that changes a policy file is not followed;
// a following fetch catches up a bookmark the view already knows is behind;
// a bookmark not followed merges as before (`record_matches_jj`).
#[tokio::test]
async fn following_refuses_policy_changes() {
    use reef_lib::jj::push::{self, LandedUpdate};
    if !has_git() {
        return;
    }
    let mut fixture = Fixture::new(true).await;
    let policy = push_on_remote(&fixture, "reef-policy.toml");
    let recorded = fetch_following(&mut fixture).await;
    assert_eq!(recorded.branches[0].left, Some(fetch::Left::Policy));
    assert_eq!(
        local_main(&fixture.head()).as_deref(),
        Some(fixture.base.as_str())
    );
    assert_eq!(
        remote_main(&fixture.head()).as_deref(),
        Some(policy.as_str())
    );
    let planned = push::plan_landed_push(&fixture.head(), "main", "origin", &[], 1_000)
        .await
        .unwrap();
    assert_eq!(planned.update, LandedUpdate::RemoteAhead);
    assert!(
        push::policy_files_differ(
            fixture.head().as_ref(),
            &jj_lib::backend::CommitId::try_from_hex(&fixture.base).unwrap(),
            &jj_lib::backend::CommitId::try_from_hex(&policy).unwrap(),
        )
        .await
        .unwrap()
    );

    // An ordinary commit on top: still behind a policy change, still left.
    let more = push_on_remote(&fixture, "more.txt");
    let recorded = fetch_following(&mut fixture).await;
    assert_eq!(recorded.branches[0].left, Some(fetch::Left::Policy));
    assert_eq!(remote_main(&fixture.head()).as_deref(), Some(more.as_str()));

    // Once the local destination has the policy change (a person moved it),
    // the next following fetch catches up with what the view already knows.
    git(&fixture.root, &["update-ref", "refs/heads/main", &policy]);
    fixture
        .repo
        .import_git_refs(&OpMeta::new(OpKind::ImportRefs, "reef: import git refs"))
        .await
        .unwrap();
    let recorded = fetch_following(&mut fixture).await;
    assert!(recorded.branches[0].local_moved);
    assert!(recorded.op_id.is_some());
    assert_eq!(local_main(&fixture.head()).as_deref(), Some(more.as_str()));
}
