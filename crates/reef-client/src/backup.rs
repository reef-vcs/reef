//! A backup's `manifest.json` (R1.18, ADR 0020), as `reef backup` reads it
//! back to print it. The daemon writes it and `restore` checks it; both use
//! these types.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use reef_error::{Classify, ErrorKind, classify_by_source};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// The manifest's `version`.
pub const MANIFEST_VERSION: u32 = 1;
/// The manifest's file name.
pub const MANIFEST_FILE: &str = "manifest.json";

/// `manifest.json`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct Manifest {
    /// [`MANIFEST_VERSION`].
    pub version: u32,
    /// When the backup began, RFC 3339.
    pub created_at: String,
    /// The database's `meta.daemon_instance_id`.
    pub daemon_instance_id: String,
    /// The daemon's version.
    pub reef_version: String,
    /// The jj-lib version of the daemon that wrote it.
    pub jj_lib_version: String,
    /// The state directory backed up.
    pub state_dir: PathBuf,
    /// The repositories, in registration order.
    pub repos: Vec<RepoManifest>,
    /// The database copy.
    pub sqlite: SqliteManifest,
    /// Every repository's heads are written; a manifest without it is an
    /// interrupted backup, which restore refuses.
    #[serde(default)]
    pub complete: bool,
}

/// One repository in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RepoManifest {
    /// The repository's id, in hex.
    pub id: String,
    /// Its registered root.
    pub root: PathBuf,
    /// Its `.jj/repo` when backed up.
    pub jj_repo_path: PathBuf,
    /// Whether jj and git share the working copy (`git/` holds `.git`).
    pub colocated: bool,
    /// The operation heads, as `heads.json` has them.
    pub op_heads: Vec<String>,
    /// Files copied.
    pub files: u64,
    /// Bytes copied.
    pub bytes: u64,
    /// How long the repository thread was paused for step 3.
    pub pause_ms: u64,
}

/// The database copy in the manifest.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct SqliteManifest {
    /// Pages copied.
    pub pages: u64,
    /// The copy's size.
    pub bytes: u64,
}

impl Manifest {
    /// Reads `<dir>/manifest.json`.
    pub fn read(dir: &Path) -> Result<Manifest, ManifestError> {
        let path = dir.join(MANIFEST_FILE);
        let text = fs::read_to_string(&path).map_err(|source| ManifestError::Read {
            path: path.clone(),
            source,
        })?;
        serde_json::from_str(&text).map_err(|source| ManifestError::Parse { path, source })
    }

    /// The repository with this id, in hex.
    pub fn repo(&self, id: &str) -> Option<&RepoManifest> {
        self.repos.iter().find(|repo| repo.id == id)
    }
}

/// Failure to read a manifest.
#[derive(Debug, thiserror::Error)]
#[non_exhaustive]
pub enum ManifestError {
    /// The file could not be read.
    #[error("read {}", path.display())]
    Read {
        /// The manifest.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
    /// The file is not a manifest.
    #[error("parse {}", path.display())]
    Parse {
        /// The manifest.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: serde_json::Error,
    },
}

impl Classify for ManifestError {
    fn kind(&self) -> ErrorKind {
        match self {
            Self::Read { source, .. } if source.kind() == io::ErrorKind::NotFound => {
                ErrorKind::NotFound
            }
            Self::Read { source, .. } => classify_by_source(source),
            Self::Parse { .. } => ErrorKind::InvalidArgument,
        }
    }
}
