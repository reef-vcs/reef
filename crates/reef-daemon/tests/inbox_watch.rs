//! `WatchInbox` (#372, ADR 0091): the inbox, then every change to it by
//! key, read once for every stream and only when something that can change
//! it is recorded. Each test folds the frames as a client does and checks
//! the fold against `GetInbox`.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use reef_api::v1::{
    self, CommentKind, CreateCommentRequest, CreateIdentityRequest, EditFilesRequest, FileEdit,
    GetInboxRequest, HelpInput, HelpReason, IdentityKind, InboxBucket, InboxListing, InboxMark,
    LandChangeRequest, MarkInboxSeenRequest, TransitionChangeRequest, WatchInboxRequest,
    WatchInboxResponse, edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots};
use reef_testing::synth::Preset;

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

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

/// Rewrites `commit`'s description outside the daemon, as `jj describe` at
/// the root does: an `EXTERNAL` operation.
async fn describe_outside(repo: &SyntheticRepo, commit: &str, text: &str) {
    let (root, commit, text) = (repo.root.clone(), commit.to_owned(), text.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::backend::CommitId;
        use jj_lib::default_backend_factories::{
            default_backend_factories, default_working_copy_factories,
        };
        use jj_lib::repo::Repo as _;
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
        let old = repo
            .store()
            .get_commit(&CommitId::try_from_hex(&commit).unwrap())
            .unwrap();
        let mut tx = repo.start_transaction();
        pollster::block_on(
            tx.repo_mut()
                .rewrite_commit(&old)
                .set_description(text)
                .write(),
        )
        .unwrap();
        pollster::block_on(tx.repo_mut().rebase_descendants()).unwrap();
        pollster::block_on(tx.commit("describe from another client")).unwrap();
    })
    .await
    .unwrap();
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

const PERMISSIVE: &str = r#"
version = 1

[owners]
identities = ["alice"]

[landing]
checks = "proposal"
"#;

/// `main` with a permissive policy; alice exists.
async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo) -> v1::Change {
    let client = &daemon.client;
    let _ = client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: IdentityKind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await;
    let main = commit_on(
        client,
        &repo.id,
        "root()",
        &[("README.md", "base\n"), ("reef-policy.toml", PERMISSIVE)],
    )
    .await;
    bookmark(repo, "main", &main.commit_id).await;
    main
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

async fn propose(client: &Client, repo: &str, change: &v1::Change) {
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.to_owned(),
            revision: change.commit_id.clone(),
            to: v1::LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
}

/// One repository as a client holds it.
#[derive(Debug, Default, Clone)]
struct Held {
    items: BTreeMap<String, v1::InboxItem>,
    needs_you_now_total: u32,
    watching_total: u32,
    landed: Option<v1::LandedDigest>,
    error: Option<v1::InboxError>,
    listed: bool,
    raised_through: u64,
}

/// A stream and the client's fold of it.
struct Watched {
    stream: tonic::Streaming<WatchInboxResponse>,
    repos: BTreeMap<String, Held>,
    synced: bool,
    frames: usize,
    /// The last frame's raise ledger.
    epoch: String,
}

impl Watched {
    async fn open(client: &Client, repos: &[&str]) -> Watched {
        let stream = client
            .watch_inbox(WatchInboxRequest {
                repos: repos.iter().map(|repo| (*repo).to_owned()).collect(),
            })
            .await
            .unwrap();
        let mut watched = Watched {
            stream,
            repos: BTreeMap::new(),
            synced: false,
            frames: 0,
            epoch: String::new(),
        };
        while !watched.synced {
            watched.next().await;
        }
        watched
    }

    fn apply(&mut self, frame: WatchInboxResponse) {
        self.frames += 1;
        self.synced = frame.synced;
        assert!(
            !frame.raise_epoch.is_empty(),
            "every frame names the ledger"
        );
        self.epoch = frame.raise_epoch;
        for section in frame.sections {
            if section.removed {
                let held = self.repos.remove(&section.repo_id).unwrap_or_default();
                let mut left = section.left.clone();
                left.sort();
                let keys: Vec<String> = held.items.into_keys().collect();
                assert_eq!(left, keys, "a removed section lists its items in `left`");
                continue;
            }
            let held = self.repos.entry(section.repo_id.clone()).or_default();
            let listing = section.listing();
            assert!(
                listing == InboxListing::Unspecified || !held.listed,
                "a repository lists once per stream: {section:?}"
            );
            held.listed |= listing == InboxListing::End;
            for key in &section.left {
                held.items.remove(key);
            }
            for item in section.items {
                held.items.insert(item.key.clone(), item);
            }
            held.needs_you_now_total = section.needs_you_now_total;
            held.watching_total = section.watching_total;
            held.landed = section.landed;
            held.error = section.error;
            if section.raised_through > 0 {
                for item in held.items.values() {
                    if item.bucket() == InboxBucket::NeedsYouNow {
                        // 0 for a proposal not judged yet that has no number.
                        let provisional = matches!(
                            &item.record,
                            Some(v1::inbox_item::Record::Proposal(entry))
                                if entry.automatic.as_ref().is_none_or(
                                    |status| status.state() == v1::AutomaticState::Pending
                                )
                        );
                        assert!(
                            (item.raised > 0 || provisional)
                                && item.raised <= section.raised_through,
                            "{item:?} against {}",
                            section.raised_through
                        );
                    } else {
                        assert_eq!(item.raised, 0, "{item:?}");
                    }
                }
                assert!(
                    section.raised_through >= held.raised_through,
                    "raises only grow"
                );
                held.raised_through = section.raised_through;
            }
        }
    }

    async fn next(&mut self) {
        let frame = tokio::time::timeout(Duration::from_secs(30), self.stream.message())
            .await
            .expect("a frame within 30 s")
            .unwrap()
            .expect("the stream is open");
        self.apply(frame);
    }

    /// Reads frames until `found` holds of the fold.
    async fn until(&mut self, what: &str, found: impl Fn(&Watched) -> bool) {
        let deadline = Instant::now() + Duration::from_secs(30);
        while !found(self) {
            assert!(Instant::now() < deadline, "{what}: {:#?}", self.repos);
            self.next().await;
        }
    }

    fn item(&self, key: &str) -> Option<&v1::InboxItem> {
        self.repos.values().find_map(|held| held.items.get(key))
    }

    /// What a person sees, what `GetInbox` must agree with: each item by
    /// key, and each repository's totals and digest count.
    fn seen(&self) -> Seen {
        let items = self
            .repos
            .values()
            .flat_map(|held| held.items.values())
            .map(|item| {
                (
                    item.key.clone(),
                    (item.bucket, item.reason, item.headline.clone()),
                )
            })
            .collect();
        let sections = self
            .repos
            .iter()
            .map(|(repo, held)| {
                let landed = held.landed.as_ref().map_or(0, |landed| landed.count);
                (
                    repo.clone(),
                    (held.needs_you_now_total, held.watching_total, landed),
                )
            })
            .collect();
        (items, sections)
    }
}

type Seen = (
    BTreeMap<String, (i32, i32, String)>,
    BTreeMap<String, (u32, u32, u64)>,
);

/// `GetInbox` the way [`Watched::seen`] shows it, checking each item's
/// bucket is the list it is in.
async fn read(client: &Client) -> Seen {
    let response = client.get_inbox(GetInboxRequest::default()).await.unwrap();
    let mut seen = BTreeMap::new();
    let mut sections = BTreeMap::new();
    for section in response.sections {
        let landed = section.landed.as_ref().map_or(0, |landed| landed.count);
        sections.insert(
            section.repo_id.clone(),
            (section.needs_you_now_total, section.watching_total, landed),
        );
        for (list, bucket) in [
            (section.needs_you_now, InboxBucket::NeedsYouNow),
            (section.watching, InboxBucket::Watching),
        ] {
            for item in list {
                assert_eq!(item.bucket(), bucket, "{item:?}");
                seen.insert(item.key.clone(), (item.bucket, item.reason, item.headline));
            }
        }
    }
    (seen, sections)
}

/// Reads frames until the fold agrees with `GetInbox`.
async fn agrees(watched: &mut Watched, client: &Client, what: &str) {
    let deadline = Instant::now() + Duration::from_secs(30);
    loop {
        let wanted = read(client).await;
        if watched.seen() == wanted {
            return;
        }
        assert!(
            Instant::now() < deadline,
            "{what}: the stream holds {:#?}, GetInbox says {wanted:#?}",
            watched.seen()
        );
        if let Ok(frame) =
            tokio::time::timeout(Duration::from_millis(500), watched.stream.message()).await
        {
            watched.apply(frame.unwrap().expect("the stream is open"));
        }
    }
}

#[tokio::test(flavor = "multi_thread")]
async fn the_first_inbox_then_each_change_by_key() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo).await;
    let draft = commit_on(client, &repo.id, &main.commit_id, &[("b.txt", "b\n")]).await;
    let asked = ask(client, &repo.id, &draft.commit_id, "agent-a").await;

    let mut watched = Watched::open(client, &[]).await;
    let help = format!("help:{}:{}", repo.id, asked.id);
    let first = watched
        .item(&help)
        .expect("the first inbox lists the help request");
    assert_eq!(first.bucket(), InboxBucket::NeedsYouNow);
    assert_eq!(
        watched.seen(),
        read(client).await,
        "the first inbox is GetInbox's"
    );

    // A proposal enters watching.
    let change = commit_on(client, &repo.id, &main.commit_id, &[("a.txt", "a\n")]).await;
    propose(client, &repo.id, &change).await;
    let proposal = format!("proposal:{}:{}", repo.id, change.change_id);
    watched
        .until("the proposal enters", |w| w.item(&proposal).is_some())
        .await;
    assert_eq!(
        watched.item(&proposal).unwrap().bucket(),
        InboxBucket::Watching
    );
    agrees(&mut watched, client, "after the proposal").await;

    // The help request is withdrawn: its key leaves.
    client
        .withdraw_help_request(v1::WithdrawHelpRequestRequest {
            repo: repo.id.clone(),
            id: asked.id.clone(),
            ..v1::WithdrawHelpRequestRequest::default()
        })
        .await
        .unwrap();
    watched
        .until("the help request leaves", |w| w.item(&help).is_none())
        .await;
    agrees(&mut watched, client, "after the withdraw").await;

    // Another client's describe of the proposed change (EXTERNAL) reaches it.
    describe_outside(&repo, &change.commit_id, "Describe it from the root\n").await;
    watched
        .until("the outside describe reaches the proposal", |w| {
            w.item(&proposal).is_some_and(|item| match &item.record {
                Some(v1::inbox_item::Record::Proposal(entry)) => entry
                    .change
                    .as_ref()
                    .is_some_and(|change| change.description.starts_with("Describe it")),
                _ => false,
            })
        })
        .await;

    // Landing it: the proposal leaves and the digest counts it.
    // The describe amended the proposal: propose its new commit, then land.
    let amended = proposal_commit(client, &repo.id, &change.change_id).await;
    client
        .transition_change(TransitionChangeRequest {
            repo: repo.id.clone(),
            revision: amended.clone(),
            to: v1::LifecycleState::Proposed.into(),
            ..TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    client
        .land_change(LandChangeRequest {
            repo: repo.id.clone(),
            revision: amended,
            wait_ms: 60_000,
            ..LandChangeRequest::default()
        })
        .await
        .unwrap();
    watched
        .until("the landing is counted", |w| {
            w.item(&proposal).is_none()
                && w.repos[&repo.id]
                    .landed
                    .as_ref()
                    .is_some_and(|landed| landed.count == 1)
        })
        .await;
    let landed = watched.repos[&repo.id].landed.clone().unwrap();
    assert_eq!(landed.newest[0].subject, "Describe it from the root");

    // The mark, moved by another client, empties the digest live.
    client
        .mark_inbox_seen(MarkInboxSeenRequest {
            marks: vec![InboxMark {
                repo: repo.id.clone(),
                seq: landed.through_seq,
            }],
        })
        .await
        .unwrap();
    watched
        .until("the mark reaches the digest", |w| {
            w.repos[&repo.id]
                .landed
                .as_ref()
                .is_some_and(|landed| landed.count == 0 && landed.from_mark)
        })
        .await;
}

async fn proposal_commit(client: &Client, repo: &str, change: &str) -> String {
    let entries = client
        .list_proposals_page(v1::ListProposalsRequest {
            repo: repo.to_owned(),
            changes: vec![change.to_owned()],
            ..v1::ListProposalsRequest::default()
        })
        .await
        .unwrap()
        .entries;
    entries[0].change.as_ref().unwrap().commit_id.clone()
}

#[tokio::test(flavor = "multi_thread")]
async fn a_repository_registered_later_joins_and_one_forgotten_leaves() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let mut every = Watched::open(client, &[]).await;
    let mut one = Watched::open(client, &[&repo.id]).await;

    let other = daemon.tmp.path().join("other");
    let at = other.clone();
    tokio::task::spawn_blocking(move || {
        reef_daemon::testing::init_synthetic_repo(&at, Preset::Small);
    })
    .await
    .unwrap();
    let second = client
        .register_repository(&other, v1::IfNotJj::Fail)
        .await
        .unwrap()
        .repository
        .unwrap()
        .id;
    // #367 review F1: the first help request in a repository registered
    // after the page opened shows without a reload.
    let base = commit_on(client, &second, "root()", &[("README.md", "other\n")]).await;
    let asked = ask(client, &second, &base.commit_id, "agent-b").await;
    let help = format!("help:{second}:{}", asked.id);
    every
        .until("the new repository's help request", |w| {
            w.item(&help).is_some()
        })
        .await;

    client.forget_repository(&second).await.unwrap();
    every
        .until("the forgotten repository is removed", |w| {
            !w.repos.contains_key(&second)
        })
        .await;
    assert!(every.item(&help).is_none());

    // The stream that named one repository never got the other.
    assert!(!one.repos.contains_key(&second), "{:#?}", one.repos);
    let draft = commit_on(client, &repo.id, "root()", &[("c.txt", "c\n")]).await;
    let mine = ask(client, &repo.id, &draft.commit_id, "agent-c").await;
    let key = format!("help:{}:{}", repo.id, mine.id);
    one.until("its own repository's help", |w| w.item(&key).is_some())
        .await;
    assert_eq!(one.repos.keys().collect::<Vec<_>>(), [&repo.id]);
}

#[tokio::test(flavor = "multi_thread")]
async fn snapshots_alone_read_nothing_and_streams_share_a_read() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let state = daemon.handle.state();
    let main = setup(&daemon, &repo).await;
    let mut first = Watched::open(client, &[]).await;
    let mut second = Watched::open(client, &[]).await;
    tokio::time::sleep(Duration::from_millis(600)).await;
    let passes = state.inbox.reads();

    // Edits to a change nothing proposes record operations the inbox does
    // not read on.
    let workspace = client
        .create_workspace(v1::CreateWorkspaceRequest {
            repo: repo.id.clone(),
            owner: "claude@watch".to_owned(),
            ..v1::CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    // The create's own read, paced, settles first.
    let mut created = state.inbox.reads();
    loop {
        tokio::time::sleep(Duration::from_millis(1_000)).await;
        let now = state.inbox.reads();
        if now == created {
            break;
        }
        created = now;
    }
    for n in 0..20 {
        std::fs::write(
            std::path::Path::new(&workspace.path).join(format!("f{n}.txt")),
            "x\n",
        )
        .unwrap();
        client
            .snapshot_workspace(&repo.id, &workspace.id, "")
            .await
            .unwrap();
    }
    // Longer than a paced read's spacing under load.
    tokio::time::sleep(Duration::from_secs(2)).await;
    assert_eq!(
        state.inbox.reads(),
        created,
        "snapshots read nothing (a create read once: {passes} -> {created})"
    );

    // One help request: both streams see it after one read.
    let draft = commit_on(client, &repo.id, &main.commit_id, &[("d.txt", "d\n")]).await;
    let before = state.inbox.reads();
    let asked = ask(client, &repo.id, &draft.commit_id, "agent-d").await;
    let key = format!("help:{}:{}", repo.id, asked.id);
    first
        .until("the first stream", |w| w.item(&key).is_some())
        .await;
    second
        .until("the second stream", |w| w.item(&key).is_some())
        .await;
    assert!(
        state.inbox.reads() - before <= 2,
        "{} passes for one comment and two streams",
        state.inbox.reads() - before
    );

    // Attention moves with no operation; its presence entry reaches the
    // stream, as it comes and as it goes.
    let id: reef_daemon::model::WorkspaceId = workspace.id.parse().unwrap();
    let key = format!("workspace:{}:{}", repo.id, workspace.id);
    state
        .db
        .set_workspace_attention(id, Some(reef_daemon::model::Attention::WatchFailed), None)
        .unwrap();
    first
        .until("attention enters", |w| w.item(&key).is_some())
        .await;
    assert_eq!(first.item(&key).unwrap().bucket(), InboxBucket::NeedsYouNow);
    state.db.set_workspace_attention(id, None, None).unwrap();
    first
        .until("attention leaves", |w| w.item(&key).is_none())
        .await;
}

#[tokio::test(flavor = "multi_thread")]
async fn a_slow_reader_gets_the_latest_state_not_the_history() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let main = setup(&daemon, &repo).await;
    let draft = commit_on(client, &repo.id, &main.commit_id, &[("d.txt", "d\n")]).await;
    let mut watched = Watched::open(client, &[]).await;
    let frames = watched.frames;
    let passes = daemon.handle.state().inbox.reads();
    let storm = Instant::now();

    // While the client reads nothing, help requests come and go.
    let mut last = None;
    for n in 0..200 {
        let asked = ask(client, &repo.id, &draft.commit_id, &format!("agent-{n}")).await;
        client
            .withdraw_help_request(v1::WithdrawHelpRequestRequest {
                repo: repo.id.clone(),
                id: asked.id.clone(),
                ..v1::WithdrawHelpRequestRequest::default()
            })
            .await
            .unwrap();
        last = Some(asked);
    }
    let kept = ask(client, &repo.id, &draft.commit_id, "agent-kept").await;
    let key = format!("help:{}:{}", repo.id, kept.id);
    tokio::time::sleep(Duration::from_secs(2)).await;
    watched
        .until("the kept request", |w| w.item(&key).is_some())
        .await;
    let withdrawn = format!("help:{}:{}", repo.id, last.unwrap().id);
    assert!(watched.item(&withdrawn).is_none());
    // At most one frame per read of the board, which the pacing folds 401
    // changes into: never a frame per change. (Holding the stream to one
    // built frame while its transport is blocked is
    // `session::tests::a_client_that_stops_reading_gets_the_latest_board`.)
    let read = daemon.handle.state().inbox.reads() - passes;
    let elapsed = storm.elapsed();
    let sent = u64::try_from(watched.frames - frames).unwrap();
    assert!(sent <= read + 1, "{sent} frames for {read} reads");
    // Reads of one repository start at most every 250 ms, however many
    // changes land meanwhile.
    let spaced = u64::try_from(elapsed.as_millis() / 250).unwrap() + 2;
    assert!(
        read <= spaced,
        "{read} reads for 401 changes in {elapsed:?}"
    );
}

#[tokio::test(flavor = "multi_thread")]
async fn a_daemon_with_no_repository_sends_one_empty_synced_frame() {
    // At one slot a stream still opens: the board's own subscription is
    // taken past the cap.
    let daemon = TestDaemon::start_with(|config| config.subscribe.max_subscriptions = 1).await;
    let mut stream = daemon
        .client
        .watch_inbox(WatchInboxRequest::default())
        .await
        .unwrap();
    let frame = tokio::time::timeout(Duration::from_secs(30), stream.message())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(frame.synced);
    assert_eq!(frame.sections, Vec::new());
    let refused = daemon
        .client
        .watch_inbox(WatchInboxRequest::default())
        .await
        .unwrap_err();
    assert_eq!(refused.reason(), reef_api::error::TOO_MANY_SUBSCRIPTIONS);
}

/// Reads frames until `key` is listed, and how long that took.
async fn latency(watched: &mut Watched, what: &str, key: &str) -> Duration {
    let asked = Instant::now();
    watched.until(what, |w| w.item(key).is_some()).await;
    asked.elapsed()
}

/// A second repository, registered.
async fn second(daemon: &TestDaemon) -> String {
    let other = daemon.tmp.path().join("other");
    let at = other.clone();
    tokio::task::spawn_blocking(move || {
        reef_daemon::testing::init_synthetic_repo(&at, Preset::Small);
    })
    .await
    .unwrap();
    daemon
        .client
        .register_repository(&other, v1::IfNotJj::Fail)
        .await
        .unwrap()
        .repository
        .unwrap()
        .id
}

/// Holds `repo`'s thread for `stall`, as a long git import holds it.
fn hold(daemon: &TestDaemon, repo: &str, stall: Duration) -> tokio::task::JoinHandle<()> {
    let held = daemon
        .handle
        .state()
        .repos
        .get(repo.parse().unwrap())
        .unwrap()
        .call(move |_| std::thread::sleep(stall));
    tokio::spawn(async move { held.await.unwrap() })
}

const STALL: Duration = Duration::from_secs(5);

#[tokio::test(flavor = "multi_thread")]
async fn a_busy_repository_holds_back_only_itself() {
    let (daemon, a) = fixture().await;
    let client = &daemon.client;
    let b = second(&daemon).await;
    let on_a = commit_on(client, &a.id, "root()", &[("a.txt", "a\n")]).await;
    let on_b = commit_on(client, &b, "root()", &[("b.txt", "b\n")]).await;
    let mut watched = Watched::open(client, &[]).await;

    let asked = ask(client, &b, &on_b.commit_id, "agent-b0").await;
    let control = latency(&mut watched, "control", &format!("help:{b}:{}", asked.id)).await;

    // A's thread is held, as a long git import holds it, right after a
    // help request there.
    let asked = ask(client, &a.id, &on_a.commit_id, "agent-a").await;
    let stalled = Instant::now();
    let held = hold(&daemon, &a.id, STALL);
    let a_key = format!("help:{}:{}", a.id, asked.id);

    let asked = ask(client, &b, &on_b.commit_id, "agent-b1").await;
    let b_during = latency(
        &mut watched,
        "B while A is busy",
        &format!("help:{b}:{}", asked.id),
    )
    .await;
    watched
        .until("A's help request", |w| w.item(&a_key).is_some())
        .await;
    let a_latency = stalled.elapsed();
    held.await.unwrap();

    let asked = ask(client, &b, &on_b.commit_id, "agent-b2").await;
    let b_after = latency(
        &mut watched,
        "B after A was busy",
        &format!("help:{b}:{}", asked.id),
    )
    .await;
    let asked = ask(client, &a.id, &on_a.commit_id, "agent-a2").await;
    let a_after = latency(
        &mut watched,
        "A after it was busy",
        &format!("help:{}:{}", a.id, asked.id),
    )
    .await;

    let report = format!(
        "control={control:?} b_during={b_during:?} a_latency={a_latency:?} b_after={b_after:?} a_after={a_after:?}"
    );
    eprintln!("{report}");
    // Waiting on A's thread is not work: neither repository's next read
    // is spaced by it, and B's reads never wait for A's.
    let quick = Duration::from_secs(2);
    assert!(b_during < quick, "{report}");
    assert!(a_latency < STALL + quick, "{report}");
    assert!(b_after < quick, "{report}");
    assert!(a_after < quick, "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn a_board_started_while_a_repository_is_busy_sends_the_others_first() {
    let (daemon, a) = fixture().await;
    let client = &daemon.client;
    let b = second(&daemon).await;
    let on_b = commit_on(client, &b, "root()", &[("b.txt", "b\n")]).await;
    ask(client, &b, &on_b.commit_id, "agent-b").await;

    // No stream is open, so the board starts with A's thread held: a
    // stream naming B is whole at once, and one of everything gets B
    // before A and `synced` once A is read.
    let stalled = Instant::now();
    let held = hold(&daemon, &a.id, STALL);
    let mut every = client
        .watch_inbox(WatchInboxRequest::default())
        .await
        .unwrap();
    let named = Watched::open(client, &[&b]).await;
    let named_first = stalled.elapsed();
    let mut b_first = None;
    loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), every.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        if b_first.is_none() && frame.sections.iter().any(|section| section.repo_id == b) {
            assert!(!frame.synced, "B came before A was read");
            b_first = Some(stalled.elapsed());
        }
        if frame.synced {
            break;
        }
    }
    let every_synced = stalled.elapsed();
    held.await.unwrap();
    assert!(named.repos.contains_key(&b), "{:#?}", named.repos);
    let report =
        format!("named_first={named_first:?} b_first={b_first:?} every_synced={every_synced:?}");
    eprintln!("{report}");
    let quick = Duration::from_secs(2);
    assert!(named_first < quick, "{report}");
    assert!(b_first.is_some_and(|at| at < quick), "{report}");
    assert!(every_synced + quick >= STALL, "{report}");
}

#[tokio::test(flavor = "multi_thread")]
async fn an_item_that_arrives_before_synced_is_raised_above_its_listing() {
    let (daemon, a) = fixture().await;
    let client = &daemon.client;
    let b = second(&daemon).await;
    let on_b = commit_on(client, &b, "root()", &[("b.txt", "b\n")]).await;
    let before = ask(client, &b, &on_b.commit_id, "agent-b0").await;
    let before_key = format!("help:{b}:{}", before.id);

    let held = hold(&daemon, &a.id, STALL);
    let mut stream = client
        .watch_inbox(WatchInboxRequest::default())
        .await
        .unwrap();
    let mut listed_through = None;
    let mut epoch = String::new();
    while listed_through.is_none() {
        let frame = tokio::time::timeout(Duration::from_secs(30), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert!(
            !frame.raise_epoch.is_empty(),
            "every frame names the ledger"
        );
        epoch.clone_from(&frame.raise_epoch);
        for section in frame.sections.iter().filter(|section| section.repo_id == b) {
            if section.listing() == InboxListing::End {
                assert!(!frame.synced, "B lists while A is held");
                let listed = section
                    .items
                    .iter()
                    .find(|item| item.key == before_key)
                    .unwrap();
                assert!(listed.raised > 0 && listed.raised <= section.raised_through);
                listed_through = Some(section.raised_through);
            }
        }
    }
    let listed_through = listed_through.unwrap();

    let after = ask(client, &b, &on_b.commit_id, "agent-b1").await;
    let after_key = format!("help:{b}:{}", after.id);
    let arrived = loop {
        let frame = tokio::time::timeout(Duration::from_secs(30), stream.message())
            .await
            .unwrap()
            .unwrap()
            .unwrap();
        assert_eq!(frame.raise_epoch, epoch);
        let found = frame
            .sections
            .iter()
            .filter(|section| section.repo_id == b)
            .find_map(|section| {
                let item = section.items.iter().find(|item| item.key == after_key)?;
                Some((section.listing(), item.raised, frame.synced))
            });
        if let Some(found) = found {
            break found;
        }
    };
    let (listing, raised, synced) = arrived;
    assert!(!synced, "the #389 case: it arrived before A was read");
    assert_eq!(
        listing,
        InboxListing::Unspecified,
        "a change, not B's listing"
    );
    assert!(
        raised > listed_through,
        "news by number: {raised} > {listed_through}"
    );
    held.await.unwrap();

    // GetInbox numbers the same items the same way.
    let response = client.get_inbox(GetInboxRequest::default()).await.unwrap();
    let section = response
        .sections
        .iter()
        .find(|section| section.repo_id == b)
        .unwrap();
    let numbered: BTreeMap<&str, u64> = section
        .needs_you_now
        .iter()
        .map(|item| (item.key.as_str(), item.raised))
        .collect();
    assert_eq!(numbered[after_key.as_str()], raised);
}

#[tokio::test(flavor = "multi_thread")]
async fn raises_survive_a_daemon_restart() {
    let (daemon, a) = fixture().await;
    let client = &daemon.client;
    let on_a = commit_on(client, &a.id, "root()", &[("a.txt", "a\n")]).await;
    let asked = ask(client, &a.id, &on_a.commit_id, "agent-a").await;
    let key = format!("help:{}:{}", a.id, asked.id);
    let mut watched = Watched::open(client, &[]).await;
    watched
        .until("the help request", |w| w.item(&key).is_some())
        .await;
    let before = watched.item(&key).unwrap().raised;
    let epoch = watched.epoch.clone();
    drop(watched);

    let daemon = daemon.restart().await;
    let client = &daemon.client;
    let mut watched = Watched::open(client, &[]).await;
    watched
        .until("the help request again", |w| w.item(&key).is_some())
        .await;
    assert_eq!(watched.item(&key).unwrap().raised, before);
    assert_eq!(watched.epoch, epoch, "a restart keeps the ledger");
    assert!(watched.repos[&a.id].listed);
}
