//! One notify watcher per daemon (R1.13): with 20 active workspaces the
//! process holds a single inotify instance. A file of its own because it
//! counts the test process's descriptors, which daemons started by tests
//! running in parallel would share.

#![cfg(target_os = "linux")]
#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

mod common;

use common::TestDaemon;
use reef_daemon::proto::v0::CreateRequest;
use reef_testing::synth::Preset;

/// Open file descriptors of this process that are inotify instances.
fn inotify_instances() -> usize {
    std::fs::read_dir("/proc/self/fd")
        .unwrap()
        .filter_map(|entry| std::fs::read_link(entry.ok()?.path()).ok())
        .filter(|target| target.to_string_lossy() == "anon_inode:inotify")
        .count()
}

#[tokio::test(flavor = "multi_thread")]
async fn one_inotify_instance_for_twenty_workspaces() {
    let (daemon, repo) = TestDaemon::with_synthetic_repo(Preset::Small).await;
    for _ in 0..20 {
        daemon
            .client
            .create_workspace(CreateRequest {
                repo: repo.id.clone(),
                owner: "tester@host".to_owned(),
                ..CreateRequest::default()
            })
            .await
            .unwrap();
    }
    assert_eq!(daemon.handle.state().manager.loaded(), 20);
    assert_eq!(inotify_instances(), 1);
}
