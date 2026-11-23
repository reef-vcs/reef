//! The shell against a real daemon (R3.39): a help request posts one banner
//! keyed by the item, starting the shell again from its saved cursor posts
//! nothing, and answering lowers the menu's count.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

use reef_api::v1::{
    self, CommentKind, CreateCommentRequest, EditFilesRequest, FileEdit, HelpInput, HelpReason,
    edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{TestDaemon, no_automatic_snapshots};
use reef_shell::menu::MenuSpec;
use reef_shell::notice::Note;
use reef_shell::{Command, Page, Platform, Shell, ShellConfig, ShellHandle};
use reef_testing::synth::Preset;
use tokio::time::Instant;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Post(String),
    Retract(Vec<String>),
}

#[derive(Clone, Default)]
struct Recorder {
    calls: Arc<Mutex<Vec<Call>>>,
    menus: Arc<Mutex<Vec<MenuSpec>>>,
}

impl Platform for Recorder {
    fn show(&self, _page: Page) {}
    fn post(&self, note: &Note) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Call::Post(note.id.clone()));
    }
    fn retract(&self, ids: &[String]) {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Call::Retract(ids.to_vec()));
    }
    fn render(&self, menu: &MenuSpec) {
        self.menus
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(menu.clone());
    }
    fn open_in_browser(&self, _launch_file: &Path) {}
}

impl Recorder {
    fn calls(&self) -> Vec<Call> {
        self.calls
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }

    async fn until_title(&self, title: &str) {
        let deadline = Instant::now() + Duration::from_secs(30);
        loop {
            let last = self
                .menus
                .lock()
                .unwrap()
                .last()
                .map(|menu| menu.title.clone());
            if last.as_deref() == Some(title) {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "the menu never said {title:?}: {last:?}"
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn until_menus(&self, what: &str, found: impl Fn(&[MenuSpec]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(60);
        while !found(&self.menus.lock().unwrap_or_else(PoisonError::into_inner)) {
            assert!(Instant::now() < deadline, "{what}");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn until(&self, what: &str, found: impl Fn(&[Call]) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !found(&self.calls()) {
            assert!(Instant::now() < deadline, "{what}: {:?}", self.calls());
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn start(daemon: &TestDaemon, data: &Path) -> (Recorder, ShellHandle) {
    let recorder = Recorder::default();
    let shell = Shell::start(
        recorder.clone(),
        ShellConfig {
            socket: daemon.handle.socket_path().to_owned(),
            data_dir: data.to_owned(),
            installed: Vec::new(),
            bundled: None,
        },
    );
    (recorder, shell)
}

async fn change(client: &Client, repo: &str) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn("root()".to_owned())),
            edits: vec![FileEdit {
                path: "a.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"a\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

async fn ask(client: &Client, repo: &str, revision: &str, session: &str) -> v1::Comment {
    client
        .create_comment(CreateCommentRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            kind: CommentKind::HelpRequest.into(),
            text: "Which format?".to_owned(),
            help: Some(HelpInput {
                reason: HelpReason::Decision.into(),
                tried: "read the docs".to_owned(),
                question: "TOML or YAML?".to_owned(),
                ..HelpInput::default()
            }),
            client_session: session.to_owned(),
            ..CreateCommentRequest::default()
        })
        .await
        .unwrap()
        .comment
        .unwrap()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_help_request_posts_once_and_a_restart_posts_nothing() {
    let (daemon, repo) =
        TestDaemon::with_synthetic_repo_and(Preset::Small, no_automatic_snapshots).await;
    let client = &daemon.client;
    let data = tempfile::tempdir().unwrap();
    let on = change(client, &repo.id).await;
    let before = ask(client, &repo.id, &on.commit_id, "agent-0").await;

    let (recorder, shell) = start(&daemon, data.path());
    recorder.until_title("1").await;
    assert_eq!(
        recorder.calls(),
        [],
        "what needed you at launch is not announced"
    );

    let asked = ask(client, &repo.id, &on.commit_id, "agent-1").await;
    let key = format!("help:{}:{}", repo.id, asked.id);
    recorder
        .until("one banner for the new request", |calls| {
            calls.contains(&Call::Post(key.clone()))
        })
        .await;
    recorder.until_title("2").await;
    assert_eq!(recorder.calls(), [Call::Post(key.clone())]);
    drop(shell);

    let (again, shell) = start(&daemon, data.path());
    again.until_title("2").await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(again.calls(), []);

    // A daemon restart under the running shell: it reconnects, lists again,
    // and announces nothing.
    let drawn = again
        .menus
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .len();
    let daemon = daemon.restart().await;
    let client = &daemon.client;
    again
        .until_menus("down, then up again", |menus| {
            let after = &menus[drawn.min(menus.len())..];
            after
                .iter()
                .position(|menu| menu.title.is_empty())
                .is_some_and(|down| after[down..].iter().any(|menu| menu.title == "2"))
        })
        .await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(again.calls(), []);

    client
        .create_comment(CreateCommentRequest {
            repo: repo.id.clone(),
            revision: on.commit_id.clone(),
            kind: CommentKind::Instruction.into(),
            reply_to: before.id.clone(),
            text: "TOML".to_owned(),
            actor: v1::TransitionActor::Human.into(),
            ..CreateCommentRequest::default()
        })
        .await
        .unwrap();
    again.until_title("1").await;
    assert_eq!(again.calls(), []);
    shell.send(Command::WindowClosed);
}
