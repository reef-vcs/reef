//! [`LocalArtifacts`]: [`ArtifactBytes`] over files under a repository's
//! state directory (R3.02):
//!
//! ```text
//! <dir>/sha256/<first 2 hex>/<remaining 62 hex>   stored, mode 0444
//! <dir>/incoming/<sha256>.<size>                  a partial upload, mode 0600
//! ```
//!
//! A partial upload's size is in its name, so an upload declaring another
//! size never appends to it. An append opens the file with `O_APPEND` and
//! checks its length first, under one of 32 mutexes picked by the
//! file's name, so two writers to one partial upload never interleave
//! inside a chunk. A check fsyncs the complete file and hashes it (ring,
//! 64 KiB reads); a commit then makes it read-only and renames it into
//! `sha256/`, and fsyncs the directory.
//!
//! Directories are made one level at a time under `<dir>`, and `<dir>`
//! only inside an existing repository directory: nothing here recreates a
//! forgotten repository's state directory.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Seek as _, SeekFrom, Write as _};
use std::os::unix::fs::{MetadataExt as _, OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};
use std::time::SystemTime;

use super::Digest;
use super::store::{AppendError, ArtifactBytes, CommitError};

/// How many mutexes appends are spread over.
const STRIPES: usize = 32;

/// The store of one repository's artifacts, rooted at its `artifacts/`
/// directory.
#[derive(Debug)]
pub struct LocalArtifacts {
    dir: PathBuf,
    stripes: [Mutex<()>; STRIPES],
}

impl LocalArtifacts {
    /// A store rooted at `dir`, created as files arrive.
    pub fn new(dir: PathBuf) -> LocalArtifacts {
        LocalArtifacts {
            dir,
            stripes: std::array::from_fn(|_| Mutex::new(())),
        }
    }

    /// Where stored bytes of `digest` live.
    pub fn stored_path(&self, digest: &Digest) -> PathBuf {
        let (fan, rest) = digest.as_str().split_at(2);
        self.dir.join("sha256").join(fan).join(rest)
    }

    /// Where a partial upload of (`digest`, `size`) lives.
    pub fn incoming_path(&self, digest: &Digest, size: u64) -> PathBuf {
        self.dir.join("incoming").join(format!("{digest}.{size}"))
    }

    /// Makes `<dir>` and each of `parts` below it, one level at a time;
    /// the deepest.
    fn make_dirs(&self, parts: &[&str]) -> io::Result<PathBuf> {
        make_dir(&self.dir)?;
        let mut dir = self.dir.clone();
        for part in parts {
            dir.push(part);
            make_dir(&dir)?;
        }
        Ok(dir)
    }

    fn stripe(&self, digest: &Digest, size: u64) -> std::sync::MutexGuard<'_, ()> {
        let name = digest.as_str().as_bytes();
        let byte = usize::from(name[0]) ^ usize::from(name[1]);
        let index = (byte ^ usize::try_from(size % 251).unwrap_or(0)) % STRIPES;
        self.stripes[index]
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }
}

/// The length of the file at `path`; 0 when there is none.
fn length(path: &Path) -> io::Result<u64> {
    match fs::metadata(path) {
        Ok(metadata) => Ok(metadata.len()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
        Err(error) => Err(error),
    }
}

/// Makes the directory `dir` when its parent exists; `NotFound` when that
/// is gone too.
fn make_dir(dir: &Path) -> io::Result<()> {
    match fs::create_dir(dir) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => Ok(()),
        Err(error) => Err(error),
    }
}

/// fsyncs the directory `dir`, so a rename into it is durable.
fn sync_dir(dir: &Path) -> io::Result<()> {
    File::open(dir)?.sync_all()
}

impl ArtifactBytes for LocalArtifacts {
    fn received(&self, digest: &Digest, size: u64) -> io::Result<u64> {
        length(&self.incoming_path(digest, size))
    }

    fn append(
        &self,
        digest: &Digest,
        size: u64,
        offset: u64,
        data: &[u8],
    ) -> Result<u64, AppendError> {
        let end = offset.saturating_add(u64::try_from(data.len()).unwrap_or(u64::MAX));
        if end > size {
            return Err(AppendError::Beyond { end, size });
        }
        let path = self.incoming_path(digest, size);
        let _held = self.stripe(digest, size);
        self.make_dirs(&["incoming"])?;
        let mut file = OpenOptions::new()
            .append(true)
            .create(true)
            .mode(0o600)
            .open(&path)?;
        let received = file.metadata()?.len();
        if received != offset {
            return Err(AppendError::Offset { received });
        }
        if !data.is_empty() {
            file.write_all(data)?;
        }
        Ok(file.metadata()?.len())
    }

    fn check(&self, digest: &Digest, size: u64) -> Result<(), CommitError> {
        let path = self.incoming_path(digest, size);
        if size == 0 && !path.exists() {
            // A zero-byte artifact needs no chunk: its partial upload is
            // made here.
            self.make_dirs(&["incoming"])?;
            OpenOptions::new()
                .write(true)
                .create(true)
                .truncate(false)
                .mode(0o600)
                .open(&path)?;
        }
        let file = match File::open(&path) {
            Ok(file) => file,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CommitError::Incomplete { received: 0, size });
            }
            Err(error) => return Err(error.into()),
        };
        let received = file.metadata()?.len();
        if received != size {
            return Err(CommitError::Incomplete { received, size });
        }
        file.sync_all()?;
        let actual = reef_evidence::sha256_hex_of(&file)?;
        if actual != digest.as_str() {
            drop(file);
            match fs::remove_file(&path) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error.into()),
            }
            return Err(CommitError::Mismatch { actual });
        }
        Ok(())
    }

    fn commit(&self, digest: &Digest, size: u64) -> Result<(), CommitError> {
        let path = self.incoming_path(digest, size);
        let received = match fs::metadata(&path) {
            Ok(metadata) => metadata.len(),
            Err(error) if error.kind() == io::ErrorKind::NotFound => {
                return Err(CommitError::Incomplete { received: 0, size });
            }
            Err(error) => return Err(error.into()),
        };
        if received != size {
            return Err(CommitError::Incomplete { received, size });
        }
        fs::set_permissions(&path, fs::Permissions::from_mode(0o444))?;
        let fan = self.make_dirs(&["sha256", &digest.as_str()[..2]])?;
        fs::rename(&path, self.stored_path(digest))?;
        sync_dir(&fan)?;
        Ok(())
    }

    fn open(&self, digest: &Digest, offset: u64) -> io::Result<Box<dyn Read + Send>> {
        let mut file = File::open(self.stored_path(digest))?;
        if offset > 0 {
            file.seek(SeekFrom::Start(offset))?;
        }
        Ok(Box::new(file))
    }

    fn stat(&self, digest: &Digest) -> io::Result<Option<u64>> {
        match fs::metadata(self.stored_path(digest)) {
            Ok(metadata) => Ok(Some(metadata.len())),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(error),
        }
    }

    fn remove(&self, digest: &Digest) -> io::Result<u64> {
        let path = self.stored_path(digest);
        let allocated = match fs::symlink_metadata(&path) {
            Ok(metadata) => metadata.blocks() * 512,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(0),
            Err(error) => return Err(error),
        };
        match fs::remove_file(&path) {
            Ok(()) => Ok(allocated),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(0),
            Err(error) => Err(error),
        }
    }

    fn stored(&self) -> io::Result<Vec<(Digest, u64, SystemTime)>> {
        let root = self.dir.join("sha256");
        let fans = match fs::read_dir(&root) {
            Ok(fans) => fans,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut found = Vec::new();
        for fan in fans {
            let fan = fan?;
            let Some(prefix) = fan.file_name().to_str().map(str::to_owned) else {
                continue;
            };
            if !fan.file_type()?.is_dir() {
                continue;
            }
            for entry in fs::read_dir(fan.path())? {
                let entry = entry?;
                let Some(digest) = entry
                    .file_name()
                    .to_str()
                    .and_then(|rest| Digest::parse(&format!("{prefix}{rest}")))
                else {
                    continue;
                };
                let metadata = match entry.metadata() {
                    Ok(metadata) => metadata,
                    Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                    Err(error) => return Err(error),
                };
                if metadata.is_file() {
                    found.push((digest, metadata.len(), metadata.modified()?));
                }
            }
        }
        found.sort();
        Ok(found)
    }

    fn sweep_incoming(&self, before: SystemTime) -> io::Result<(u32, u64)> {
        let entries = match fs::read_dir(self.dir.join("incoming")) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok((0, 0)),
            Err(error) => return Err(error),
        };
        let (mut count, mut bytes) = (0u32, 0u64);
        for entry in entries {
            let entry = entry?;
            let metadata = match entry.metadata() {
                Ok(metadata) => metadata,
                Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
                Err(error) => return Err(error),
            };
            if !metadata.is_file() || metadata.modified()? >= before {
                continue;
            }
            match fs::remove_file(entry.path()) {
                Ok(()) => {
                    count += 1;
                    bytes += metadata.len();
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {}
                Err(error) => return Err(error),
            }
        }
        Ok((count, bytes))
    }
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::*;

    fn store() -> (tempfile::TempDir, LocalArtifacts) {
        let tmp = tempfile::tempdir().unwrap();
        let store = LocalArtifacts::new(tmp.path().join("artifacts"));
        (tmp, store)
    }

    // R3.02 test 2.
    #[test]
    fn local_store() {
        let (_tmp, store) = store();
        let bytes = b"hello, artifact\n";
        let digest = Digest::of(bytes);
        let size = u64::try_from(bytes.len()).unwrap();
        assert_eq!(store.received(&digest, size).unwrap(), 0);
        assert_eq!(store.append(&digest, size, 0, &bytes[..5]).unwrap(), 5);
        // An offset that is not what was received is refused.
        for wrong in [0, 3, 4] {
            assert!(matches!(
                store.append(&digest, size, wrong, &bytes[5..]),
                Err(AppendError::Offset { received: 5 })
            ));
        }
        assert!(matches!(
            store.append(&digest, size, 5, &[0; 100]),
            Err(AppendError::Beyond { .. })
        ));
        // Another size is another partial upload.
        assert_eq!(store.received(&digest, size + 1).unwrap(), 0);
        assert_eq!(store.append(&digest, size + 1, 0, b"x").unwrap(), 1);
        assert!(matches!(
            store.check(&digest, size),
            Err(CommitError::Incomplete { received: 5, .. })
        ));
        assert!(matches!(
            store.commit(&digest, size),
            Err(CommitError::Incomplete { received: 5, .. })
        ));
        assert_eq!(store.append(&digest, size, 5, &bytes[5..]).unwrap(), size);
        store.check(&digest, size).unwrap();
        store.commit(&digest, size).unwrap();
        let path = store.stored_path(&digest);
        assert_eq!(
            path,
            store
                .dir
                .join("sha256")
                .join(&digest.as_str()[..2])
                .join(&digest.as_str()[2..])
        );
        assert_eq!(fs::read(&path).unwrap(), bytes);
        assert_eq!(
            fs::metadata(&path).unwrap().permissions().mode() & 0o777,
            0o444
        );
        assert_eq!(
            store.received(&digest, size).unwrap(),
            0,
            "moved, not copied"
        );
        assert_eq!(store.stat(&digest).unwrap(), Some(size));
        let mut read = String::new();
        store
            .open(&digest, 7)
            .unwrap()
            .read_to_string(&mut read)
            .unwrap();
        assert_eq!(read, "artifact\n");
        let listed = store.stored().unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!((&listed[0].0, listed[0].1), (&digest, size));

        // Wrong bytes: the partial upload is deleted.
        let claimed = Digest::of(b"other");
        store.append(&claimed, 5, 0, b"wrong").unwrap();
        let error = store.check(&claimed, 5).unwrap_err();
        assert!(
            matches!(&error, CommitError::Mismatch { actual } if *actual == Digest::of(b"wrong").as_str()),
            "{error:?}"
        );
        assert!(!store.incoming_path(&claimed, 5).exists());
        assert_eq!(store.stat(&claimed).unwrap(), None);

        // A zero-byte artifact commits with no partial upload.
        let empty = Digest::of(b"");
        store.check(&empty, 0).unwrap();
        store.commit(&empty, 0).unwrap();
        assert_eq!(store.stat(&empty).unwrap(), Some(0));

        // Remove, and the sweep of old partial uploads.
        assert!(store.remove(&digest).unwrap() > 0);
        assert_eq!(store.remove(&digest).unwrap(), 0);
        assert_eq!(store.stat(&digest).unwrap(), None);
        let now = SystemTime::now();
        assert_eq!(
            store.sweep_incoming(now - Duration::from_secs(60)).unwrap(),
            (0, 0)
        );
        assert_eq!(
            store.sweep_incoming(now + Duration::from_secs(60)).unwrap(),
            (1, 1),
            "the other size's partial upload"
        );
    }

    #[test]
    fn concurrent_appends_never_interleave() {
        let (_tmp, store) = store();
        let bytes: Vec<u8> = (0..64 * 1024)
            .map(|n: u32| u8::try_from(n % 251).unwrap())
            .collect();
        let digest = Digest::of(&bytes);
        let size = u64::try_from(bytes.len()).unwrap();
        let chunk = 4096;
        std::thread::scope(|scope| {
            for _ in 0..4 {
                scope.spawn(|| {
                    loop {
                        let at = store.received(&digest, size).unwrap();
                        if at == size {
                            break;
                        }
                        let start = usize::try_from(at).unwrap();
                        let end = (start + chunk).min(bytes.len());
                        let _ = store.append(&digest, size, at, &bytes[start..end]);
                    }
                });
            }
        });
        store.check(&digest, size).unwrap();
        store.commit(&digest, size).unwrap();
        assert_eq!(fs::read(store.stored_path(&digest)).unwrap(), bytes);
    }

    // R3.02 review: once the repository's directory is gone (forgotten), an
    // append fails and leaves nothing behind.
    #[test]
    fn a_forgotten_repository_is_not_recreated() {
        let tmp = tempfile::tempdir().unwrap();
        let repo_dir = tmp.path().join("repos").join("aaaa000000000001");
        fs::create_dir_all(&repo_dir).unwrap();
        let store = LocalArtifacts::new(repo_dir.join("artifacts"));
        let digest = Digest::of(b"chunk");
        assert_eq!(store.append(&digest, 5, 0, b"chu").unwrap(), 3);
        fs::remove_dir_all(&repo_dir).unwrap();
        let error = store.append(&digest, 5, 3, b"nk").unwrap_err();
        assert!(
            matches!(&error, AppendError::Io(error) if error.kind() == io::ErrorKind::NotFound),
            "{error:?}"
        );
        assert!(!repo_dir.exists());
        assert!(matches!(
            store.commit(&digest, 5),
            Err(CommitError::Incomplete { received: 0, .. })
        ));
        assert!(!repo_dir.exists());
    }
}
