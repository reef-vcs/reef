//! A daemon in a temporary directory for integration tests, here and in
//! `reef-vcs` (enabled by the `testing` feature).

#![allow(
    clippy::expect_used,
    reason = "test fixtures; a panic is the failure report"
)]

use std::path::{Path, PathBuf};

use reef_lib::jj::Repo;
use reef_lib::jj::settings::{Identity, user_settings_with_max_new_file_size};
use reef_testing::synth::{self, Preset, Spec};
use tempfile::TempDir;

use crate::client::Client;
use crate::config::{Config, Paths};
use crate::daemon::{Daemon, DaemonHandle};
use crate::logging::LogOptions;

/// Above the generator's largest files, so a synthetic tree is tracked in
/// full.
const MAX_NEW_FILE_SIZE: u64 = 16 << 20;

/// A running daemon with its own state directory and a connected client.
#[derive(Debug)]
pub struct TestDaemon {
    /// The daemon.
    pub handle: DaemonHandle,
    /// A client connected to it.
    pub client: Client,
    /// The temporary directory holding `state/`, the daemon's state
    /// directory, and `repo/` for [`TestDaemon::with_synthetic_repo`].
    /// Removed on drop.
    pub tmp: TempDir,
    /// The configuration it was started with.
    pub config: Config,
}

impl TestDaemon {
    /// Starts a daemon on a fresh state directory. Needs a multi-threaded
    /// runtime (`#[tokio::test(flavor = "multi_thread")]`).
    pub async fn start() -> TestDaemon {
        let tmp = tempfile::Builder::new()
            .prefix("reef-")
            .tempdir_in(short_tmp())
            .expect("create a temporary directory");
        let paths = Paths::in_state_dir(&tmp.path().join("state")).expect("resolve paths");
        let config = Config {
            paths,
            log: LogOptions::default(),
        };
        let handle = Daemon::start(config.clone())
            .await
            .expect("start the daemon");
        let client = Client::connect(handle.socket_path())
            .await
            .expect("connect to the daemon");
        TestDaemon {
            handle,
            client,
            tmp,
            config,
        }
    }

    /// Starts a daemon and generates a `preset` synthetic tree at
    /// `<tmp>/repo`, made a jj repository and snapshotted once. Returns the
    /// repository root. R1.05 registers it with the daemon.
    pub async fn with_synthetic_repo(preset: Preset) -> (TestDaemon, PathBuf) {
        let daemon = TestDaemon::start().await;
        let root = daemon.tmp.path().join("repo");
        let at = root.clone();
        tokio::task::spawn_blocking(move || init_synthetic_repo(&at, preset))
            .await
            .expect("generate the synthetic repository");
        (daemon, root)
    }

    /// The daemon's state directory.
    pub fn state_dir(&self) -> &Path {
        &self.config.paths.state_dir
    }

    /// Shuts the daemon down and waits for it to stop.
    pub async fn stop(self) {
        self.handle.shutdown();
        self.handle.join().await.expect("the daemon stops cleanly");
    }
}

/// Generates a `preset` tree at `root` (seed 7), makes it a jj repository,
/// and snapshots it once.
pub fn init_synthetic_repo(root: &Path, preset: Preset) {
    synth::generate(root, &Spec::preset(preset), 7, false).expect("generate the tree");
    let mut repo = pollster::block_on(Repo::init_internal_git(root, &test_settings()))
        .expect("create the jj repository");
    pollster::block_on(repo.snapshot("snapshot")).expect("snapshot the tree");
}

/// jj settings for test repositories.
pub fn test_settings() -> reef_lib::jj::UserSettings {
    user_settings_with_max_new_file_size(
        &Identity {
            name: "Reef Test".into(),
            email: "reef-test@example.com".into(),
        },
        MAX_NEW_FILE_SIZE,
    )
    .expect("build jj settings")
}

/// `/tmp` on macOS, where `$TMPDIR` (`/var/folders/...`) makes socket paths
/// too long for `sockaddr_un`; the usual temporary directory elsewhere.
pub fn short_tmp() -> PathBuf {
    let tmp = Path::new("/tmp");
    if cfg!(target_os = "macos") && tmp.is_dir() {
        tmp.to_owned()
    } else {
        std::env::temp_dir()
    }
}
