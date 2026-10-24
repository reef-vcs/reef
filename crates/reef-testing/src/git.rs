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
/// contents; top-level names only) written to disk and committed once (see
/// [`commit`]), on `main`, with HEAD on `main`. The index is not written, so
/// `git status` would show the files as staged deletions plus untracked
/// files; jj, which reads the commit, does not care. Returns the commit id
/// in hex.
pub fn init_repo(root: &Path, files: &[(&str, &str)]) -> String {
    gix::init(root).expect("git init");
    // `gix::init` honors a global `init.defaultBranch`.
    std::fs::write(root.join(".git/HEAD"), "ref: refs/heads/main\n").expect("point HEAD at main");
    commit(root, "refs/heads/main", None, files)
}

/// Commits `files` (top-level names only; written to disk too) as the whole
/// tree of a new commit on `parent`, or a root commit, and points
/// `ref_name` (a full name) at it. Returns the commit id in hex.
pub fn commit(root: &Path, ref_name: &str, parent: Option<&str>, files: &[(&str, &str)]) -> String {
    let repo = gix::open(root).expect("open the git repository");
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
    let parents: Vec<gix::ObjectId> = parent
        .map(|hex| gix::ObjectId::from_hex(hex.as_bytes()).expect("a parent id"))
        .into_iter()
        .collect();
    let commit = repo
        .commit_as(signature, signature, ref_name, "commit", tree, parents)
        .expect("commit");
    commit.to_string()
}

/// Writes `contents` as a blob; returns its id in hex.
pub fn blob(root: &Path, contents: &[u8]) -> String {
    let repo = gix::open(root).expect("open the git repository");
    repo.write_blob(contents).expect("write a blob").to_string()
}

/// Writes a tree object of `entries` (mode as git writes it, such as
/// `"100644"`, `"40000"`, or `"120000"`; name; object id in hex) exactly as
/// given: in the given order and with names git itself refuses (`..`, `a/b`,
/// `.git`), for tests of hostile repository content (RX.01). Returns its id
/// in hex.
pub fn raw_tree(root: &Path, entries: &[(&str, &[u8], &str)]) -> String {
    let repo = gix::open(root).expect("open the git repository");
    let mut bytes = Vec::new();
    for (mode, name, oid) in entries {
        bytes.extend_from_slice(mode.as_bytes());
        bytes.push(b' ');
        bytes.extend_from_slice(name);
        bytes.push(0);
        let oid = gix::ObjectId::from_hex(oid.as_bytes()).expect("an object id");
        bytes.extend_from_slice(oid.as_bytes());
    }
    // The object database's own writer: `Repository::write_object` would
    // encode a `Tree`, which cannot hold these names.
    gix::objs::Write::write_buf(&repo.objects, gix::object::Kind::Tree, &bytes)
        .expect("write the tree")
        .to_string()
}

/// A commit of `tree` (hex) on `parent`, or a root commit, with `ref_name`
/// (a full name) pointing at it; nothing is written to disk. Returns the
/// commit id in hex.
pub fn commit_tree(root: &Path, ref_name: &str, tree: &str, parent: Option<&str>) -> String {
    let repo = gix::open(root).expect("open the git repository");
    let signature = gix::actor::Signature {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
        time: gix::date::Time::new(1_700_000_000, 0),
    };
    let mut time = gix::date::parse::TimeBuf::default();
    let signature = signature.to_ref(&mut time);
    let tree = gix::ObjectId::from_hex(tree.as_bytes()).expect("a tree id");
    let parents: Vec<gix::ObjectId> = parent
        .map(|hex| gix::ObjectId::from_hex(hex.as_bytes()).expect("a parent id"))
        .into_iter()
        .collect();
    repo.commit_as(signature, signature, ref_name, "hostile", tree, parents)
        .expect("commit")
        .to_string()
}
