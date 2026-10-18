//! The workspace spec, `reef.toml` (R1.19, ADR 0021): parsing and
//! validation, and reading it from a commit's tree. Numbered as the brief
//! numbers them.

#![allow(
    clippy::expect_used,
    clippy::unwrap_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::path::Path;
use std::time::Duration;

use jj_lib::repo_path::RepoPath;
use reef_lib::error::{Classify as _, ErrorChain, ErrorKind};
use reef_lib::jj::materialize::tree_key;
use reef_lib::jj::settings::{Identity, user_settings};
use reef_lib::jj::spec::ReadFileError;
use reef_lib::jj::{CommitIdHex, Repo};
use reef_lib::spec::{Spec, SpecError, StoreSpec};

const FULL_EXAMPLE: &str = r#"
[workspace]
lease = "30m"

[snapshot]
exclude = ["target/", "node_modules/"]

[[store]]
name = "node_modules"
lockfile = "pnpm-lock.yaml"
mount = "node_modules"
build = ["pnpm", "install", "--frozen-lockfile"]
[store.env]
CI = "1"
"#;

fn parse_err(text: &str) -> SpecError {
    Spec::parse(text).expect_err(text)
}

/// One store with the given lockfile and mount.
fn store(name: &str, lockfile: &str, mount: &str) -> String {
    format!(
        "[[store]]\nname = {name:?}\nlockfile = {lockfile:?}\nmount = {mount:?}\nbuild = [\"true\"]\n"
    )
}

// 1
#[test]
fn empty_text_is_default() {
    assert_eq!(Spec::parse("").unwrap(), Spec::default());
}

// 2
#[test]
fn full_example_parses() {
    let spec = Spec::parse(FULL_EXAMPLE).unwrap();
    assert_eq!(spec.workspace.lease, Some(Duration::from_mins(30)));
    assert_eq!(spec.snapshot.exclude, ["target/", "node_modules/"]);
    assert_eq!(
        spec.stores,
        [StoreSpec {
            name: "node_modules".into(),
            lockfile: "pnpm-lock.yaml".into(),
            mount: "node_modules".into(),
            build: vec!["pnpm".into(), "install".into(), "--frozen-lockfile".into()],
            env: BTreeMap::from([("CI".into(), "1".into())]),
        }]
    );
    assert_eq!(spec.store("node_modules"), spec.stores.first());
    // The mount is anchored without a trailing `/` so that it also matches
    // the symlink R1.20 puts there (jj matches a symlink as a file).
    assert_eq!(
        spec.effective_excludes(),
        ["target/", "node_modules/", "/node_modules"]
    );
}

// 3
#[test]
fn unknown_key_is_rejected() {
    let cases = [
        ("[foo]\n", "foo"),
        ("[workspace]\nfoo = 1\n", "foo"),
        ("[snapshot]\nbar = []\n", "bar"),
        (&*format!("{}baz = 1\n", store("s", "l", "m")), "baz"),
        (
            &*format!(
                "{}[store.env]\nA = \"1\"\n[store.qux]\n",
                store("s", "l", "m")
            ),
            "qux",
        ),
    ];
    for (text, key) in cases {
        let error = parse_err(text);
        assert!(matches!(error, SpecError::Parse(_)), "{text}: {error:?}");
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        let message = ErrorChain(&error).to_string();
        assert!(message.contains(key), "{text}: {message}");
    }
}

// 4
#[test]
fn build_must_be_an_array() {
    let text =
        "[[store]]\nname = \"s\"\nlockfile = \"l\"\nmount = \"m\"\nbuild = \"pnpm install\"\n";
    assert!(matches!(parse_err(text), SpecError::Parse(_)));
}

// 5
#[test]
fn store_name_rules() {
    for name in ["Node_Modules", "-x", &"a".repeat(65), ""] {
        let error = parse_err(&store(name, "l", "m"));
        assert!(
            matches!(error, SpecError::StoreName { .. }),
            "{name:?}: {error:?}"
        );
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
    }
    assert!(Spec::parse(&store(&"a".repeat(64), "l", "m")).is_ok());
    let twice = format!("{}{}", store("s", "l1", "m1"), store("s", "l2", "m2"));
    assert!(matches!(parse_err(&twice), SpecError::DuplicateStore { name } if name == "s"));
}

// 6
#[test]
fn paths_are_normalized_relative() {
    let bad = [
        "/node_modules",
        "./node_modules",
        "a/../b",
        "node_modules/",
        "",
        "a//b",
        "a\\b",
        // What gitignore would not read as the literal path (mounts become
        // ignore lines).
        "#cache",
        "!x",
        "a*b",
        "x/[y]",
        "?",
        "x ",
        " x",
        "a\nb",
        "a\tb",
    ];
    for path in bad {
        let error = parse_err(&store("s", "lock", path));
        assert!(
            matches!(&error, SpecError::StorePath { field: "mount", value, .. } if value == path),
            "mount {path:?}: {error:?}"
        );
        let error = parse_err(&store("s", path, "m"));
        assert!(
            matches!(&error, SpecError::StorePath { field: "lockfile", value, .. } if value == path),
            "lockfile {path:?}: {error:?}"
        );
    }
    assert!(Spec::parse(&store("s", "a/b.lock", "x/y")).is_ok());
}

// 7
#[test]
fn mount_overlap() {
    let nested = format!(
        "{}{}",
        store("a", "a.lock", "vendor"),
        store("b", "b.lock", "vendor/x")
    );
    assert!(matches!(parse_err(&nested), SpecError::MountOverlap { .. }));
    let on_lockfile = format!(
        "{}{}",
        store("a", "a.lock", "m"),
        store("b", "b.lock", "a.lock")
    );
    assert!(matches!(
        parse_err(&on_lockfile),
        SpecError::MountOverlap { name, other, .. } if name == "b" && other == "a.lock"
    ));
    // Its own lockfile under its own mount would be shadowed by the mount.
    let own = store("a", "vendor/a.lock", "vendor");
    assert!(matches!(parse_err(&own), SpecError::MountOverlap { .. }));
    // Compared by component, not by string prefix.
    let siblings = format!(
        "{}{}",
        store("a", "a.lock", "vendor"),
        store("b", "b.lock", "vendors")
    );
    assert!(Spec::parse(&siblings).is_ok());
}

// 8
#[test]
fn lease_bounds() {
    let lease = |value: &str| Spec::parse(&format!("[workspace]\nlease = {value:?}\n"));
    assert!(matches!(
        lease("30s"),
        Err(SpecError::LeaseTooShort(length)) if length == Duration::from_secs(30)
    ));
    assert_eq!(
        lease("1m").unwrap().workspace.lease,
        Some(Duration::from_mins(1))
    );
    assert_eq!(
        lease("25h").unwrap().workspace.lease,
        Some(Duration::from_hours(25))
    );
    assert!(matches!(lease("soon"), Err(SpecError::Parse(_))));
}

// 9
#[test]
fn exclude_entries() {
    let exclude = |entries: &str| Spec::parse(&format!("[snapshot]\nexclude = {entries}\n"));
    assert!(matches!(
        exclude(r#"[""]"#),
        Err(SpecError::Exclude { index: 0 })
    ));
    assert!(matches!(
        exclude(r#"["ok", "a\u0000b"]"#),
        Err(SpecError::Exclude { index: 1 })
    ));
    assert!(matches!(
        exclude(r#"["  "]"#),
        Err(SpecError::Exclude { index: 0 })
    ));
    assert!(matches!(
        exclude(r#"["a\nb"]"#),
        Err(SpecError::Exclude { index: 0 })
    ));
    assert!(matches!(
        exclude(r#"["a\rb"]"#),
        Err(SpecError::Exclude { index: 0 })
    ));
    assert!(exclude(r#"["target/", "!keep", "*.log"]"#).is_ok());
}

fn settings() -> reef_lib::jj::UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

const ONE_STORE: &str = r#"
[snapshot]
exclude = ["target/"]

[[store]]
name = "deps"
lockfile = "pnpm-lock.yaml"
mount = "node_modules"
build = ["pnpm", "install"]
"#;

/// A repository with `reef.toml` (one store) and its lockfile snapshotted;
/// returns it and the snapshot's commit.
async fn repo_with_spec(root: &Path) -> (Repo, CommitIdHex) {
    let mut repo = Repo::init_internal_git(root, &settings()).await.unwrap();
    std::fs::write(root.join("reef.toml"), ONE_STORE).unwrap();
    std::fs::write(root.join("pnpm-lock.yaml"), "lockfileVersion: 9\n").unwrap();
    let outcome = repo.snapshot("add reef.toml").await.unwrap();
    (repo, outcome.wc_commit)
}

// 10
#[tokio::test]
async fn spec_at_reads_from_tree() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (mut repo, commit) = repo_with_spec(root).await;
    let spec = repo.spec_at(&commit).await.unwrap().expect("a spec");
    assert_eq!(spec, Spec::parse(ONE_STORE).unwrap());
    let lockfile = RepoPath::from_internal_string("pnpm-lock.yaml").unwrap();
    assert_eq!(
        repo.read_file_at(&commit, lockfile)
            .await
            .unwrap()
            .as_deref(),
        Some(&b"lockfileVersion: 9\n"[..])
    );
    let absent = RepoPath::from_internal_string("absent").unwrap();
    assert_eq!(repo.read_file_at(&commit, absent).await.unwrap(), None);

    std::fs::remove_file(root.join("pnpm-lock.yaml")).unwrap();
    let commit = repo.snapshot("drop the lockfile").await.unwrap().wc_commit;
    let error = repo.spec_at(&commit).await.unwrap_err();
    assert!(
        matches!(&error, SpecError::LockfileMissing { name, lockfile } if name == "deps" && lockfile == "pnpm-lock.yaml"),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::NotFound);

    std::fs::remove_file(root.join("reef.toml")).unwrap();
    let commit = repo.snapshot("drop reef.toml").await.unwrap().wc_commit;
    assert_eq!(repo.spec_at(&commit).await.unwrap(), None);
}

// 11
#[tokio::test]
async fn spec_at_ignores_disk() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let (repo, commit) = repo_with_spec(root).await;
    std::fs::write(root.join("reef.toml"), "this is [not toml").unwrap();
    std::fs::remove_file(root.join("pnpm-lock.yaml")).unwrap();
    let spec = repo.spec_at(&commit).await.unwrap().expect("a spec");
    assert_eq!(spec.store("deps").unwrap().mount, "node_modules");
}

#[tokio::test]
async fn spec_in_tree_matches_spec_at() {
    let tmp = tempfile::tempdir().unwrap();
    let (repo, commit) = repo_with_spec(tmp.path()).await;
    let key = {
        let readonly = repo.readonly();
        let id = jj_lib::backend::CommitId::try_from_hex(commit.as_str()).unwrap();
        tree_key(
            &jj_lib::repo::Repo::store(&*readonly)
                .get_commit(&id)
                .unwrap(),
        )
    };
    assert_eq!(
        repo.spec_in_tree(&key).await.unwrap(),
        repo.spec_at(&commit).await.unwrap()
    );
    let error = repo.spec_in_tree("not a key").await.unwrap_err();
    assert!(matches!(
        error,
        SpecError::Read(ReadFileError::InvalidTree(_))
    ));
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);
}

#[tokio::test]
async fn spec_must_be_a_regular_utf8_file() {
    let tmp = tempfile::tempdir().unwrap();
    let root = tmp.path();
    let mut repo = Repo::init_internal_git(root, &settings()).await.unwrap();
    std::fs::write(root.join("target.toml"), "").unwrap();
    std::os::unix::fs::symlink("target.toml", root.join("reef.toml")).unwrap();
    let commit = repo.snapshot("symlink").await.unwrap().wc_commit;
    let error = repo.spec_at(&commit).await.unwrap_err();
    assert!(
        matches!(
            &error,
            SpecError::Read(ReadFileError::NotAFile {
                kind: "symlink",
                ..
            })
        ),
        "{error:?}"
    );
    assert_eq!(error.kind(), ErrorKind::InvalidArgument);

    std::fs::remove_file(root.join("reef.toml")).unwrap();
    std::fs::write(root.join("reef.toml"), b"[workspace]\nlease = \"\xff\"\n").unwrap();
    let commit = repo.snapshot("not utf-8").await.unwrap().wc_commit;
    assert!(matches!(
        repo.spec_at(&commit).await.unwrap_err(),
        SpecError::Parse(_)
    ));
}
