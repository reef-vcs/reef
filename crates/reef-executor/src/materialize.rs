//! Getting a job's tree (ADR 0060 decision 4): `FileService` at the job's
//! commit, through a content-addressed blob cache, into a private
//! directory, checked three times.
//!
//! 1. **The listing** (`ListFiles`, recursive) must hash, as git hashes
//!    trees, to the job's `tree_id`.
//! 2. **Each blob** read (`ReadFile`) must hash, as a git blob, to the id
//!    the listing names, before it enters the cache.
//! 3. **The disk**, laid out, must hash to `tree_id` again
//!    (`[materialize] verify = "disk"`, the default).
//!
//! Nothing is written through a link: every directory on the way is one
//! this walk created, and every file is created with `O_EXCL` and
//! `O_NOFOLLOW`.

use std::collections::BTreeMap;
use std::io::Write as _;
use std::os::unix::fs::{DirBuilderExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use futures::StreamExt as _;
use reef_api::v1::{self, EntryType};
use reef_client::{Client, ClientError};

use crate::config::Verify;
use crate::githash::{self, BlobHasher, Mode, ObjectId, TreeBuilder};

/// Blobs read at once.
const CONCURRENT_READS: usize = 8;

/// The largest `reef-checks.toml` read.
const MAX_CHECKS_FILE: u64 = 1 << 20;

/// Why a tree could not be materialized. Each names its step, which
/// `CompleteJob(FAILED)`'s message carries.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum MaterializeError {
    /// The job names a tree with no commit.
    #[error(
        "this job names a tree with no commit; submit it with a revision (`reef job submit -r`), which an executor can read"
    )]
    NoCommit,
    /// The listing is not the job's tree.
    #[error("listing: {0}")]
    Listing(String),
    /// A blob is not what the listing names.
    #[error("blob: {0}")]
    Blob(String),
    /// What was laid out is not the job's tree.
    #[error("disk check: {0}")]
    Disk(String),
    /// The daemon refused a read.
    #[error("reading the tree: {0}")]
    Daemon(#[from] ClientError),
    /// The job's directory or the cache could not be written.
    #[error("laying out the tree: {0}")]
    Io(#[from] std::io::Error),
}

/// The executor's verified blobs: `<dir>/<2 hex>/<38 hex>`, shared by
/// every job and repository, bounded by `[materialize] cache_size`.
#[derive(Debug)]
pub struct BlobCache {
    dir: PathBuf,
    bound: u64,
    /// Jobs read under the read lock; eviction takes the write lock, so a
    /// blob a job is about to copy is never removed under it.
    lock: tokio::sync::RwLock<()>,
}

impl BlobCache {
    /// The cache in `dir`, created if missing.
    pub fn new(dir: PathBuf, bound: u64) -> std::io::Result<BlobCache> {
        std::fs::create_dir_all(dir.join("tmp"))?;
        Ok(BlobCache {
            dir,
            bound,
            lock: tokio::sync::RwLock::new(()),
        })
    }

    fn path(&self, id: &ObjectId) -> PathBuf {
        let hex = githash::hex(id);
        self.dir.join(&hex[..2]).join(&hex[2..])
    }

    /// Removes the least recently used blobs until the cache is under its
    /// bound. Returns the bytes removed.
    pub async fn evict(&self) -> std::io::Result<u64> {
        let _guard = self.lock.write().await;
        let dir = self.dir.clone();
        let bound = self.bound;
        tokio::task::spawn_blocking(move || evict_in(&dir, bound))
            .await
            .map_err(std::io::Error::other)?
    }
}

fn evict_in(dir: &Path, bound: u64) -> std::io::Result<u64> {
    let mut blobs = Vec::new();
    let mut total = 0u64;
    for shard in std::fs::read_dir(dir)? {
        let shard = shard?;
        if shard.file_name() == "tmp" || !shard.file_type()?.is_dir() {
            continue;
        }
        for blob in std::fs::read_dir(shard.path())? {
            let blob = blob?;
            let meta = blob.metadata()?;
            total += meta.len();
            blobs.push((meta.modified()?, meta.len(), blob.path()));
        }
    }
    let mut removed = 0;
    if total <= bound {
        return Ok(0);
    }
    blobs.sort();
    for (_, size, path) in blobs {
        if total <= bound {
            break;
        }
        if std::fs::remove_file(&path).is_ok() {
            total -= size;
            removed += size;
        }
    }
    Ok(removed)
}

/// A materialized tree.
#[derive(Debug, Clone)]
pub struct Tree {
    /// The directory it is laid out in.
    pub dir: PathBuf,
    /// Entries laid out.
    pub entries: usize,
    /// Bytes of its blobs.
    pub bytes: u64,
    /// Blobs read from the daemon (the rest came from the cache).
    pub fetched: usize,
    /// Bytes read from the daemon.
    pub fetched_bytes: u64,
    /// Which checks ran.
    pub verify: Verify,
    /// The tree's `reef-checks.toml`, if it has one as a regular file.
    pub checks_file: Option<Vec<u8>>,
}

struct Entry {
    path: String,
    mode: Mode,
    id: ObjectId,
}

/// Reads the job's tree into `into` (which must not exist), checked.
#[allow(clippy::too_many_lines)]
/// `progress` is called with blobs done and the total to fetch.
pub async fn materialize(
    client: &Client,
    repo: &str,
    spec: &v1::JobSpec,
    into: &Path,
    cache: &BlobCache,
    verify: Verify,
    progress: &(dyn Fn(u64, u64) + Sync),
) -> Result<Tree, MaterializeError> {
    if spec.commit_id.is_empty() {
        return Err(MaterializeError::NoCommit);
    }
    // 1. The listing, checked against the tree id.
    let listing = client
        .list_files(v1::ListFilesRequest {
            repo: repo.to_owned(),
            revision: spec.commit_id.clone(),
            recursive: true,
            skip_snapshot: true,
            ..v1::ListFilesRequest::default()
        })
        .await?;
    if !listing.commit_id.is_empty() && listing.commit_id != spec.commit_id {
        return Err(MaterializeError::Listing(format!(
            "the daemon listed commit {}, not the job's {}",
            listing.commit_id, spec.commit_id
        )));
    }
    let mut entries = Vec::with_capacity(listing.entries.len());
    let mut builder = TreeBuilder::new();
    for entry in listing.entries {
        let mode = match entry.r#type() {
            EntryType::File => Mode::File,
            EntryType::Executable => Mode::Executable,
            EntryType::Symlink => Mode::Symlink,
            EntryType::Submodule => Mode::Submodule,
            EntryType::Directory => continue,
            EntryType::Conflict => {
                return Err(MaterializeError::Listing(format!(
                    "{} is conflicted; a conflicted tree has no git tree id to check",
                    entry.path
                )));
            }
            EntryType::Unspecified => {
                return Err(MaterializeError::Listing(format!(
                    "{} has no entry type",
                    entry.path
                )));
            }
        };
        let id = githash::parse_hex(&entry.file_id)
            .map_err(|error| MaterializeError::Listing(format!("{}: {error}", entry.path)))?;
        builder
            .add(&entry.path, mode, id)
            .map_err(|error| MaterializeError::Listing(error.to_string()))?;
        entries.push(Entry {
            path: entry.path,
            mode,
            id,
        });
    }
    let listed = builder
        .finish()
        .map_err(|error| MaterializeError::Listing(error.to_string()))?;
    if githash::hex(&listed) != spec.tree_id {
        return Err(MaterializeError::Listing(format!(
            "the entries hash to tree {}, not the job's {}",
            githash::hex(&listed),
            spec.tree_id
        )));
    }
    entries.sort_by(|a, b| a.path.cmp(&b.path));

    let _guard = cache.lock.read().await;
    // 2. Each blob not in the cache, checked before it enters.
    let mut wanted: BTreeMap<ObjectId, String> = BTreeMap::new();
    for entry in &entries {
        if entry.mode != Mode::Submodule && !cache.path(&entry.id).exists() {
            wanted.entry(entry.id).or_insert_with(|| entry.path.clone());
        }
    }
    let total = wanted.len() as u64;
    progress(0, total);
    let mut fetched_bytes = 0u64;
    let mut done = 0u64;
    let commit = spec.commit_id.as_str();
    let fetches: Vec<_> = wanted
        .iter()
        .map(|(id, path)| fetch(client, repo, commit, path, *id, cache))
        .collect();
    let mut reads = futures::stream::iter(fetches).buffer_unordered(CONCURRENT_READS);
    while let Some(read) = reads.next().await {
        fetched_bytes += read?;
        done += 1;
        progress(done, total);
    }
    drop(reads);

    // 3. The layout, then the disk check.
    let into = into.to_owned();
    let dir = into.clone();
    let blob_paths: Vec<(String, Mode, ObjectId, PathBuf)> = entries
        .iter()
        .map(|e| (e.path.clone(), e.mode, e.id, cache.path(&e.id)))
        .collect();
    let tree_id = spec.tree_id.clone();
    let (bytes, checks_file) =
        tokio::task::spawn_blocking(move || -> Result<_, MaterializeError> {
            let laid = lay_out(&dir, &blob_paths)?;
            if verify == Verify::Disk {
                let submodules: Vec<(String, ObjectId)> = blob_paths
                    .iter()
                    .filter(|(_, mode, _, _)| *mode == Mode::Submodule)
                    .map(|(path, _, id, _)| (path.clone(), *id))
                    .collect();
                let disk = githash::disk_tree_id(&dir, &submodules)?
                    .map_err(|error| MaterializeError::Disk(error.to_string()))?;
                if githash::hex(&disk) != tree_id {
                    return Err(MaterializeError::Disk(format!(
                        "the laid-out directory hashes to tree {}, not the job's {tree_id}",
                        githash::hex(&disk)
                    )));
                }
            }
            Ok(laid)
        })
        .await
        .map_err(|error| MaterializeError::Io(std::io::Error::other(error)))??;
    Ok(Tree {
        dir: into,
        entries: entries.len(),
        bytes,
        fetched: wanted.len(),
        fetched_bytes,
        verify,
        checks_file,
    })
}

/// Reads one blob into the cache; the bytes read.
async fn fetch(
    client: &Client,
    repo: &str,
    commit: &str,
    path: &str,
    id: ObjectId,
    cache: &BlobCache,
) -> Result<u64, MaterializeError> {
    let hex = githash::hex(&id);
    let mut file = tempfile::NamedTempFile::new_in(cache.dir.join("tmp"))?;
    let mut hasher: Option<BlobHasher> = None;
    let mut offset = 0u64;
    let mut size = 0u64;
    loop {
        let read = client
            .read_file(v1::ReadFileRequest {
                repo: repo.to_owned(),
                revision: commit.to_owned(),
                path: path.to_owned(),
                skip_snapshot: true,
                offset,
                ..v1::ReadFileRequest::default()
            })
            .await?;
        if read.commit_id != commit {
            return Err(MaterializeError::Blob(format!(
                "{path}: the daemon read commit {}, not {commit}",
                read.commit_id
            )));
        }
        if let Some(entry) = &read.entry
            && !entry.file_id.is_empty()
            && entry.file_id != hex
        {
            return Err(MaterializeError::Blob(format!(
                "{path}: the daemon read blob {}, not the listed {hex}",
                entry.file_id
            )));
        }
        let hasher = hasher.get_or_insert_with(|| {
            size = read.size;
            BlobHasher::new(read.size)
        });
        hasher.update(&read.content);
        file.write_all(&read.content)?;
        offset += read.content.len() as u64;
        if offset >= size || read.content.is_empty() {
            break;
        }
    }
    if offset != size {
        return Err(MaterializeError::Blob(format!(
            "{path}: read {offset} bytes of {size}"
        )));
    }
    let got = hasher
        .unwrap_or_else(|| BlobHasher::new(0))
        .finish(path)
        .map_err(|error| MaterializeError::Blob(error.to_string()))?;
    if got != id {
        return Err(MaterializeError::Blob(format!(
            "{path}: the bytes read hash to {}, not the listed {hex}",
            githash::hex(&got)
        )));
    }
    let target = cache.path(&id);
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    file.flush()?;
    file.persist(&target).map_err(|error| error.error)?;
    Ok(size)
}

/// Lays the entries out under `dir` from the cache. Returns the blobs'
/// bytes and the tree's `reef-checks.toml`.
fn lay_out(
    dir: &Path,
    entries: &[(String, Mode, ObjectId, PathBuf)],
) -> Result<(u64, Option<Vec<u8>>), MaterializeError> {
    std::fs::DirBuilder::new().mode(0o700).create(dir)?;
    let mut made: std::collections::BTreeSet<String> = std::collections::BTreeSet::new();
    let mut bytes = 0u64;
    let mut checks_file = None;
    let now = std::time::SystemTime::now();
    for (path, mode, _, blob) in entries {
        // Parents: created here, so none is a link.
        let components: Vec<&str> = path.split('/').collect();
        let mut parent = String::new();
        for component in &components[..components.len() - 1] {
            if !parent.is_empty() {
                parent.push('/');
            }
            parent.push_str(component);
            if made.insert(parent.clone()) {
                std::fs::DirBuilder::new()
                    .mode(0o755)
                    .create(dir.join(&parent))?;
            }
        }
        let target = dir.join(path);
        match mode {
            Mode::Submodule => {
                std::fs::DirBuilder::new().mode(0o755).create(&target)?;
                made.insert(path.clone());
            }
            Mode::Symlink => {
                let link = std::fs::read(blob)?;
                bytes += link.len() as u64;
                std::os::unix::fs::symlink(
                    <std::ffi::OsStr as std::os::unix::ffi::OsStrExt>::from_bytes(&link),
                    &target,
                )?;
                touch(blob, now);
            }
            Mode::File | Mode::Executable => {
                let permissions = if *mode == Mode::Executable {
                    0o755
                } else {
                    0o644
                };
                let mut out = std::fs::OpenOptions::new()
                    .write(true)
                    .create_new(true)
                    .custom_flags(libc::O_NOFOLLOW)
                    .mode(permissions)
                    .open(&target)?;
                let mut source = std::fs::File::open(blob)?;
                bytes += std::io::copy(&mut source, &mut out)?;
                out.set_permissions(std::fs::Permissions::from_mode(permissions))?;
                touch(blob, now);
                if path == crate::CHECKS_FILE && out.metadata()?.len() <= MAX_CHECKS_FILE {
                    checks_file = Some(std::fs::read(&target)?);
                }
            }
        }
    }
    Ok((bytes, checks_file))
}

/// Marks a cache file used, for eviction.
fn touch(path: &Path, now: std::time::SystemTime) {
    if let Ok(file) = std::fs::File::options().write(true).open(path) {
        let _ = file.set_modified(now);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eviction_removes_the_least_recently_used() {
        let dir = tempfile::tempdir().unwrap();
        let cache = dir.path();
        std::fs::create_dir_all(cache.join("aa")).unwrap();
        std::fs::create_dir_all(cache.join("tmp")).unwrap();
        let old = std::time::SystemTime::now() - std::time::Duration::from_secs(100);
        for (name, age) in [("old", old), ("new", std::time::SystemTime::now())] {
            let path = cache.join("aa").join(name);
            std::fs::write(&path, [0u8; 600]).unwrap();
            std::fs::File::options()
                .write(true)
                .open(&path)
                .unwrap()
                .set_modified(age)
                .unwrap();
        }
        assert_eq!(evict_in(cache, 1000).unwrap(), 600);
        assert!(!cache.join("aa/old").exists());
        assert!(cache.join("aa/new").exists());
        assert_eq!(evict_in(cache, 1000).unwrap(), 0);
    }
}
