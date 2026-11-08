//! Git object ids, computed by the executor itself (ADR 0060 decision 4):
//! a blob's id from its bytes, and a tree's from its entries, with
//! collision-detecting SHA-1 (`sha1dc`), as git computes them.
//!
//! With these the executor checks, without trusting the daemon's word, that
//! a listing is exactly the job's `tree_id`, that each blob is what the
//! listing names, and that what it laid out on disk is the tree again.

use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::io::Read as _;
use std::path::Path;

/// A git object id: 20 bytes.
pub type ObjectId = [u8; 20];

/// What a tree entry is, by its git mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Mode {
    /// `100644`.
    File,
    /// `100755`.
    Executable,
    /// `120000`: the blob is the link's target.
    Symlink,
    /// `160000`: the id is a commit's, and nothing is checked out.
    Submodule,
}

impl Mode {
    fn text(self) -> &'static [u8] {
        match self {
            Mode::File => b"100644",
            Mode::Executable => b"100755",
            Mode::Symlink => b"120000",
            Mode::Submodule => b"160000",
        }
    }
}

/// Why entries do not form a tree.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum TreeError {
    /// A path with an empty, `.`, `..` or `.git` component, or a NUL.
    #[error("{0:?} is not a path a tree can hold")]
    BadPath(String),
    /// A path that is both a file and a directory, or listed twice.
    #[error("{0:?} is listed twice, or both as a file and as a directory")]
    Clash(String),
    /// A digest collision attack was detected.
    #[error("SHA-1 collision attack detected while hashing {0}")]
    Collision(String),
    /// An id that is not 40 hex digits.
    #[error("{0:?} is not a git object id")]
    BadId(String),
}

/// `id` as 40 lowercase hex digits.
pub fn hex(id: &ObjectId) -> String {
    let mut out = String::with_capacity(40);
    for byte in id {
        let _ = write!(out, "{byte:02x}");
    }
    out
}

/// 40 hex digits as an id.
pub fn parse_hex(text: &str) -> Result<ObjectId, TreeError> {
    let bad = || TreeError::BadId(text.to_owned());
    if text.len() != 40 {
        return Err(bad());
    }
    let mut id = [0u8; 20];
    for (index, byte) in id.iter_mut().enumerate() {
        *byte = u8::from_str_radix(&text[index * 2..index * 2 + 2], 16).map_err(|_| bad())?;
    }
    Ok(id)
}

/// The id of a blob holding `bytes`.
pub fn blob_id(bytes: &[u8]) -> Result<ObjectId, TreeError> {
    let mut hasher = BlobHasher::new(bytes.len() as u64);
    hasher.update(bytes);
    hasher.finish("a blob")
}

/// A blob's id over bytes given in pieces; the size must be known first.
pub struct BlobHasher(sha1dc::Hasher);

impl BlobHasher {
    /// A blob of `size` bytes.
    pub fn new(size: u64) -> BlobHasher {
        let mut hasher = sha1dc::Hasher::default();
        hasher.update(format!("blob {size}\0").as_bytes());
        BlobHasher(hasher)
    }

    /// More of the blob.
    pub fn update(&mut self, bytes: &[u8]) {
        self.0.update(bytes);
    }

    /// The id, or an error naming `what` on a detected collision.
    pub fn finish(self, what: &str) -> Result<ObjectId, TreeError> {
        self.0
            .finalize()
            .map(Into::into)
            .map_err(|_| TreeError::Collision(what.to_owned()))
    }
}

/// The id of the blob in the file at `path`, read once.
pub fn file_blob_id(path: &Path) -> std::io::Result<Result<ObjectId, TreeError>> {
    let mut file = std::fs::File::open(path)?;
    let size = file.metadata()?.len();
    let mut hasher = BlobHasher::new(size);
    let mut buffer = vec![0u8; 1 << 16];
    let mut read = 0u64;
    loop {
        let n = file.read(&mut buffer)?;
        if n == 0 {
            break;
        }
        read += n as u64;
        hasher.update(&buffer[..n]);
    }
    if read != size {
        return Err(std::io::Error::other(format!(
            "{} changed size while it was hashed",
            path.display()
        )));
    }
    Ok(hasher.finish(&path.display().to_string()))
}

/// Builds tree ids from `(path, mode, id)` entries: every non-directory
/// entry of the tree, slash-separated, in any order.
#[derive(Default)]
pub struct TreeBuilder {
    root: Dir,
}

#[derive(Default)]
struct Dir {
    entries: BTreeMap<Vec<u8>, Node>,
}

enum Node {
    Leaf(Mode, ObjectId),
    Dir(Dir),
}

impl TreeBuilder {
    /// An empty tree.
    pub fn new() -> TreeBuilder {
        TreeBuilder::default()
    }

    /// Adds an entry.
    pub fn add(&mut self, path: &str, mode: Mode, id: ObjectId) -> Result<(), TreeError> {
        let components = components(path)?;
        let (last, parents) = components
            .split_last()
            .ok_or_else(|| TreeError::BadPath(path.to_owned()))?;
        let mut dir = &mut self.root;
        for component in parents {
            let node = dir
                .entries
                .entry(component.as_bytes().to_vec())
                .or_insert_with(|| Node::Dir(Dir::default()));
            dir = match node {
                Node::Dir(dir) => dir,
                Node::Leaf(..) => return Err(TreeError::Clash(path.to_owned())),
            };
        }
        match dir.entries.entry(last.as_bytes().to_vec()) {
            std::collections::btree_map::Entry::Occupied(_) => {
                Err(TreeError::Clash(path.to_owned()))
            }
            std::collections::btree_map::Entry::Vacant(slot) => {
                slot.insert(Node::Leaf(mode, id));
                Ok(())
            }
        }
    }

    /// The root tree's id.
    pub fn finish(&self) -> Result<ObjectId, TreeError> {
        hash_dir(&self.root, "")
    }
}

fn hash_dir(dir: &Dir, path: &str) -> Result<ObjectId, TreeError> {
    // Git sorts entries by name as bytes, a directory's name compared as
    // if it ended in `/`.
    let mut entries: Vec<(Vec<u8>, &[u8], ObjectId)> = Vec::with_capacity(dir.entries.len());
    for (name, node) in &dir.entries {
        let child = if path.is_empty() {
            String::from_utf8_lossy(name).into_owned()
        } else {
            format!("{path}/{}", String::from_utf8_lossy(name))
        };
        match node {
            Node::Leaf(mode, id) => entries.push((name.clone(), mode.text(), *id)),
            Node::Dir(sub) => {
                let id = hash_dir(sub, &child)?;
                let mut key = name.clone();
                key.push(b'/');
                entries.push((key, b"40000", id));
            }
        }
    }
    entries.sort_by(|a, b| a.0.cmp(&b.0));
    let mut body = Vec::new();
    for (key, mode, id) in &entries {
        let name = key.strip_suffix(b"/").unwrap_or(key);
        body.extend_from_slice(mode);
        body.push(b' ');
        body.extend_from_slice(name);
        body.push(0);
        body.extend_from_slice(id);
    }
    let mut hasher = sha1dc::Hasher::default();
    hasher.update(format!("tree {}\0", body.len()).as_bytes());
    hasher.update(&body);
    hasher.finalize().map(Into::into).map_err(|_| {
        TreeError::Collision(if path.is_empty() {
            "the root tree".into()
        } else {
            path.to_owned()
        })
    })
}

/// A path's components: refused when one is empty, `.`, `..`, `.git` in
/// any case, or holds a NUL.
pub fn components(path: &str) -> Result<Vec<&str>, TreeError> {
    let parts: Vec<&str> = path.split('/').collect();
    let bad = parts.iter().any(|c| {
        c.is_empty()
            || *c == "."
            || *c == ".."
            || c.eq_ignore_ascii_case(".git")
            || c.contains('\0')
    });
    if bad || path.is_empty() {
        return Err(TreeError::BadPath(path.to_owned()));
    }
    Ok(parts)
}

/// The tree id of the directory at `root` as it is on disk: regular files
/// by their executable bit, symlinks by their target, with the
/// `submodules` (paths and commit ids) added as git leaves them, empty
/// directories ignored. Nothing is followed through a link.
pub fn disk_tree_id(
    root: &Path,
    submodules: &[(String, ObjectId)],
) -> std::io::Result<Result<ObjectId, TreeError>> {
    use std::os::unix::ffi::OsStrExt as _;
    use std::os::unix::fs::PermissionsExt as _;
    let mut builder = TreeBuilder::new();
    let skip: std::collections::BTreeSet<&str> =
        submodules.iter().map(|(path, _)| path.as_str()).collect();
    let mut stack = vec![(root.to_owned(), String::new())];
    while let Some((dir, prefix)) = stack.pop() {
        for entry in std::fs::read_dir(&dir)? {
            let entry = entry?;
            let name = entry.file_name();
            let Ok(name) = std::str::from_utf8(name.as_bytes()) else {
                return Ok(Err(TreeError::BadPath(name.to_string_lossy().into_owned())));
            };
            let path = if prefix.is_empty() {
                name.to_owned()
            } else {
                format!("{prefix}/{name}")
            };
            let kind = entry.file_type()?;
            if kind.is_dir() {
                if !skip.contains(path.as_str()) {
                    stack.push((entry.path(), path));
                }
            } else if kind.is_symlink() {
                let target = std::fs::read_link(entry.path())?;
                let id = match blob_id(target.as_os_str().as_bytes()) {
                    Ok(id) => id,
                    Err(error) => return Ok(Err(error)),
                };
                if let Err(error) = builder.add(&path, Mode::Symlink, id) {
                    return Ok(Err(error));
                }
            } else if kind.is_file() {
                let mode = if entry.metadata()?.permissions().mode() & 0o111 != 0 {
                    Mode::Executable
                } else {
                    Mode::File
                };
                let id = match file_blob_id(&entry.path())? {
                    Ok(id) => id,
                    Err(error) => return Ok(Err(error)),
                };
                if let Err(error) = builder.add(&path, mode, id) {
                    return Ok(Err(error));
                }
            } else {
                return Ok(Err(TreeError::BadPath(path)));
            }
        }
    }
    for (path, id) in submodules {
        if let Err(error) = builder.add(path, Mode::Submodule, *id) {
            return Ok(Err(error));
        }
    }
    Ok(builder.finish())
}

#[cfg(test)]
mod tests {
    use super::*;

    // Ids from git itself: `git hash-object`, `git mktree`, `git write-tree`.
    const EMPTY_BLOB: &str = "e69de29bb2d1d6434b8b29ae775ad8c2e48c5391";
    const HELLO_BLOB: &str = "ce013625030ba8dba906f756967f9e9ca394464a"; // "hello\n"
    const EMPTY_TREE: &str = "4b825dc642cb6eb9a060e54bf8d69288fbee4904";

    #[test]
    fn blob_ids_are_git_s() {
        assert_eq!(hex(&blob_id(b"").unwrap()), EMPTY_BLOB);
        assert_eq!(hex(&blob_id(b"hello\n").unwrap()), HELLO_BLOB);
    }

    #[test]
    fn the_empty_tree() {
        assert_eq!(hex(&TreeBuilder::new().finish().unwrap()), EMPTY_TREE);
    }

    #[test]
    fn a_tree_with_one_file() {
        // printf 'hello\n' > hello; git add hello; git write-tree
        let mut builder = TreeBuilder::new();
        builder
            .add("hello", Mode::File, parse_hex(HELLO_BLOB).unwrap())
            .unwrap();
        assert_eq!(
            hex(&builder.finish().unwrap()),
            "b4d01e9b0c4a9356736dfddf8830ba9a54f5271c"
        );
    }

    #[test]
    fn directories_sort_with_a_trailing_slash() {
        // `a.b` (file) sorts before `a/` (directory), though `a` < `a.b`.
        // git: mkdir a; echo hello > a/x; echo hello > a.b; git add .; git write-tree
        let hello = parse_hex(HELLO_BLOB).unwrap();
        let mut builder = TreeBuilder::new();
        builder.add("a/x", Mode::File, hello).unwrap();
        builder.add("a.b", Mode::File, hello).unwrap();
        let mut reversed = TreeBuilder::new();
        reversed.add("a.b", Mode::File, hello).unwrap();
        reversed.add("a/x", Mode::File, hello).unwrap();
        assert_eq!(builder.finish().unwrap(), reversed.finish().unwrap());
        assert_eq!(
            hex(&builder.finish().unwrap()),
            "ae079d5d2fecf69af67f09002152cd6cbbbd6ef8",
        );
    }

    #[test]
    fn symlinks_and_executables() {
        // ...then: ln -s a/x link; printf '#!/bin/sh\n' > run.sh; chmod +x run.sh
        let hello = parse_hex(HELLO_BLOB).unwrap();
        let mut builder = TreeBuilder::new();
        builder.add("a/x", Mode::File, hello).unwrap();
        builder.add("a.b", Mode::File, hello).unwrap();
        builder
            .add("link", Mode::Symlink, blob_id(b"a/x").unwrap())
            .unwrap();
        builder
            .add("run.sh", Mode::Executable, blob_id(b"#!/bin/sh\n").unwrap())
            .unwrap();
        assert_eq!(
            hex(&builder.finish().unwrap()),
            "407930139bf3da5238a5541597ded586ed1d8909",
        );
    }

    #[test]
    fn the_disk_hashes_as_the_listing() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::create_dir(root.join("a")).unwrap();
        std::fs::write(root.join("a/x"), "hello\n").unwrap();
        std::fs::write(root.join("a.b"), "hello\n").unwrap();
        std::os::unix::fs::symlink("a/x", root.join("link")).unwrap();
        std::fs::write(root.join("run.sh"), "#!/bin/sh\n").unwrap();
        std::fs::set_permissions(
            root.join("run.sh"),
            std::os::unix::fs::PermissionsExt::from_mode(0o755),
        )
        .unwrap();
        std::fs::create_dir(root.join("empty")).unwrap();
        assert_eq!(
            hex(&disk_tree_id(root, &[]).unwrap().unwrap()),
            "407930139bf3da5238a5541597ded586ed1d8909",
        );
        std::fs::write(root.join("a/x"), "changed\n").unwrap();
        assert_ne!(
            hex(&disk_tree_id(root, &[]).unwrap().unwrap()),
            "407930139bf3da5238a5541597ded586ed1d8909",
        );
    }

    #[test]
    fn clashes_and_bad_paths_are_refused() {
        let id = [0u8; 20];
        let mut builder = TreeBuilder::new();
        builder.add("a", Mode::Symlink, id).unwrap();
        assert!(matches!(
            builder.add("a/b", Mode::File, id),
            Err(TreeError::Clash(_))
        ));
        assert!(matches!(
            builder.add("a", Mode::File, id),
            Err(TreeError::Clash(_))
        ));
        for bad in ["", "/a", "a//b", "a/./b", "a/../b", ".git/config", "x/.GIT"] {
            assert!(
                matches!(builder.add(bad, Mode::File, id), Err(TreeError::BadPath(_))),
                "{bad:?}"
            );
        }
    }

    #[test]
    fn hex_round_trips() {
        let id = parse_hex(HELLO_BLOB).unwrap();
        assert_eq!(hex(&id), HELLO_BLOB);
        assert!(parse_hex("xyz").is_err());
        assert!(parse_hex(&"g".repeat(40)).is_err());
    }
}
