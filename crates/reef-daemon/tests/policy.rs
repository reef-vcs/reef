//! Repository policy through the daemon (R3.07, the brief's tests 11 to
//! 14): `EvaluatePolicy` over real evidence, identities and provenance;
//! where the policy comes from; the input it is given; and the remote
//! listener.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::path::Path;
use std::time::{Duration, Instant, UNIX_EPOCH};

use reef_api::v1::{
    self, AddEvidenceRequest, CreateIdentityRequest, CreateWorkspaceRequest, EditFilesRequest,
    EvaluatePolicyRequest, FileEdit, GetChangeRequest, GetDiffRequest, IdentityKind as Kind,
    PolicyDecision, PolicyRuleStatus, RemoteScope, Workspace, edit_files_request, file_edit,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::session::Actor;
use reef_evidence::{Claim, Outcome, SigningKey, TreeDigest};
use reef_testing::synth::Preset;
use serde_json::Map;

const OWNER: &str = "tester@host";

/// `main`'s policy: CI's `test` on crates, and alice owns the policy.
const POLICY: &str = r#"
version = 1

[owners]
identities = ["alice"]

[attestors.ci]
identities = ["ci-main"]

[[require]]
name = "tests"
paths = ["crates/**"]
kinds = ["test"]
attestors = ["ci"]
"#;

/// A change's weaker policy: nothing required.
const WEAK: &str = r#"
version = 1

[owners]
identities = ["alice"]
"#;

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        remote_on(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn listening(daemon: &TestDaemon) -> SocketAddr {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if let Some(addr) = daemon.handle.state().remote.bound_addr() {
            return addr;
        }
        assert!(Instant::now() < deadline, "the remote listener never bound");
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
}

/// A client through the remote listener with a token of `scope` linked to
/// `identity` (empty for none).
async fn remote_client(
    daemon: &TestDaemon,
    name: &str,
    scope: RemoteScope,
    identity: &str,
) -> Client {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token_for(name, scope, Some(Duration::from_hours(1)), identity)
        .await
        .unwrap();
    let target = RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: Some(fingerprint),
        token: Some(Token::new(&created.secret)),
    };
    Client::connect_remote(&target).await.unwrap()
}

async fn create_identity(client: &Client, name: &str, kind: Kind) {
    client
        .create_identity(CreateIdentityRequest {
            name: name.to_owned(),
            kind: kind.into(),
            ..CreateIdentityRequest::default()
        })
        .await
        .unwrap();
}

async fn workspace(client: &Client, repo: &str, on: &str) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            start: Some(v1::create_workspace_request::Start::NewOn(on.to_owned())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

async fn change_at(client: &Client, repo: &str, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// A new commit on `on` writing each of `files`, outside any workspace.
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

/// Sets a local bookmark from outside the daemon, as `jj bookmark set`
/// would.
fn set_bookmark(root: &Path, name: &str, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::RefTarget;
    use jj_lib::ref_name::RefName;
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
    let repo = pollster::block_on(workspace.repo_loader().load_at_head(None)).unwrap();
    let mut tx = repo.start_transaction();
    tx.repo_mut().set_local_bookmark_target(
        RefName::new(name),
        RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
    );
    pollster::block_on(tx.commit("bookmark set")).unwrap();
}

async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || set_bookmark(&root, &name, &commit))
        .await
        .unwrap();
}

fn claim(kind: &str, result: Outcome) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: kind.to_owned(),
        result,
        target: None,
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: Vec::new(),
        annotations: Map::new(),
    }
}

/// Adds `kind` evidence about `tree` through `client`, signed by `key`.
async fn add(
    client: &Client,
    repo: &str,
    tree: &str,
    kind: &str,
    key: &SigningKey,
) -> v1::Evidence {
    let envelope = claim(kind, Outcome::Pass)
        .sign(&TreeDigest::git(tree).unwrap(), key)
        .unwrap()
        .to_bytes();
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            workspace: String::new(),
            envelope,
            public_key: key.public_key().spki().to_vec(),
            revision: String::new(),
        })
        .await
        .unwrap()
        .evidence
        .unwrap()
}

async fn evaluated(client: &Client, request: EvaluatePolicyRequest) -> v1::PolicyEvaluation {
    client
        .evaluate_policy(request)
        .await
        .unwrap()
        .evaluation
        .unwrap()
}

fn on(repo: &str, revision: &str) -> EvaluatePolicyRequest {
    EvaluatePolicyRequest {
        repo: repo.to_owned(),
        revision: revision.to_owned(),
        include_input: true,
        ..EvaluatePolicyRequest::default()
    }
}

fn rule<'a>(evaluation: &'a v1::PolicyEvaluation, name: &str) -> &'a v1::PolicyRuleResult {
    evaluation
        .rules
        .iter()
        .find(|rule| rule.name == name)
        .unwrap_or_else(|| panic!("no rule {name}: {evaluation:#?}"))
}

fn input(evaluation: &v1::PolicyEvaluation) -> reef_policy::Input {
    serde_json::from_str(&evaluation.input_json).unwrap()
}

/// `main` (the policy), and a change under `crates/` on it.
struct Setup {
    main: v1::Change,
    change: v1::Change,
}

async fn setup(daemon: &TestDaemon, repo: &SyntheticRepo) -> Setup {
    let client = &daemon.client;
    create_identity(client, "ci-main", Kind::Ci).await;
    create_identity(client, "alice", Kind::Person).await;
    let base = change_at(client, &repo.id, "root()").await;
    let base = commit_on(
        client,
        &repo.id,
        &base.commit_id,
        &[("README.md", "base\n")],
    )
    .await;
    let main = commit_on(
        client,
        &repo.id,
        &base.commit_id,
        &[("reef-policy.toml", POLICY)],
    )
    .await;
    bookmark(repo, "main", &main.commit_id).await;
    let change = commit_on(
        client,
        &repo.id,
        &main.commit_id,
        &[("crates/a/src/lib.rs", "pub fn a() {}\n")],
    )
    .await;
    Setup { main, change }
}

// 11.
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture through every attestor case"
)]
async fn evaluate() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo).await;

    // No evidence: blocked, `test` missing from `ci`.
    let none = evaluated(client, on(&repo.id, &change.commit_id)).await;
    assert_eq!(none.decision(), PolicyDecision::Block);
    let tests = rule(&none, "tests");
    assert_eq!(tests.status(), PolicyRuleStatus::Unsatisfied);
    assert_eq!(tests.missing.len(), 1);
    assert_eq!(
        (
            tests.missing[0].kind.as_str(),
            tests.missing[0].target.as_str()
        ),
        ("test", "")
    );
    assert_eq!(tests.missing[0].attestors, ["ci"]);
    assert_eq!(none.tree_id, change.tree_id);

    // Evidence that does not count: the default person identity's agent
    // key, an executor key of that person, and a bare key.
    let agent = client
        .issue_session_key("", Actor::Agent, None, "")
        .await
        .unwrap();
    let person = client
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    let bare = SigningKey::from_seed(&[9; 32]).unwrap();
    for key in [&agent.signing_key, &person.signing_key, &bare] {
        add(client, &repo.id, &change.tree_id, "test", key).await;
    }
    let untrusted = evaluated(client, on(&repo.id, &change.commit_id)).await;
    assert_eq!(
        rule(&untrusted, "tests").status(),
        PolicyRuleStatus::Unsatisfied
    );
    assert_eq!(untrusted.input.unwrap().evidence, 3);
    let seen = input(&untrusted);
    assert_eq!(seen.evidence.len(), 3);
    let statuses: Vec<&str> = seen
        .evidence
        .iter()
        .map(|e| e.key.status.as_str())
        .collect();
    assert!(statuses.contains(&"unbound"), "{statuses:?}");

    // CI's key, through a token linked to `ci-main`: allowed, naming it.
    let ci = remote_client(&daemon, "ci", RemoteScope::Write, "ci-main").await;
    let ci_key = ci
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    let trusted = add(&ci, &repo.id, &change.tree_id, "test", &ci_key.signing_key).await;
    let allowed = evaluated(client, on(&repo.id, &change.commit_id)).await;
    // Trend data, not a gate (docs/benchmarks.md): the median of ten
    // evaluations of a one-file change, socket, warm cache.
    let mut times = Vec::new();
    for _ in 0..10 {
        let started = Instant::now();
        evaluated(client, on(&repo.id, &change.commit_id)).await;
        times.push(started.elapsed());
    }
    times.sort();
    println!("EvaluatePolicy median: {:?}", times[5]);
    assert_eq!(allowed.decision(), PolicyDecision::Allow, "{allowed:#?}");
    assert_eq!(
        rule(&allowed, "tests").evidence_ids,
        std::slice::from_ref(&trusted.evidence_id)
    );
    let ci_seen = input(&allowed)
        .evidence
        .into_iter()
        .find(|e| e.id == trusted.evidence_id)
        .unwrap();
    assert_eq!(ci_seen.key.identity, "ci-main");
    assert_eq!(ci_seen.key.identity_kind, "ci");
    assert_eq!(ci_seen.key.actor, "executor");
    assert_eq!(ci_seen.key.status, "bound");

    // A key revoked before its evidence was witnessed does not count.
    let revoked_change = commit_on(
        client,
        &repo.id,
        &change.commit_id,
        &[("crates/a/b.rs", "b\n")],
    )
    .await;
    let revoked_key = ci
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    daemon
        .client
        .revoke_session_keys(v1::RevokeSessionKeysRequest {
            target: Some(v1::revoke_session_keys_request::Target::KeyId(
                revoked_key.record.key_id.clone(),
            )),
            ..v1::RevokeSessionKeysRequest::default()
        })
        .await
        .unwrap();
    add(
        &ci,
        &repo.id,
        &revoked_change.tree_id,
        "test",
        &revoked_key.signing_key,
    )
    .await;
    let revoked = evaluated(client, on(&repo.id, &revoked_change.commit_id)).await;
    assert_eq!(revoked.decision(), PolicyDecision::Block);
    assert_eq!(input(&revoked).evidence[0].key.status, "revoked");

    // From the change's own workspace and agent: `self`, so the default set
    // drops it.
    let ws = workspace(client, &repo.id, &change.commit_id).await;
    std::fs::write(Path::new(&ws.path).join("crates/a/c.rs"), "c\n").unwrap();
    client.snapshot_workspace("", &ws.id, "").await.unwrap();
    let own = change_at(client, &repo.id, &format!("{}@", ws_name(&ws))).await;
    let agent_label = ws.provenance.clone().unwrap().agent;
    let as_workspace = Client::connect(daemon.handle.socket_path())
        .await
        .unwrap()
        .with_agent(agent_label.clone())
        .unwrap();
    add(
        &as_workspace,
        &repo.id,
        &own.tree_id,
        "test",
        &ci_key.signing_key,
    )
    .await;
    let mine = evaluated(client, on(&repo.id, &own.commit_id)).await;
    let seen = input(&mine);
    assert!(
        !seen.provenance.is_empty(),
        "the workspace recorded its commit"
    );
    assert_eq!(seen.evidence[0].added_by_agent, agent_label);
    assert_eq!(
        mine.decision(),
        PolicyDecision::Block,
        "self-attested: {mine:#?}"
    );

    // 1,100 untrusted objects on the trusted change's tree: the trusted one
    // is still read, and the rest past 1,000 are counted.
    for n in 0..1_100u32 {
        let key = SigningKey::from_seed(&[(n % 200) as u8 + 10; 32]).unwrap();
        let envelope = Claim {
            target: Some(format!("junk-{n}")),
            ..claim("test", Outcome::Pass)
        }
        .sign(&TreeDigest::git(&change.tree_id).unwrap(), &key)
        .unwrap()
        .to_bytes();
        client
            .add_evidence(AddEvidenceRequest {
                repo: repo.id.clone(),
                envelope,
                public_key: key.public_key().spki().to_vec(),
                ..AddEvidenceRequest::default()
            })
            .await
            .unwrap();
    }
    let flooded = evaluated(client, on(&repo.id, &change.commit_id)).await;
    assert_eq!(flooded.decision(), PolicyDecision::Allow);
    assert_eq!(rule(&flooded, "tests").evidence_ids, [trusted.evidence_id]);
    let summary = flooded.input.unwrap();
    assert_eq!(summary.evidence, 1 + 1_000);
    assert_eq!(summary.evidence_ignored, 3 + 1_100 - 1_000);
    daemon.stop().await;
}

fn ws_name(ws: &Workspace) -> String {
    format!("reef-{}", ws.id)
}

// 12.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every source of a policy")]
async fn sources() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { main, change } = setup(&daemon, &repo).await;

    // trunk() by default, with the file's commit, blob and digest.
    let first = evaluated(client, on(&repo.id, &change.commit_id)).await;
    let source = first.source.clone().unwrap();
    assert_eq!(source.revision, "trunk()");
    assert_eq!(source.commit_id, main.commit_id);
    assert_eq!(source.path, "reef-policy.toml");
    assert_eq!(source.blob_id.len(), 40);
    assert_eq!(source.sha256, reef_policy::sha256_hex(POLICY.as_bytes()));
    assert_eq!(source.version, 1);
    assert!(!source.from_request);

    // A second evaluation of the same blob compiles nothing.
    let compiles = daemon.handle.state().policy.compiles();
    evaluated(client, on(&repo.id, &change.commit_id)).await;
    assert_eq!(daemon.handle.state().policy.compiles(), compiles);

    // A change that weakens its own policy is judged by main's, and needs
    // the owners.
    let weaker = commit_on(
        client,
        &repo.id,
        &change.commit_id,
        &[("reef-policy.toml", WEAK), ("crates/a/src/weak.rs", "w\n")],
    )
    .await;
    let judged = evaluated(client, on(&repo.id, &weaker.commit_id)).await;
    assert_eq!(judged.decision(), PolicyDecision::Block);
    assert_eq!(
        rule(&judged, "tests").status(),
        PolicyRuleStatus::Unsatisfied
    );
    let owners = rule(&judged, "builtin:owners");
    assert_eq!(owners.status(), PolicyRuleStatus::Unsatisfied);
    assert_eq!(owners.signoff.as_ref().unwrap().identities, ["alice"]);
    assert_eq!(
        rule(&judged, "builtin:policy-file").status(),
        PolicyRuleStatus::Satisfied
    );
    // alice's sign-off, as a human, satisfies the owners.
    let alice = client
        .issue_session_key("alice", Actor::Human, None, "")
        .await
        .unwrap();
    add(
        client,
        &repo.id,
        &weaker.tree_id,
        "signoff",
        &alice.signing_key,
    )
    .await;
    let signed = evaluated(client, on(&repo.id, &weaker.commit_id)).await;
    let owners = rule(&signed, "builtin:owners");
    assert_eq!(owners.status(), PolicyRuleStatus::Satisfied, "{signed:#?}");
    assert_eq!(owners.signoff.as_ref().unwrap().signed_by, ["alice"]);

    // A broken new policy in a change fails builtin:policy-file.
    let broken = commit_on(
        client,
        &repo.id,
        &change.commit_id,
        &[("reef-policy.toml", "version = 1\n")],
    )
    .await;
    let refused = evaluated(client, on(&repo.id, &broken.commit_id)).await;
    assert_eq!(
        rule(&refused, "builtin:policy-file").status(),
        PolicyRuleStatus::Unsatisfied
    );

    // `policy_revision` and policy text are used when given.
    let at_weaker = evaluated(
        client,
        EvaluatePolicyRequest {
            policy_revision: weaker.commit_id.clone(),
            ..on(&repo.id, &change.commit_id)
        },
    )
    .await;
    assert_eq!(
        at_weaker.source.clone().unwrap().sha256,
        reef_policy::sha256_hex(WEAK.as_bytes())
    );
    assert_eq!(at_weaker.decision(), PolicyDecision::Allow);
    let text = evaluated(
        client,
        EvaluatePolicyRequest {
            policy: WEAK.as_bytes().to_vec(),
            ..on(&repo.id, &change.commit_id)
        },
    )
    .await;
    let source = text.source.clone().unwrap();
    assert!(source.from_request);
    assert_eq!(source.commit_id, "");
    assert_eq!(text.decision(), PolicyDecision::Allow);

    // Both at once, and text over the limit.
    let error = client
        .evaluate_policy(EvaluatePolicyRequest {
            policy: WEAK.as_bytes().to_vec(),
            policy_revision: "trunk()".to_owned(),
            ..on(&repo.id, &change.commit_id)
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::POLICY_SOURCE_CONFLICT);
    let error = client
        .evaluate_policy(EvaluatePolicyRequest {
            policy: vec![b'#'; 257 * 1024],
            ..on(&repo.id, &change.commit_id)
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::POLICY_TOO_LARGE);

    // No policy at the revision; and one that does not load.
    let nothing = evaluated(
        client,
        EvaluatePolicyRequest {
            policy_revision: "root()".to_owned(),
            ..on(&repo.id, &change.commit_id)
        },
    )
    .await;
    assert_eq!(nothing.decision(), PolicyDecision::NoPolicy);
    assert_eq!(nothing.rules, []);
    let invalid = evaluated(
        client,
        EvaluatePolicyRequest {
            policy_revision: broken.commit_id.clone(),
            ..on(&repo.id, &change.commit_id)
        },
    )
    .await;
    assert_eq!(invalid.decision(), PolicyDecision::InvalidPolicy);
    assert!(
        invalid.diagnostics[0].message.contains("no [owners]"),
        "{invalid:#?}"
    );

    // At an operation: the change as it was then.
    let op = first.at_operation.clone();
    let then = evaluated(
        client,
        EvaluatePolicyRequest {
            at_operation: op.clone(),
            ..on(&repo.id, &change.commit_id)
        },
    )
    .await;
    assert_eq!(then.at_operation, op);
    assert_eq!(then.commit_id, change.commit_id);
    daemon.stop().await;
}

// 13.
#[tokio::test(flavor = "multi_thread")]
async fn inputs() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    // A rename, an addition and a large file.
    let renamed = client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::NewOn(change.commit_id.clone())),
            edits: vec![
                FileEdit {
                    path: "crates/a/src/moved.rs".to_owned(),
                    action: Some(file_edit::Action::CopyFrom(
                        "crates/a/src/lib.rs".to_owned(),
                    )),
                    executable: None,
                },
                FileEdit {
                    path: "crates/a/src/lib.rs".to_owned(),
                    action: Some(file_edit::Action::Delete(v1::FileDelete {})),
                    executable: None,
                },
                FileEdit {
                    path: "big.bin".to_owned(),
                    action: Some(file_edit::Action::Content(vec![0; 70_000])),
                    executable: None,
                },
            ],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    let evaluation = evaluated(client, on(&repo.id, &renamed.commit_id)).await;
    let seen = input(&evaluation);
    let diff = client
        .get_diff(GetDiffRequest {
            repo: repo.id.clone(),
            to_revision: renamed.commit_id.clone(),
            format: v1::DiffFormat::Stat.into(),
            ..GetDiffRequest::default()
        })
        .await
        .unwrap();
    let mut from_diff: Vec<String> = diff
        .files
        .iter()
        .flat_map(|file| {
            let change = file.change.clone().unwrap_or_default();
            std::iter::once(change.path.clone())
                .chain((!change.old_path.is_empty()).then_some(change.old_path))
        })
        .collect();
    from_diff.sort();
    from_diff.dedup();
    assert_eq!(seen.change.paths, from_diff);
    assert!(
        seen.change
            .paths
            .contains(&"crates/a/src/lib.rs".to_owned())
    );
    let added: i64 = diff.files.iter().map(|f| i64::from(f.lines_added)).sum();
    assert_eq!(seen.change.lines_added, added);
    let big = seen
        .change
        .files
        .iter()
        .find(|f| f.path == "big.bin")
        .unwrap();
    assert!(big.binary);
    assert_eq!(big.size, 70_000);
    assert_eq!(seen.change.commit, renamed.commit_id);
    assert_eq!(seen.change.tree, renamed.tree_id);

    // The input, read back offline, evaluates the same.
    let offline = reef_policy::Evaluation::of(Some(POLICY.as_bytes()), &seen);
    let statuses = |rules: Vec<(String, i32)>| rules;
    let online: Vec<(String, i32)> = evaluation
        .rules
        .iter()
        .map(|r| (r.name.clone(), r.status))
        .collect();
    let local: Vec<(String, i32)> = offline
        .rules
        .iter()
        .map(|r| {
            let status = match r.status {
                reef_policy::Status::Satisfied => PolicyRuleStatus::Satisfied,
                reef_policy::Status::Unsatisfied => PolicyRuleStatus::Unsatisfied,
                reef_policy::Status::NotApplicable => PolicyRuleStatus::NotApplicable,
                reef_policy::Status::Error => PolicyRuleStatus::Error,
            };
            (r.name.clone(), status as i32)
        })
        .collect();
    assert_eq!(statuses(online), local);
    daemon.stop().await;
}

// 14.
#[tokio::test(flavor = "multi_thread")]
async fn remote() {
    let (daemon, repo) = fixture().await;
    let Setup { change, .. } = setup(&daemon, &repo).await;
    let reader = remote_client(&daemon, "reader", RemoteScope::Read, "").await;
    let evaluation = evaluated(&reader, on(&repo.id, &change.commit_id)).await;
    assert_eq!(evaluation.decision(), PolicyDecision::Block);
    let error: ClientError = reader
        .evaluate_policy(EvaluatePolicyRequest {
            policy: vec![b'#'; 257 * 1024],
            ..on(&repo.id, &change.commit_id)
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::POLICY_TOO_LARGE);
    // A rule that would cost too much returns at once, as its rule's error.
    let list = format!(
        "[{}]",
        (1..=30)
            .map(|n| n.to_string())
            .collect::<Vec<_>>()
            .join(",")
    );
    let expr = format!(
        "{list}.all(a, {list}.all(b, {list}.all(c, {list}.all(d, {list}.all(e, a != e || true)))))"
    );
    let costly = format!("{WEAK}\n[[rule]]\nname = \"costly\"\nexpr = \"{expr}\"\n");
    let started = Instant::now();
    let refused = evaluated(
        &reader,
        EvaluatePolicyRequest {
            policy: costly.into_bytes(),
            ..on(&repo.id, &change.commit_id)
        },
    )
    .await;
    assert!(started.elapsed() < Duration::from_secs(10));
    let rule = rule(&refused, "costly");
    assert_eq!(rule.status(), PolicyRuleStatus::Error, "{refused:#?}");
    assert!(rule.message.contains("cost"), "{}", rule.message);
    daemon.stop().await;
}
