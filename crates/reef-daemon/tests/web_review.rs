//! Reviewing from the web UI (R3.31, ADR 0086): the web listener's rows for
//! declines, comments, instructions' confirms and landings, their rules, and
//! the web UI session each write records where a remote token would be.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use hyper_util::client::legacy::Client as HttpClient;
use hyper_util::client::legacy::connect::HttpConnector;
use hyper_util::rt::TokioExecutor;
use reef_api::v1::change_service_client::ChangeServiceClient;
use reef_api::v1::comment_service_client::CommentServiceClient;
use reef_api::v1::landing_service_client::LandingServiceClient;
use reef_api::v1::{
    self, CommentKind, CreateCommentRequest, EditFilesRequest, FileEdit, InstructionState,
    LifecycleState as State, ListTransitionsRequest, TransitionActor, TransitionChangeRequest,
    UpdateInstructionRequest, edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_testing::synth::Preset;
use tonic::body::Body;
use tonic::codegen::http::Uri;
use tonic::service::Interceptor;
use tonic::service::interceptor::InterceptedService;
use tonic::{Request, Status};
use tonic_web::{GrpcWebClientLayer, GrpcWebClientService};
use tower::Layer as _;

/// Adds `authorization: Bearer <token>`.
#[derive(Clone)]
struct Bearer(String, Option<String>);

impl Interceptor for Bearer {
    fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
        request.metadata_mut().insert(
            "authorization",
            format!("Bearer {}", self.0).parse().unwrap(),
        );
        if let Some(agent) = &self.1 {
            request
                .metadata_mut()
                .insert("reef-agent", agent.parse().unwrap());
        }
        Ok(request)
    }
}

type Web = InterceptedService<
    GrpcWebClientService<HttpClient<HttpConnector, tonic_web::GrpcWebCall<Body>>>,
    Bearer,
>;

/// A page's connection: a web UI session's origin and token.
#[derive(Clone)]
struct Page {
    origin: Uri,
    token: String,
    /// A `reef-agent` the page's calls send, which a web call never records.
    agent: Option<String>,
}

impl Page {
    async fn open(client: &Client) -> Page {
        let session = client.create_web_session(false, "").await.unwrap();
        let (_, token) = session.session_url.split_once("#session=").unwrap();
        Page {
            origin: session.url.parse().unwrap(),
            token: token.to_owned(),
            agent: None,
        }
    }

    fn service(&self) -> Web {
        let client = HttpClient::builder(TokioExecutor::new()).build_http();
        InterceptedService::new(
            GrpcWebClientLayer::new().layer(client),
            Bearer(self.token.clone(), self.agent.clone()),
        )
    }

    /// The session as the daemon records it: the first 16 hex digits of
    /// its token's digest.
    fn session_id(&self) -> String {
        blake3::hash(self.token.as_bytes()).to_hex()[..16].to_owned()
    }

    fn inbox(&self) -> v1::inbox_service_client::InboxServiceClient<Web> {
        v1::inbox_service_client::InboxServiceClient::with_origin(
            self.service(),
            self.origin.clone(),
        )
    }

    fn changes(&self) -> ChangeServiceClient<Web> {
        ChangeServiceClient::with_origin(self.service(), self.origin.clone())
    }

    fn comments(&self) -> CommentServiceClient<Web> {
        CommentServiceClient::with_origin(self.service(), self.origin.clone())
    }

    fn landing(&self) -> LandingServiceClient<Web> {
        LandingServiceClient::with_origin(self.service(), self.origin.clone())
    }
}

fn reason(status: &Status) -> String {
    reef_daemon::error::detail_of(status)
        .map(|detail| detail.reason)
        .unwrap_or_default()
}

fn rule(status: &Status) -> String {
    reef_daemon::error::detail_of(status)
        .and_then(|detail| detail.metadata.get("rule").cloned())
        .unwrap_or_default()
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

/// A new commit on `on` writing `path`, outside any workspace.
async fn commit_on(client: &Client, repo: &str, on: &str, path: &str) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: vec![FileEdit {
                path: path.to_owned(),
                action: Some(file_edit::Action::Content(b"text\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// Sets the local bookmark `main` from outside the daemon, as `jj bookmark
/// set` would.
fn set_main(root: &std::path::Path, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories, default_workspace_loader_factory,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
    let workspace = default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &test_settings(),
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new("main"),
        RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

/// `main` on a base commit, and `count` changes on it, each proposed by an
/// agent over the socket.
async fn proposals(daemon: &TestDaemon, repo: &SyntheticRepo, count: usize) -> Vec<v1::Change> {
    let client = &daemon.client;
    let main = commit_on(client, &repo.id, "root()", "README.md").await;
    let (root, commit) = (repo.root.clone(), main.commit_id.clone());
    tokio::task::spawn_blocking(move || set_main(&root, &commit))
        .await
        .unwrap();
    let mut proposed = Vec::new();
    for n in 0..count {
        let change = commit_on(client, &repo.id, &main.commit_id, &format!("{n}.txt")).await;
        let response = client
            .transition_change(TransitionChangeRequest {
                repo: repo.id.clone(),
                revision: change.commit_id.clone(),
                to: State::Proposed.into(),
                reason: "ready".to_owned(),
                actor: TransitionActor::Agent.into(),
                ..TransitionChangeRequest::default()
            })
            .await
            .unwrap();
        proposed.push(response.change.unwrap());
    }
    proposed
}

// A page declines only, at the commit it showed; the decline, a comment and
// an instruction's confirm each record the page's session; a landing from
// the page needs a person's touch.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one scenario, in the page's order")]
async fn web_decline_and_comment_audited() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let changes = proposals(&daemon, &repo, 2).await;
    let (declined, landed) = (&changes[0], &changes[1]);
    let page = Page::open(client).await;
    let decline = |commit: &str| TransitionChangeRequest {
        repo: repo.id.clone(),
        revision: declined.commit_id.clone(),
        to: State::Declined.into(),
        reason: "the tests do not cover the new branch".to_owned(),
        expected_commit_id: commit.to_owned(),
        expected_state: State::Proposed.into(),
        actor: TransitionActor::Human.into(),
        ..TransitionChangeRequest::default()
    };

    // The rule: a decline, at a named commit.
    let unnamed = page
        .changes()
        .transition_change(decline(""))
        .await
        .unwrap_err();
    assert_eq!(reason(&unnamed), "WEB_ACCESS_DENIED", "{unnamed:?}");
    assert_eq!(rule(&unnamed), "decline");
    let withdraw = page
        .changes()
        .transition_change(TransitionChangeRequest {
            to: State::Withdrawn.into(),
            ..decline(&declined.commit_id)
        })
        .await
        .unwrap_err();
    assert_eq!(rule(&withdraw), "decline", "{withdraw:?}");
    let stale = page
        .changes()
        .transition_change(decline(&landed.commit_id))
        .await
        .unwrap_err();
    assert_ne!(reason(&stale), "WEB_ACCESS_DENIED", "{stale:?}");

    let response = page
        .changes()
        .transition_change(decline(&declined.commit_id))
        .await
        .unwrap()
        .into_inner();
    let transition = response.transition.unwrap();
    assert_eq!(transition.to(), State::Declined);
    assert_eq!(transition.actor(), TransitionActor::Human);
    let session = transition
        .web
        .clone()
        .expect("the decline names its session");
    assert_eq!(session.session.len(), 16);
    assert!(transition.remote.is_none());
    assert_eq!(response.operation.unwrap().web.as_ref(), Some(&session));
    // The record outlives the operation: it is the transition's row.
    let listed = client
        .list_transitions(
            ListTransitionsRequest {
                repo: repo.id.clone(),
                changes: vec![declined.change_id.clone()],
                ..ListTransitionsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    // Newest first.
    assert_eq!(listed.first().unwrap().web.as_ref(), Some(&session));
    assert_eq!(
        listed.last().unwrap().web,
        None,
        "the agent's proposal came over the socket"
    );

    // A note and an instruction, confirmed from the page.
    let comment = |kind: CommentKind, text: &str| CreateCommentRequest {
        repo: repo.id.clone(),
        revision: landed.commit_id.clone(),
        kind: kind.into(),
        text: text.to_owned(),
        actor: TransitionActor::Human.into(),
        ..CreateCommentRequest::default()
    };
    // The rule: a person's comment, unconfirmed, as no workspace's session.
    for refused in [
        CreateCommentRequest {
            confirm: true,
            ..comment(CommentKind::Instruction, "go")
        },
        CreateCommentRequest {
            actor: TransitionActor::Agent.into(),
            ..comment(CommentKind::Note, "as an agent")
        },
        CreateCommentRequest {
            client_session: "s-1".to_owned(),
            ..comment(CommentKind::Note, "as a session")
        },
        comment(CommentKind::HelpRequest, "help"),
    ] {
        let status = page.comments().create_comment(refused).await.unwrap_err();
        assert_eq!(rule(&status), "comment", "{status:?}");
    }
    let note = page
        .comments()
        .create_comment(comment(CommentKind::Note, "looks right"))
        .await
        .unwrap()
        .into_inner();
    let author = note.comment.unwrap().author.unwrap();
    assert_eq!(author.web.as_ref(), Some(&session));
    assert_eq!(author.actor(), TransitionActor::Human);
    assert_eq!(note.operation.unwrap().web.as_ref(), Some(&session));
    let instruction = page
        .comments()
        .create_comment(comment(CommentKind::Instruction, "add a test for 1.txt"))
        .await
        .unwrap()
        .into_inner()
        .comment
        .unwrap();
    assert_eq!(
        instruction.instruction.as_ref().unwrap().state(),
        InstructionState::AwaitingConfirmation
    );
    let confirmed = page
        .comments()
        .update_instruction(UpdateInstructionRequest {
            repo: repo.id.clone(),
            id: instruction.id.clone(),
            to: InstructionState::Pending.into(),
            expected_state: InstructionState::AwaitingConfirmation.into(),
            ..UpdateInstructionRequest::default()
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(confirmed.operation.unwrap().web.as_ref(), Some(&session));
    // A claimed instruction goes back to the queue only by its claimer.
    let claimed = client
        .claim_instruction(v1::ClaimInstructionRequest {
            repo: repo.id.clone(),
            comment_id: instruction.id.clone(),
            change: landed.change_id.clone(),
            take: true,
            ..v1::ClaimInstructionRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        claimed.comment.unwrap().instruction.unwrap().state(),
        InstructionState::Delivered
    );
    let release = page
        .comments()
        .update_instruction(UpdateInstructionRequest {
            repo: repo.id.clone(),
            id: instruction.id.clone(),
            to: InstructionState::Pending.into(),
            expected_state: InstructionState::Delivered.into(),
            ..UpdateInstructionRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(rule(&release), "confirm", "{release:?}");
    let socket_note = client
        .create_comment(comment(CommentKind::Note, "from the CLI"))
        .await
        .unwrap();
    assert_eq!(socket_note.comment.unwrap().author.unwrap().web, None);

    // A landing asked for from the page needs a person's touch on the
    // tree of a passkey the destination pins, so a destination with no
    // policy takes none, and its first policy is committed by hand
    // (`passkeys::a_web_landing_needs_a_touch` lands one with Accept's
    // sign-off); the socket's needs none.
    let land = v1::LandChangeRequest {
        repo: repo.id.clone(),
        revision: landed.change_id.clone(),
        expected_commit_id: landed.commit_id.clone(),
        actor: TransitionActor::Human.into(),
        ..v1::LandChangeRequest::default()
    };
    let untouched = page.landing().land_change(land.clone()).await.unwrap_err();
    let detail = reef_daemon::error::detail_of(&untouched).unwrap();
    assert_eq!(
        (
            detail.reason.as_str(),
            detail.metadata["missing"].as_str(),
            detail.metadata["policy"].as_str(),
        ),
        ("VERIFIED_SIGNOFF_REQUIRED", "pin", "no_policy"),
        "{untouched:?}"
    );
    assert_eq!(detail.next, ["reef", "policy", "init"]);
    assert!(!detail.hint.contains("`reef land "), "{}", detail.hint);
    let landing = client
        .land_change(land)
        .await
        .unwrap()
        .landing
        .expect("the socket's request is judged");
    assert_eq!(landing.web, None);
}

/// Rewrites `change`'s commit with another file, as the agent amending it
/// does; the new commit.
async fn amend(client: &Client, repo: &str, commit: &str) -> String {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::Revision(commit.to_owned())),
            edits: vec![FileEdit {
                path: "amended.txt".to_owned(),
                action: Some(file_edit::Action::Content(b"more\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
        .commit_id
}

// R3.31 review round 2: the page names the change and the commit it showed,
// so an amend between its read and the click refuses the decline; a page
// names no workspace, and a `reef-agent` it sends is not recorded.
#[tokio::test(flavor = "multi_thread")]
async fn a_web_decline_holds_to_what_the_page_showed() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let changes = proposals(&daemon, &repo, 1).await;
    let shown = &changes[0];
    let page = Page::open(client).await;
    let decline = TransitionChangeRequest {
        repo: repo.id.clone(),
        revision: shown.change_id.clone(),
        to: State::Declined.into(),
        reason: "no".to_owned(),
        expected_commit_id: shown.commit_id.clone(),
        expected_state: State::Proposed.into(),
        ..TransitionChangeRequest::default()
    };
    let named = page
        .changes()
        .transition_change(TransitionChangeRequest {
            workspace: "6ce393d29a64e06c".to_owned(),
            ..decline.clone()
        })
        .await
        .unwrap_err();
    assert_eq!(rule(&named), "decline", "{named:?}");
    let amended = amend(client, &repo.id, &shown.commit_id).await;
    assert_ne!(amended, shown.commit_id);
    let stale = page
        .changes()
        .transition_change(decline.clone())
        .await
        .unwrap_err();
    assert_eq!(reason(&stale), "EXPECTED_COMMIT_MISMATCH", "{stale:?}");

    let agent = Page {
        agent: Some("planner-1".to_owned()),
        ..page.clone()
    };
    let declined = agent
        .changes()
        .transition_change(TransitionChangeRequest {
            expected_commit_id: amended,
            ..decline
        })
        .await
        .unwrap()
        .into_inner()
        .transition
        .unwrap();
    assert_eq!(declined.actor(), TransitionActor::Human);
    assert_eq!(declined.agent, "", "a web call records no agent name");
    assert!(declined.web.is_some());
}

// R3.31 review round 3: through the web, a reply skips the confirm only as
// the first answer to an open help request; once it is answered, a reply
// waits for its estimate and Confirm like any instruction.
#[tokio::test(flavor = "multi_thread")]
async fn a_web_answer_skips_the_confirm_only_while_the_question_is_open() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let changes = proposals(&daemon, &repo, 1).await;
    let shown = &changes[0];
    let asked = client
        .create_comment(CreateCommentRequest {
            repo: repo.id.clone(),
            revision: shown.commit_id.clone(),
            kind: CommentKind::HelpRequest.into(),
            text: "Which config format?".to_owned(),
            help: Some(v1::HelpInput {
                reason: v1::HelpReason::Decision.into(),
                tried: "read the docs".to_owned(),
                question: "TOML or YAML?".to_owned(),
                conflict: String::new(),
                while_blocked: false,
            }),
            client_session: "helper".to_owned(),
            ..CreateCommentRequest::default()
        })
        .await
        .unwrap()
        .comment
        .unwrap();
    let page = Page::open(client).await;
    let reply = |text: &str| CreateCommentRequest {
        repo: repo.id.clone(),
        revision: shown.change_id.clone(),
        expected_commit_id: shown.commit_id.clone(),
        kind: CommentKind::Instruction.into(),
        reply_to: asked.id.clone(),
        text: text.to_owned(),
        actor: TransitionActor::Human.into(),
        ..CreateCommentRequest::default()
    };
    let mut states = Vec::new();
    for text in ["TOML", "and also delete the YAML files"] {
        let written = page
            .comments()
            .create_comment(reply(text))
            .await
            .unwrap()
            .into_inner()
            .comment
            .unwrap();
        states.push(written.instruction.unwrap().state());
    }
    assert_eq!(
        states,
        [
            InstructionState::Pending,
            InstructionState::AwaitingConfirmation
        ]
    );
}

// R3.33 (S-261): through the web UI a mark is the default person's, moved
// forward only and capped at the newest transition; the request has
// nothing else to set.
#[tokio::test(flavor = "multi_thread")]
async fn a_web_mark_is_the_default_persons() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let page = Page::open(client).await;
    let unread = client
        .get_inbox(v1::GetInboxRequest {
            repos: vec![repo.id.clone()],
            ..v1::GetInboxRequest::default()
        })
        .await
        .unwrap();
    assert!(!unread.sections[0].landed.as_ref().unwrap().from_mark);
    let stands = page
        .inbox()
        .mark_inbox_seen(v1::MarkInboxSeenRequest {
            marks: vec![v1::InboxMark {
                repo: repo.id.clone(),
                seq: 1_000,
            }],
        })
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        stands.marks[0].seq, 0,
        "capped at the newest transition: none"
    );
    let read = client
        .get_inbox(v1::GetInboxRequest {
            repos: vec![repo.id.clone()],
            ..v1::GetInboxRequest::default()
        })
        .await
        .unwrap();
    assert!(
        read.sections[0].landed.as_ref().unwrap().from_mark,
        "the socket reads the mark the page set: both are the default person's"
    );

    // S-253: the row names the session that moved the mark, and only a move
    // changes it.
    let state = daemon.handle.state();
    let person = state.identity.default_id().unwrap().to_owned();
    let id = repo.id.parse().unwrap();
    let row = || {
        state
            .db
            .inbox_marks(&person, &[id])
            .unwrap()
            .remove(&id)
            .unwrap()
    };
    assert_eq!(
        row().web_session,
        Some(page.session_id()),
        "a web mark records its session"
    );
    proposals(&daemon, &repo, 1).await;
    let mark = |seq| v1::MarkInboxSeenRequest {
        marks: vec![v1::InboxMark {
            repo: repo.id.clone(),
            seq,
        }],
    };
    let through = client.mark_inbox_seen(mark(1_000)).await.unwrap().marks[0].seq;
    assert!(through > 0);
    assert_eq!(row().web_session, None, "the socket moved it last");
    page.inbox().mark_inbox_seen(mark(through)).await.unwrap();
    assert_eq!(
        row().web_session,
        None,
        "a web mark that moves nothing records nothing"
    );
}
