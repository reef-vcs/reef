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
