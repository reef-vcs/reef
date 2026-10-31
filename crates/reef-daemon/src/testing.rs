//! A daemon in a temporary directory for integration tests, here and in
//! `reef-vcs` (enabled by the `testing` feature).

#![allow(
    clippy::expect_used,
    reason = "test fixtures; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use reef_api::v1::IfNotJj;
use reef_lib::jj::Repo;
use reef_lib::jj::settings::{Identity, user_settings_with_max_new_file_size};
use reef_testing::synth::{self, Preset, Spec};
use tempfile::TempDir;

use crate::client::Client;
use crate::config::{Config, Paths};
use crate::daemon::{Daemon, DaemonHandle};

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
        TestDaemon::start_with(|_| {}).await
    }

    /// [`TestDaemon::start`] with the default configuration changed by
    /// `configure` first, such as shorter watcher intervals.
    pub async fn start_with(configure: impl FnOnce(&mut Config)) -> TestDaemon {
        let tmp = tempfile::Builder::new()
            .prefix("reef-")
            .tempdir_in(short_tmp())
            .expect("create a temporary directory");
        let paths = Paths::in_state_dir(&tmp.path().join("state")).expect("resolve paths");
        let mut config = Config::with_defaults(paths);
        // No timed GC partway through a long test; a test turns it on.
        config.file.gc.interval = Duration::ZERO;
        // No background indexing unless a test asks: timing tests must not
        // share the CPU with a first index (R2.14).
        config.file.symbols.background = false;
        // Serve the placeholder through the full gate: the web listener's
        // tests run on builds without the app (R2.18).
        config.web.assets_built = true;
        configure(&mut config);
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
    /// `<tmp>/repo`, made a jj repository, snapshotted once, and registered
    /// with the daemon. Returns the repository root (canonical) and its
    /// registration.
    pub async fn with_synthetic_repo(preset: Preset) -> (TestDaemon, SyntheticRepo) {
        TestDaemon::with_synthetic_repo_and(preset, |_| {}).await
    }

    /// [`TestDaemon::with_synthetic_repo`] on a daemon started with
    /// [`TestDaemon::start_with`].
    pub async fn with_synthetic_repo_and(
        preset: Preset,
        configure: impl FnOnce(&mut Config),
    ) -> (TestDaemon, SyntheticRepo) {
        let daemon = TestDaemon::start_with(configure).await;
        let root = daemon.tmp.path().join("repo");
        let at = root.clone();
        tokio::task::spawn_blocking(move || init_synthetic_repo(&at, preset))
            .await
            .expect("generate the synthetic repository");
        let registered = daemon
            .client
            .register_repository(&root, IfNotJj::Fail)
            .await
            .expect("register the synthetic repository");
        let repository = registered.repository.expect("a repository in the response");
        let repo = SyntheticRepo {
            root: std::fs::canonicalize(&root).expect("resolve the repository root"),
            id: repository.id,
        };
        (daemon, repo)
    }

    /// The daemon's state directory.
    pub fn state_dir(&self) -> &Path {
        &self.config.paths.state_dir
    }

    /// Stops the daemon and starts a new one on the same state directory
    /// and configuration, with a new client.
    pub async fn restart(self) -> TestDaemon {
        let TestDaemon {
            handle,
            client,
            tmp,
            config,
        } = self;
        drop(client);
        handle.shutdown();
        handle.join().await.expect("the daemon stops cleanly");
        let handle = Daemon::start(config.clone())
            .await
            .expect("restart the daemon");
        let client = Client::connect(handle.socket_path())
            .await
            .expect("connect to the restarted daemon");
        TestDaemon {
            handle,
            client,
            tmp,
            config,
        }
    }

    /// Shuts the daemon down and waits for it to stop.
    pub async fn stop(self) {
        self.handle.shutdown();
        self.handle.join().await.expect("the daemon stops cleanly");
    }

    /// Shuts the daemon down and waits for it to stop, keeping its
    /// temporary directory, so a test can change the state directory with
    /// no daemon running (R1.18's restore) and [`StoppedDaemon::start`] one
    /// there again.
    pub async fn stop_keeping(self) -> StoppedDaemon {
        let TestDaemon {
            handle,
            client,
            tmp,
            config,
        } = self;
        drop(client);
        handle.shutdown();
        handle.join().await.expect("the daemon stops cleanly");
        StoppedDaemon { tmp, config }
    }
}

/// A [`TestDaemon`] stopped by [`TestDaemon::stop_keeping`].
#[derive(Debug)]
pub struct StoppedDaemon {
    /// The temporary directory, as [`TestDaemon::tmp`].
    pub tmp: TempDir,
    /// The configuration it ran with.
    pub config: Config,
}

impl StoppedDaemon {
    /// Starts a daemon on the same state directory and configuration.
    pub async fn start(self) -> TestDaemon {
        let StoppedDaemon { tmp, config } = self;
        let handle = Daemon::start(config.clone())
            .await
            .expect("start the daemon again");
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
}

/// A synthetic repository registered with a [`TestDaemon`].
#[derive(Clone, Debug)]
pub struct SyntheticRepo {
    /// Its root, canonical.
    pub root: PathBuf,
    /// Its repo id, as the daemon returned it.
    pub id: String,
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
