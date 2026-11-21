//! What one repository's inbox read costs as the inbox grows (#372): the
//! work `WatchInbox` repeats each time a trigger lands in a repository.
//!
//! `#[ignore]`d; run it with
//! `REEF_INBOX_COST=20,200 cargo nextest run --release -p reef-daemon --test inbox_cost --run-ignored all --no-capture`.
//! Each size is that many open proposals waiting on a sign-off plus that
//! many open help requests (at most 256, R3.32's cap). It prints one JSON
//! line per size.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "a measurement; a panic is the failure report"
)]

use std::time::Instant;

use reef_api::v1::{
    self, CommentKind, CreateCommentRequest, CreateIdentityRequest, EditFilesRequest, FileEdit,
    GetInboxRequest, HelpInput, HelpReason, IdentityKind, TransitionChangeRequest,
    edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;

const SIGNOFF: &str = r#"
version = 1

[owners]
identities = ["alice"]

[[signoff]]
name = "review"
identities = ["alice"]
"#;

async fn commit_on(client: &Client, repo: &str, on: &str, files: &[(&str, &str)]) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: files
                .iter()
                .map(|(path, text)| FileEdit {
                    path: (*path).to_owned(),
                    action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
                    executable: None,
                })
                .collect(),
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// Sets bookmark `name` of `repo` to `commit`, outside the daemon.
async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let settings = reef_daemon::testing::test_settings();
        let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
            .create(&root)
            .unwrap()
            .load(
                &settings,
                &default_backend_factories(),
                &default_working_copy_factories(),
            )
            .unwrap();
        let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
        let mut tx = repo.start_transaction();
        tx.repo_mut().set_local_bookmark_target(
            RefName::new(&name),
            RefTarget::normal(CommitId::try_from_hex(&commit).unwrap()),
        );
        pollster::block_on(tx.commit("bookmark set")).unwrap();
    })
    .await
    .unwrap();
}

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
#[ignore = "a measurement: REEF_INBOX_COST=<sizes> and --run-ignored"]
async fn inbox_read_cost() {
    let Ok(sizes) = std::env::var("REEF_INBOX_COST") else {
        return;
    };
    let sizes: Vec<usize> = sizes
        .split(',')
        .map(|n| n.trim().parse().unwrap())
        .collect();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    let client = &daemon.client;
    client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: IdentityKind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[("README.md", "base\n"), ("reef-policy.toml", SIGNOFF)],
    )
    .await;
    bookmark(&repo, "main", &main.commit_id).await;
    let mut made = 0;
    for size in sizes {
        while made < size {
            let path = format!("f{made}.txt");
            let change = commit_on(client, &repo.id, &main.commit_id, &[(&path, "x\n")]).await;
            client
                .transition_change(TransitionChangeRequest {
                    repo: repo.id.clone(),
                    revision: change.commit_id.clone(),
                    to: v1::LifecycleState::Proposed.into(),
                    ..TransitionChangeRequest::default()
                })
                .await
                .unwrap();
            if made < 256 {
                let draft = commit_on(client, &repo.id, &main.commit_id, &[(&path, "y\n")]).await;
                client
                    .create_comment(CreateCommentRequest {
                        repo: repo.id.clone(),
                        revision: draft.commit_id.clone(),
                        kind: CommentKind::HelpRequest.into(),
                        text: "Which format?".to_owned(),
                        help: Some(HelpInput {
                            reason: HelpReason::Decision.into(),
                            tried: "read the docs".to_owned(),
                            question: "TOML or YAML?".to_owned(),
                            ..HelpInput::default()
                        }),
                        client_session: format!("agent-{made}"),
                        ..CreateCommentRequest::default()
                    })
                    .await
                    .unwrap();
            }
            made += 1;
        }
        let request = GetInboxRequest {
            repos: vec![repo.id.clone()],
            ..GetInboxRequest::default()
        };
        let warm = client.get_inbox(request.clone()).await.unwrap();
        let section = &warm.sections[0];
        let mut times = Vec::new();
        for _ in 0..11 {
            let start = Instant::now();
            client.get_inbox(request.clone()).await.unwrap();
            times.push(start.elapsed().as_secs_f64() * 1000.0);
        }
        times.sort_by(f64::total_cmp);
        println!(
            "{{\"proposals\":{size},\"needs_you_now_total\":{},\"watching_total\":{},\"listed\":{},\"median_ms\":{:.1},\"min_ms\":{:.1},\"max_ms\":{:.1}}}",
            section.needs_you_now_total,
            section.watching_total,
            section.needs_you_now.len() + section.watching.len(),
            times[5],
            times[0],
            times[10],
        );
    }
}
