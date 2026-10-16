//! The events and spans `reef-lib` emits (R0.09): entry points open a span
//! with their identifying fields, state changes are logged at `info`, and a
//! failed operation is logged exactly once, with its error chain.

#![allow(
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::io;
use std::path::Path;
use std::sync::{Arc, Mutex};

use jj_lib::settings::UserSettings;
use jj_lib::workspace::Workspace;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_lib::jj::Repo;
use reef_lib::jj::settings::{Identity, user_settings};
use serde_json::Value;
use tracing::subscriber::DefaultGuard;
use tracing_subscriber::fmt::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;

/// Collects the JSON log lines written while it is the default subscriber.
#[derive(Clone, Default)]
struct Capture(Arc<Mutex<Vec<u8>>>);

impl io::Write for Capture {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        self.0.lock().expect("capture lock").extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

impl<'a> MakeWriter<'a> for Capture {
    type Writer = Self;

    fn make_writer(&'a self) -> Self {
        self.clone()
    }
}

impl Capture {
    /// Installs a subscriber shaped like the daemon's JSON one, for this
    /// thread, at `debug`.
    fn install(&self) -> DefaultGuard {
        let layer = tracing_subscriber::fmt::layer()
            .json()
            .flatten_event(true)
            .with_current_span(true)
            .with_span_list(false)
            .with_writer(self.clone());
        let subscriber = tracing_subscriber::registry()
            .with(tracing_subscriber::filter::LevelFilter::DEBUG)
            .with(layer);
        tracing::subscriber::set_default(subscriber)
    }

    fn events(&self) -> Vec<Value> {
        let bytes = self.0.lock().expect("capture lock").clone();
        String::from_utf8(bytes)
            .expect("utf-8 logs")
            .lines()
            .map(|line| serde_json::from_str(line).expect("each line is JSON"))
            .collect()
    }

    fn with_message(&self, message: &str) -> Vec<Value> {
        self.events()
            .into_iter()
            .filter(|event| event["message"] == message)
            .collect()
    }
}

fn settings() -> UserSettings {
    user_settings(&Identity {
        name: "Reef Test".into(),
        email: "reef-test@example.com".into(),
    })
    .expect("settings")
}

async fn init_and_open(root: &Path) -> Repo {
    Workspace::init_internal_git(&settings(), root, gix::hash::Kind::Sha1)
        .await
        .expect("init internal git repo");
    Repo::open(root, &settings()).await.expect("open repo")
}

#[tokio::test]
async fn failure_is_logged_once_with_its_chain_in_the_span() {
    let capture = Capture::default();
    let _guard = capture.install();
    let dir = tempfile::tempdir().unwrap();

    let Err(err) = Repo::open(dir.path(), &settings()).await else {
        panic!("opened a directory with no repository");
    };
    assert_eq!(err.kind(), ErrorKind::NotFound);

    let failures = capture.with_message("operation failed");
    assert_eq!(failures.len(), 1, "{failures:#?}");
    let event = &failures[0];
    assert_eq!(event["level"], "WARN", "a caller error is a warning");
    assert!(
        event["error"]
            .as_str()
            .unwrap()
            .starts_with("no jj repository at "),
        "{event}"
    );
    assert_eq!(event["span"]["name"], "open");
    assert_eq!(
        event["span"]["path"],
        dir.path().display().to_string().as_str()
    );
}

#[tokio::test]
async fn snapshot_is_logged_in_its_span() {
    let capture = Capture::default();
    let _guard = capture.install();
    let dir = tempfile::tempdir().unwrap();
    let mut repo = init_and_open(dir.path()).await;
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();

    let outcome = repo.snapshot("snapshot working copy").await.unwrap();

    let taken = capture.with_message("snapshot taken");
    assert_eq!(taken.len(), 1, "{taken:#?}");
    let event = &taken[0];
    assert_eq!(event["level"], "INFO");
    assert_eq!(event["op_id"], outcome.op_id.as_str());
    assert_eq!(event["commit_id"], outcome.wc_commit.as_str());
    assert!(event["elapsed_ms"].is_u64(), "{event}");
    assert_eq!(event["span"]["name"], "snapshot");
    assert_eq!(
        event["span"]["repo"],
        repo.workspace_root().display().to_string().as_str()
    );
}

/// `undo_last` snapshots first. When that snapshot fails, the failure is
/// reported by `undo_last`'s span only, not again by `snapshot`'s.
#[tokio::test]
async fn nested_failure_is_logged_by_the_outer_operation_only() {
    let dir = tempfile::tempdir().unwrap();
    let mut stale = init_and_open(dir.path()).await;
    let mut fresh = Repo::open(dir.path(), &settings()).await.unwrap();
    std::fs::write(dir.path().join("a.txt"), "a").unwrap();
    fresh.snapshot("snapshot working copy").await.unwrap();

    let capture = Capture::default();
    let _guard = capture.install();
    let err = stale.undo_last().await.unwrap_err();
    assert_eq!(err.kind(), ErrorKind::Conflict, "{err}");

    let failures = capture.with_message("operation failed");
    assert_eq!(failures.len(), 1, "{failures:#?}");
    assert_eq!(failures[0]["span"]["name"], "undo_last");
    assert!(
        failures[0]["error"]
            .as_str()
            .unwrap()
            .starts_with("failed to snapshot the working copy before undo: working copy is at"),
        "{}",
        failures[0]
    );
}
