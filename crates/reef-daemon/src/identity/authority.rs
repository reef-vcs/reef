//! The daemon's issuing authority (R3.03, ADR 0057): one current ed25519
//! key, its private half PKCS#8 PEM at `<state>/identity/authority.pem`
//! (`0600` in a `0700` directory, never backed up), its public half and key
//! id in `identity_authorities` (backed up, kept for good).
//!
//! At every start the key file is loaded and checked against the current
//! row. A missing, unreadable or mismatched file (a restore, a new machine)
//! retires the current row and makes a new key and row, logging both ids:
//! certificates the old key issued still verify by its kept public key;
//! only new issuance moves. A file group or others can read is changed to
//! `0600` with a warning. A new key is written to a temporary file created
//! new with mode `0600`, synced, renamed into place, and the directory
//! synced, so a crash never leaves half a key.

use std::io::{self, Write as _};
use std::os::unix::fs::{OpenOptionsExt as _, PermissionsExt as _};
use std::path::{Path, PathBuf};

use reef_evidence::{KeyError, SigningKey};
use reef_lib::error::{Classify, ErrorKind};
use zeroize::Zeroizing;

use crate::db::{AuthorityRow, Db, DbError, RevocationRow};
use crate::error::{Advice, Advise};
use crate::model::Timestamp;

/// The key file's name in `<state>/identity`.
pub const KEY_FILE: &str = "authority.pem";

/// The current authority: its private key, held in memory, and its row.
pub struct Current {
    /// The private key.
    pub key: SigningKey,
    /// Its row.
    pub row: AuthorityRow,
}

impl std::fmt::Debug for Current {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Current")
            .field("key_id", &self.row.id)
            .finish_non_exhaustive()
    }
}

/// Why the authority key could not be loaded or made.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum AuthorityError {
    /// The key file or its directory cannot be written:
    /// `AUTHORITY_UNAVAILABLE`.
    #[error("write the authority key {}", path.display())]
    Unavailable {
        /// The file or directory.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The directory belongs to another user.
    #[error("{} is not the daemon user's", path.display())]
    NotOwned {
        /// The directory.
        path: PathBuf,
    },
    /// The system's random number generator failed.
    #[error("generate the authority key")]
    Generate(#[source] KeyError),
    /// The database failed.
    #[error(transparent)]
    Db(#[from] DbError),
}

impl Classify for AuthorityError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Unavailable { .. } => ErrorKind::Unavailable,
            Self::NotOwned { .. } => ErrorKind::PermissionDenied,
            Self::Generate(_) => ErrorKind::Internal,
            Self::Db(error) => error.kind(),
        }
    }
}

impl Advise for AuthorityError {
    fn advice(&self) -> Option<Advice> {
        match self {
            Self::Unavailable { path, .. } => Some(Advice::hint(format!(
                "Make {} writable by the daemon's user; the daemon keeps its issuing key there.",
                path.display()
            ))),
            Self::NotOwned { path } => Some(Advice::hint(format!(
                "Remove {} or give it to the daemon's user; it holds the daemon's issuing key.",
                path.display()
            ))),
            Self::Db(error) => error.advice(),
            Self::Generate(_) => None,
        }
    }
}

/// Why a key file was not used.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Unused {
    /// There is none.
    Missing,
    /// It cannot be read or is not an ed25519 PKCS#8 key.
    Unreadable,
    /// It is not the current authority's key.
    Mismatch,
    /// No authority is current (the first start, or a restore).
    NoCurrent,
}

impl Unused {
    fn as_str(self) -> &'static str {
        match self {
            Unused::Missing => "missing",
            Unused::Unreadable => "unreadable",
            Unused::Mismatch => "mismatch",
            Unused::NoCurrent => "no_current",
        }
    }
}

/// The current authority, as the start sequence finds or makes it.
pub fn load(dir: &Path, db: &Db, host: &str) -> Result<Current, AuthorityError> {
    private_dir(dir)?;
    let path = dir.join(KEY_FILE);
    let rows = db.authorities()?;
    let current = rows.iter().find(|row| row.retired_at.is_none());
    let unused = match (read_key(&path)?, current) {
        (Ok(key), Some(row))
            if key.public_key().id().as_str() == row.id
                && key.public_key().spki() == row.public_key.as_slice() =>
        {
            return Ok(Current {
                key,
                row: row.clone(),
            });
        }
        (Ok(_), Some(_)) => Unused::Mismatch,
        (_, None) => Unused::NoCurrent,
        (Err(unused), Some(_)) => unused,
    };
    let made = generate(dir, host)?;
    db.replace_authority(&made.row, None)?;
    match current {
        Some(old) => tracing::warn!(
            retired_key_id = %old.id,
            key_id = %made.row.id,
            reason = unused.as_str(),
            "the authority key file is not the current authority's; retired it and made a new authority key, whose certificates start now (old certificates still verify)"
        ),
        None if rows.is_empty() => tracing::info!(
            key_id = %made.row.id,
            path = %path.display(),
            "made the daemon's authority key"
        ),
        None => tracing::warn!(
            key_id = %made.row.id,
            "no authority was current (a restore retires it); made a new authority key"
        ),
    }
    super::audit::authority_changed("create", current.map(|row| row.id.as_str()), &made.row.id);
    Ok(made)
}

/// A new current authority replacing `old`: a new key file and row, `old`
/// retired, and revoked with `revoke` when given, in one transaction.
pub fn rotate(
    dir: &Path,
    db: &Db,
    host: &str,
    old: &AuthorityRow,
    revoke: Option<&RevocationRow>,
) -> Result<Current, AuthorityError> {
    private_dir(dir)?;
    let made = generate(dir, host)?;
    db.replace_authority(
        &made.row,
        revoke.map(|revocation| (old.id.as_str(), revocation)),
    )?;
    super::audit::authority_changed("rotate", Some(&old.id), &made.row.id);
    Ok(made)
}

fn private_dir(dir: &Path) -> Result<(), AuthorityError> {
    match crate::daemon::ensure_private_dir(dir, true) {
        Ok(Some(mode)) => crate::daemon::log_tightened(dir, mode),
        Ok(None) => {}
        Err(crate::daemon::PrivateDirError::NotOwned { path, .. }) => {
            return Err(AuthorityError::NotOwned { path });
        }
        Err(crate::daemon::PrivateDirError::Io { path, source }) => {
            return Err(AuthorityError::Unavailable { path, source });
        }
    }
    Ok(())
}

/// The key in `path`: `Err(Unused)` when there is none or it is not one.
/// A file group or others can read is changed to `0600` first.
fn read_key(path: &Path) -> Result<Result<SigningKey, Unused>, AuthorityError> {
    let metadata = match std::fs::symlink_metadata(path) {
        Ok(metadata) => metadata,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Err(Unused::Missing)),
        Err(_) => return Ok(Err(Unused::Unreadable)),
    };
    if !metadata.is_file() {
        return Ok(Err(Unused::Unreadable));
    }
    let mode = metadata.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(
            |source| AuthorityError::Unavailable {
                path: path.to_owned(),
                source,
            },
        )?;
        tracing::warn!(
            path = %path.display(),
            mode = format!("{mode:04o}"),
            "the authority key file could be read by group or others; changed it to 0600"
        );
    }
    let Ok(text) = std::fs::read_to_string(path).map(Zeroizing::new) else {
        return Ok(Err(Unused::Unreadable));
    };
    Ok(SigningKey::from_pkcs8_pem(&text).map_err(|_| Unused::Unreadable))
}

/// A new key, written to `dir`'s key file, and its row.
fn generate(dir: &Path, host: &str) -> Result<Current, AuthorityError> {
    let (private_pem, _) = SigningKey::generate().map_err(AuthorityError::Generate)?;
    let private_pem = Zeroizing::new(private_pem);
    let key = SigningKey::from_pkcs8_pem(&private_pem).map_err(AuthorityError::Generate)?;
    let path = dir.join(KEY_FILE);
    write_private(dir, &path, private_pem.as_bytes()).map_err(|source| {
        AuthorityError::Unavailable {
            path: path.clone(),
            source,
        }
    })?;
    let public = key.public_key();
    let row = AuthorityRow {
        id: public.id().to_string(),
        algorithm: public.algorithm().to_owned(),
        public_key: public.spki().to_vec(),
        host: host.to_owned(),
        created_at: Timestamp::now(),
        retired_at: None,
        revocation: None,
        kind: "reef-daemon".to_owned(),
    };
    Ok(Current { key, row })
}

/// Writes `bytes` to `path` in `dir`: a temporary file created new with
/// mode `0600`, synced, renamed over `path`, and the directory synced.
fn write_private(dir: &Path, path: &Path, bytes: &[u8]) -> io::Result<()> {
    let tmp = dir.join(format!(
        ".{KEY_FILE}.{}.{}.tmp",
        std::process::id(),
        super::new_id("")
    ));
    let written = (|| {
        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&tmp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        std::fs::rename(&tmp, path)?;
        std::fs::File::open(dir)?.sync_all()
    })();
    if written.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    written
}

#[cfg(test)]
mod tests {
    use super::*;

    fn mode(path: &Path) -> u32 {
        std::fs::metadata(path).unwrap().permissions().mode() & 0o777
    }

    // R3.03 test 4, the authority's part.
    #[test]
    fn the_authority_is_made_kept_and_replaced() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("identity");
        let db = Db::open_in_memory().unwrap();
        let first = load(&dir, &db, "host").unwrap();
        assert_eq!(mode(&dir), 0o700);
        assert_eq!(mode(&dir.join(KEY_FILE)), 0o600);
        assert_eq!(db.authorities().unwrap().len(), 1);
        // A second load keeps it.
        let again = load(&dir, &db, "host").unwrap();
        assert_eq!(again.row, first.row);
        assert_eq!(db.authorities().unwrap().len(), 1);
        // A loosened mode is tightened, and the key kept.
        std::fs::set_permissions(dir.join(KEY_FILE), std::fs::Permissions::from_mode(0o644))
            .unwrap();
        let tightened = load(&dir, &db, "host").unwrap();
        assert_eq!(tightened.row.id, first.row.id);
        assert_eq!(mode(&dir.join(KEY_FILE)), 0o600);
        // A missing file retires the authority and makes another.
        std::fs::remove_file(dir.join(KEY_FILE)).unwrap();
        let second = load(&dir, &db, "host").unwrap();
        assert_ne!(second.row.id, first.row.id);
        let rows = db.authorities().unwrap();
        assert_eq!(rows.len(), 2);
        assert!(
            rows.iter()
                .any(|row| row.id == first.row.id && row.retired_at.is_some())
        );
        // Another key in the file does too.
        let (other, _) = SigningKey::generate().unwrap();
        std::fs::write(dir.join(KEY_FILE), other).unwrap();
        let third = load(&dir, &db, "host").unwrap();
        assert_ne!(third.row.id, second.row.id);
        assert_eq!(db.authorities().unwrap().len(), 3);
        // A file that matches a retired row, with nothing current (what a
        // restore leaves): a new authority, not the retired one again.
        let fourth_file = std::fs::read(dir.join(KEY_FILE)).unwrap();
        db.retire_authorities(Timestamp::now()).unwrap();
        let fifth = load(&dir, &db, "host").unwrap();
        assert_ne!(fifth.row.id, third.row.id);
        assert_ne!(std::fs::read(dir.join(KEY_FILE)).unwrap(), fourth_file);
        assert_eq!(db.authorities().unwrap().len(), 4);
        assert_eq!(
            db.authorities()
                .unwrap()
                .iter()
                .filter(|row| row.retired_at.is_none())
                .count(),
            1
        );
        // No temporary file is left.
        let names: Vec<String> = std::fs::read_dir(&dir)
            .unwrap()
            .map(|entry| entry.unwrap().file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names, [KEY_FILE]);
    }
}
