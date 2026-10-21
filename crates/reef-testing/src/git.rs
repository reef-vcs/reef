//! Plain git repositories for tests (R1.25), made with `gix` so no test
//! needs a `git` binary.

#![allow(
    clippy::expect_used,
    reason = "fixture builders; a panic is the failure report"
)]

use std::path::Path;

use gix::bstr::BString;
use gix::objs::tree::{Entry, EntryKind};

/// Makes `root` (which must exist) a git repository with `files` (path,
/// contents; top-level names only) written to disk and committed once, as
/// `init`, on `main`, with HEAD on `main`. The index is not written, so
/// `git status` would show the files as staged deletions plus untracked
/// files; jj, which reads the commit, does not care. Returns the commit id
/// in hex.
pub fn init_repo(root: &Path, files: &[(&str, &str)]) -> String {
    let repo = gix::init(root).expect("git init");
    // `gix::init` honors a global `init.defaultBranch`.
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("point HEAD at main");
    let mut entries: Vec<Entry> = files
        .iter()
        .map(|(name, contents)| {
            std::fs::write(root.join(name), contents).expect("write a file");
            Entry {
                mode: EntryKind::Blob.into(),
                filename: BString::from(*name),
                oid: repo.write_blob(contents).expect("write a blob").detach(),
            }
        })
        .collect();
    entries.sort();
    let tree = repo
        .write_object(gix::objs::Tree { entries })
        .expect("write the tree")
        .detach();
    let signature = gix::actor::Signature {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
        time: gix::date::Time::new(1_700_000_000, 0),
    };
    let mut time = gix::date::parse::TimeBuf::default();
    let signature = signature.to_ref(&mut time);
    let commit = repo
        .commit_as(
            signature,
            signature,
            "refs/heads/main",
            "init",
            tree,
            gix::commit::NO_PARENT_IDS,
        )
        .expect("commit");
    commit.to_string()
}
