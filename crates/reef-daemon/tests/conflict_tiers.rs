//! Resolution tiers and attempt caps through the daemon (R3.28, ADR 0082):
//! a `reef-policy.toml` on `main` gives each conflict a tier and a cap; a
//! conflict that may not be attempted waits for a person, who allows it;
//! a resolution is listed on its proposal and, unless its tier is `land`,
//! lands only with a person's sign-off. Conflicts are made as R3.27's tests
//! make them: two workspaces write one file, and the second change is
//! rebased onto the first outside the daemon.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use jj_lib::backend::CommitId;
use jj_lib::object_id::ObjectId as _;
use jj_lib::repo::{ReadonlyRepo, Repo as _};
use reef_api::v1::{
    self, AddEvidenceRequest, AssignConflictRequest, ChangeStep, ClaimConflictRequest,
    ConflictAssignment, ConflictAssignmentReason as Reason, ConflictAssignmentState as State,
    ConflictTier, CreateIdentityRequest, CreateWorkspaceRequest, EditFilesRequest,
    EvaluatePolicyRequest, GetConflictRequest, GetWorkspaceRequest, IdentityKind as Kind,
    ListConflictAssignmentsRequest, PolicyRuleStatus as RuleStatus, SignedEvidence,
    TransitionActor, Workspace, edit_files_request, file_edit,
};
use reef_daemon::client::Client;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome as Verdict, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;

const OWNERS: &str = "version = 1\n[owners]\nidentities = [\"alice\"]\n";

async fn fixture(conflicts: &str) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    daemon
        .handle
        .state()
        .assignments
        .set_grace(Duration::from_hours(1));
    policy_on_main(&daemon.client, &repo, &format!("{OWNERS}{conflicts}")).await;
    daemon
        .client
        .create_identity(CreateIdentityRequest {
            name: "alice".to_owned(),
            kind: Kind::Person.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
    (daemon, repo)
}

/// `main` moved to a new commit on `main` (or the root) with `policy`.
async fn policy_on_main(client: &Client, repo: &SyntheticRepo, policy: &str) {
    let on = if bookmark_exists(client, &repo.id).await {
        "main"
    } else {
        "root()"
    };
    let main = client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: vec![v1::FileEdit {
                path: "reef-policy.toml".to_owned(),
                action: Some(file_edit::Action::Content(policy.as_bytes().to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    bookmark(repo, "main", &main.commit_id).await;
}

async fn bookmark_exists(client: &Client, repo: &str) -> bool {
    client
        .get_change(v1::GetChangeRequest {
            repo: repo.to_owned(),
            revision: "main".to_owned(),
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .is_ok()
}

/// The repository at `root`, loaded at its head with jj-lib.
fn load(root: &Path) -> std::sync::Arc<ReadonlyRepo> {
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    let settings = test_settings();
    let workspace = jj_lib::default_backend_factories::default_workspace_loader_factory()
        .create(root)
        .unwrap()
        .load(
            &settings,
            &default_backend_factories(),
            &default_working_copy_factories(),
        )
        .unwrap();
    pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap()
}

async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || {
        use jj_lib::op_store::RefTarget;
        use jj_lib::ref_name::RefName;
        let repo = load(&root);
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

/// `commit` rebased onto `onto` outside the daemon; the new commit's id.
fn rebase(root: &Path, commit: &str, onto: &str) -> String {
    let repo = load(root);
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let old = pollster::block_on(store.get_commit_async(&CommitId::try_from_hex(commit).unwrap()))
        .unwrap();
    let new = pollster::block_on(jj_lib::rewrite::rebase_commit(
        tx.repo_mut(),
        old.clone(),
        vec![CommitId::try_from_hex(onto).unwrap()],
    ))
    .unwrap();
    tx.repo_mut()
        .set_rewritten_commit(old.id().clone(), new.id().clone());
    pollster::block_on(tx.repo_mut().rebase_descendants()).unwrap();
    pollster::block_on(tx.commit("rebase")).unwrap();
    new.id().hex()
}

async fn create(client: &Client, repo: &str, owner: &str, change: Option<&str>) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: owner.to_owned(),
            start: change.map(|c| v1::create_workspace_request::Start::Change(c.to_owned())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn write(client: &Client, ws: &Workspace, file: &str, text: &str) -> v1::Change {
    let path = Path::new(&ws.path).join(file);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(path, text).unwrap();
    let snapshot = client.snapshot_workspace("", &ws.id, "").await.unwrap();
    assert!(snapshot.changed);
    client
        .get_change(v1::GetChangeRequest {
            repo: ws.repo_id.clone(),
            revision: snapshot.commit_id,
            skip_snapshot: true,
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

struct Scene {
    b: Workspace,
    /// The conflicted commit.
    rebased: String,
}

/// Workspaces `a` and `b` write `file`; `b`'s change is rebased onto
/// `a`'s while `b` is live, so `b` owns the conflict.
async fn scene(daemon: &TestDaemon, repo: &SyntheticRepo, file: &str) -> Scene {
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a@test", None).await;
    let b = create(client, &repo.id, "agent-b@test", None).await;
    let a_commit = write(client, &a, file, "one\nfrom a\nthree\n")
        .await
        .commit_id;
    let b_commit = write(client, &b, file, "one\nfrom b\nthree\n")
        .await
        .commit_id;
    let rebased = rebase(&repo.root, &b_commit, &a_commit);
    Scene { b, rebased }
}

async fn the_one(client: &Client, repo: &str) -> ConflictAssignment {
    let open = client
        .list_conflict_assignments(
            ListConflictAssignmentsRequest {
                repo: repo.to_owned(),
                ..ListConflictAssignmentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(open.len(), 1, "{open:#?}");
    open.into_iter().next().unwrap()
}

async fn step(client: &Client, ws: &Workspace) -> ChangeStep {
    client
        .get_workspace(GetWorkspaceRequest {
            workspace: ws.id.clone(),
            ..GetWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
        .step()
}

fn assign(repo: &str, assignment: &str, actor: TransitionActor) -> AssignConflictRequest {
    AssignConflictRequest {
        repo: repo.to_owned(),
        assignment: assignment.to_owned(),
        target: Some(v1::assign_conflict_request::Target::Allow(true)),
        actor: actor.into(),
        ..AssignConflictRequest::default()
    }
}

/// `builtin:resolution`'s status for `revision`.
async fn resolution_rule(client: &Client, repo: &str, revision: &str) -> RuleStatus {
    let evaluation = client
        .evaluate_policy(EvaluatePolicyRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            ..EvaluatePolicyRequest::default()
        })
        .await
        .unwrap()
        .evaluation
        .unwrap();
    evaluation
        .rules
        .iter()
        .find(|rule| rule.name == "builtin:resolution")
        .unwrap_or_else(|| panic!("no builtin:resolution in {evaluation:#?}"))
        .status()
}

/// A person's (`who`'s, actor `actor`) sign-off on `tree`.
async fn sign_off(client: &Client, repo: &str, who: &str, actor: Actor, tree: &str) {
    let key = client
        .issue_session_key(who, actor, None, "")
        .await
        .unwrap()
        .signing_key;
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    let started = UNIX_EPOCH + Duration::from_secs(secs);
    let claim = Claim {
        kind: "signoff".to_owned(),
        result: Verdict::Pass,
        target: None,
        started_on: started,
        finished_on: started + Duration::from_secs(1),
        artifacts: Vec::new(),
        annotations: Map::new(),
    };
    let evidence = SignedEvidence {
        envelope: claim
            .sign(&TreeDigest::git(tree).unwrap(), &key)
            .unwrap()
            .to_bytes(),
        public_key: key.public_key().spki().to_vec(),
    };
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            envelope: evidence.envelope,
            public_key: evidence.public_key,
            ..AddEvidenceRequest::default()
        })
        .await
        .unwrap();
}

// Test 4: an `ask` conflict waits for a person; an agent can neither claim
// nor allow it; a person's go-ahead hands it to its owner as attempt 1.
#[tokio::test(flavor = "multi_thread")]
async fn ask_blocks_until_a_person_allows() {
    let (daemon, repo) = fixture("[conflicts]\ntier = \"ask\"\n").await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo, "conflict.txt").await;
    let blocked = the_one(client, &repo.id).await;
    assert_eq!(
        (blocked.state(), blocked.reason()),
        (State::Blocked, Reason::Ask)
    );
    assert_eq!(blocked.tier.as_ref().unwrap().tier(), ConflictTier::Ask);
    assert_eq!((blocked.attempts, blocked.attempt_limit), (0, 3));
    assert_eq!(
        blocked.owner.as_ref().unwrap().workspace_id,
        s.b.id,
        "kept for the go-ahead"
    );
    assert_eq!(step(client, &s.b).await, ChangeStep::Ask);

    let resolver = create(client, &repo.id, "resolver@test", None).await;
    let refused = client
        .claim_conflict(ClaimConflictRequest {
            repo: repo.id.clone(),
            workspace: resolver.id.clone(),
            assignment: blocked.id.clone(),
            ..ClaimConflictRequest::default()
        })
        .await
        .unwrap_err();
    assert_eq!(
        refused.reason(),
        reef_api::error::ASSIGNMENT_BLOCKED,
        "{refused}"
    );
    let refused = client
        .assign_conflict(assign(&repo.id, &blocked.id, TransitionActor::Agent))
        .await
        .unwrap_err();
    assert_eq!(
        refused.reason(),
        reef_api::error::ALLOW_NEEDS_PERSON,
        "{refused}"
    );

    let allowed = client
        .assign_conflict(assign(&repo.id, &blocked.id, TransitionActor::Human))
        .await
        .unwrap()
        .assignment
        .unwrap();
    assert_eq!(
        (allowed.state(), allowed.reason(), allowed.attempts),
        (State::Assigned, Reason::Allowed, 1)
    );
    assert_eq!(allowed.owner.as_ref().unwrap().workspace_id, s.b.id);
    assert!(!allowed.go_ahead_identity_id.is_empty() || allowed.go_ahead_at.is_some());
    assert_eq!(step(client, &s.b).await, ChangeStep::Resolve);
    let _ = s.rebased;
    daemon.stop().await;
}

// Tests 8 to 13: the input names the conflict's paths and verified sides;
// a rule by path decides; a `land` resolution lands with nothing more, an
// `approve` one needs a person's sign-off (an agent's does not count), and
// the resolution is listed on its proposal with its conflicted commit.
#[tokio::test(flavor = "multi_thread")]
async fn resolutions_follow_their_tier() {
    let (daemon, repo) = fixture(
        "[conflicts]\ntier = \"approve\"\nattempts = 2\n\
         [[conflicts.rule]]\npaths = [\"docs/**\"]\ntier = \"land\"\nattempts = 5\n",
    )
    .await;
    let client = &daemon.client;

    // The rule for docs: land, 5 attempts.
    let docs = scene(&daemon, &repo, "docs/a.md").await;
    let assignment = the_one(client, &repo.id).await;
    let tier = assignment.tier.clone().unwrap();
    assert_eq!(
        (tier.tier(), tier.attempts, tier.rule, tier.error.as_str()),
        (ConflictTier::Land, 5, Some(0), "")
    );
    assert_eq!(tier.policy_sha256.len(), 64, "{tier:?}");
    let got = client
        .get_conflict(GetConflictRequest {
            repo: repo.id.clone(),
            revision: docs.rebased.clone(),
            policy_input: true,
            skip_snapshot: true,
            ..GetConflictRequest::default()
        })
        .await
        .unwrap();
    let input: serde_json::Value = serde_json::from_str(&got.policy_input).unwrap();
    assert_eq!(input["conflict"]["paths"], serde_json::json!(["docs/a.md"]));
    let sides = input["conflict"]["sides"].as_array().unwrap();
    assert_eq!(sides.len(), 2, "{input}");
    assert!(sides.iter().all(|side| side["actor"] == "agent"), "{input}");
    let fixer = create(client, &repo.id, "agent-b@test", Some(&docs.rebased)).await;
    let resolved = write(client, &fixer, "docs/a.md", "one\nmerged\nthree\n").await;
    assert_eq!(
        resolution_rule(client, &repo.id, &resolved.commit_id).await,
        RuleStatus::NotApplicable,
        "a land resolution needs nothing more"
    );
    for ws in [&docs.b, &fixer] {
        client
            .release_workspace(&repo.id, &ws.id, false, false)
            .await
            .unwrap();
    }
    let open = client
        .list_conflict_assignments(
            ListConflictAssignmentsRequest {
                repo: repo.id.clone(),
                ..ListConflictAssignmentsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(open, [], "the docs conflict closed");

    // The default: approve. Judged right after the resolving snapshot,
    // before the follower has looked, it is still a resolution (S-234).
    let src = scene(&daemon, &repo, "src.rs").await;
    let assignment = the_one(client, &repo.id).await;
    assert_eq!(
        assignment.tier.as_ref().unwrap().tier(),
        ConflictTier::Approve
    );
    let fixer = create(client, &repo.id, "agent-b@test", Some(&src.rebased)).await;
    let resolved = write(client, &fixer, "src.rs", "one\nmerged\nthree\n").await;
    assert_eq!(
        resolution_rule(client, &repo.id, &resolved.commit_id).await,
        RuleStatus::Unsatisfied
    );
    sign_off(client, &repo.id, "alice", Actor::Agent, &resolved.tree_id).await;
    assert_eq!(
        resolution_rule(client, &repo.id, &resolved.commit_id).await,
        RuleStatus::Unsatisfied,
        "an agent's sign-off does not approve"
    );
    sign_off(client, &repo.id, "alice", Actor::Human, &resolved.tree_id).await;
    assert_eq!(
        resolution_rule(client, &repo.id, &resolved.commit_id).await,
        RuleStatus::Satisfied
    );

    // Proposed, it lists the resolution, whose conflicted commit still
    // reads both sides.
    client
        .transition_change(v1::TransitionChangeRequest {
            repo: repo.id.clone(),
            revision: resolved.commit_id.clone(),
            to: v1::LifecycleState::Proposed.into(),
            ..v1::TransitionChangeRequest::default()
        })
        .await
        .unwrap();
    let proposals = client
        .list_proposals(
            v1::ListProposalsRequest {
                repo: repo.id.clone(),
                ..v1::ListProposalsRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    let entry = proposals
        .iter()
        .find(|entry| entry.change_id == resolved.change_id)
        .unwrap();
    let [resolution] = entry.resolutions.as_slice() else {
        panic!("{entry:#?}");
    };
    assert_eq!(resolution.tier(), ConflictTier::Approve);
    assert!(resolution.approval);
    assert_eq!(resolution.resolved_actor, "agent");
    let sides = client
        .get_conflict(GetConflictRequest {
            repo: repo.id.clone(),
            revision: resolution.conflicted_commit_id.clone(),
            skip_snapshot: true,
            ..GetConflictRequest::default()
        })
        .await
        .unwrap()
        .conflict
        .unwrap();
    assert_eq!(sides.files[0].path, "src.rs");
    daemon.stop().await;
}

// Test 7: a policy change on `main` changes an open conflict's tier at the
// next look without taking it from its owner; the next hand-off obeys it.
// An invalid policy is `ask`, with the error.
#[tokio::test(flavor = "multi_thread")]
async fn policy_changes_refresh_the_tier() {
    let (daemon, repo) = fixture("[conflicts]\ntier = \"land\"\n").await;
    let client = &daemon.client;
    let s = scene(&daemon, &repo, "conflict.txt").await;
    let open = the_one(client, &repo.id).await;
    assert_eq!(open.tier.as_ref().unwrap().tier(), ConflictTier::Land);
    assert_eq!(open.state(), State::Assigned);

    policy_on_main(
        client,
        &repo,
        &format!("{OWNERS}[conflicts]\ntier = \"ask\"\n"),
    )
    .await;
    let refreshed = the_one(client, &repo.id).await;
    assert_eq!(refreshed.tier.as_ref().unwrap().tier(), ConflictTier::Ask);
    assert_eq!(refreshed.state(), State::Assigned, "its attempt goes on");

    client
        .release_conflict(v1::ReleaseConflictRequest {
            repo: repo.id.clone(),
            workspace: s.b.id.clone(),
            reason: "handing back".to_owned(),
            ..v1::ReleaseConflictRequest::default()
        })
        .await
        .unwrap();
    let released = the_one(client, &repo.id).await;
    assert_eq!(
        (released.state(), released.reason()),
        (State::Blocked, Reason::Ask)
    );

    policy_on_main(client, &repo, "version = 1\n[conflicts]\ntier = \"nope\"\n").await;
    let invalid = the_one(client, &repo.id).await;
    let tier = invalid.tier.unwrap();
    assert_eq!(tier.tier(), ConflictTier::Ask);
    assert!(tier.error.contains("invalid"), "{tier:?}");
    daemon.stop().await;
}

/// Two workspaces write `files` from one base; the second change is
/// rebased onto the first; the conflicted commit.
async fn conflict_on(daemon: &TestDaemon, repo: &SyntheticRepo, files: &[&str]) -> String {
    let client = &daemon.client;
    let a = create(client, &repo.id, "agent-a@test", None).await;
    let b = create(client, &repo.id, "agent-b@test", None).await;
    let mut a_commit = String::new();
    let mut b_commit = String::new();
    for file in files {
        a_commit = write(client, &a, file, "one\nfrom a\nthree\n")
            .await
            .commit_id;
        b_commit = write(client, &b, file, "one\nfrom b\nthree\n")
            .await
            .commit_id;
    }
    rebase(&repo.root, &b_commit, &a_commit)
}

// Review finding 1 (S-230): resolving the strict paths first narrows the
// conflict to a `land` rule; the tier stays the strictest it had, so the
// resolution still needs a person's sign-off.
#[tokio::test(flavor = "multi_thread")]
async fn a_partial_resolution_keeps_the_strictest_tier() {
    let (daemon, repo) = fixture(
        "[conflicts]\ntier = \"land\"\n[[conflicts.rule]]\npaths = [\"infra/**\"]\ntier = \"approve\"\n",
    )
    .await;
    let client = &daemon.client;
    let rebased = conflict_on(&daemon, &repo, &["infra/a.txt", "docs/b.txt"]).await;
    let open = the_one(client, &repo.id).await;
    assert_eq!(open.tier.as_ref().unwrap().tier(), ConflictTier::Approve);
    let fixer = create(client, &repo.id, "agent-b@test", Some(&rebased)).await;
    write(client, &fixer, "infra/a.txt", "one\nmerged\nthree\n").await;
    let partial = the_one(client, &repo.id).await;
    assert_eq!(partial.files_total, 1);
    assert_eq!(partial.tier.as_ref().unwrap().tier(), ConflictTier::Approve);
    let resolved = write(client, &fixer, "docs/b.txt", "one\nmerged\nthree\n").await;
    assert!(!resolved.conflict);
    assert_eq!(
        resolution_rule(client, &repo.id, &resolved.commit_id).await,
        RuleStatus::Unsatisfied
    );
    daemon.stop().await;
}

// Review finding 2 and test 11 (S-230): an agent that edits an `ask`
// conflict anyway cannot unblock it by narrowing it, and its resolution,
// made while blocked, needs a person's sign-off.
#[tokio::test(flavor = "multi_thread")]
async fn ignoring_an_ask_block_needs_a_signoff() {
    let (daemon, repo) = fixture(
        "[conflicts]\ntier = \"land\"\n[[conflicts.rule]]\npaths = [\"infra/**\"]\ntier = \"ask\"\n",
    )
    .await;
    let client = &daemon.client;
    let rebased = conflict_on(&daemon, &repo, &["infra/a.txt", "docs/b.txt"]).await;
    let open = the_one(client, &repo.id).await;
    assert_eq!((open.state(), open.reason()), (State::Blocked, Reason::Ask));
    let fixer = create(client, &repo.id, "agent-b@test", Some(&rebased)).await;
    write(client, &fixer, "infra/a.txt", "one\nmerged\nthree\n").await;
    let partial = the_one(client, &repo.id).await;
    assert_eq!(
        (partial.state(), partial.tier.as_ref().unwrap().tier()),
        (State::Blocked, ConflictTier::Ask),
        "still waits for a person"
    );
    let resolved = write(client, &fixer, "docs/b.txt", "one\nmerged\nthree\n").await;
    assert_eq!(
        resolution_rule(client, &repo.id, &resolved.commit_id).await,
        RuleStatus::Unsatisfied
    );
    daemon.stop().await;
}
