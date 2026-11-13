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
    add_outcome(client, repo, tree, kind, Outcome::Pass, key).await
}

/// [`add`] with `result`.
async fn add_outcome(
    client: &Client,
    repo: &str,
    tree: &str,
    kind: &str,
    result: Outcome,
    key: &SigningKey,
) -> v1::Evidence {
    let envelope = claim(kind, result)
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

// R3.08: simulation (the brief's tests 1 to 9).

/// The proposed policy: `main`'s, and no keys and no large changes.
const PROPOSED: &str = r#"
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

[limits]
max_lines = 4000

[[forbid]]
name = "secrets"
paths = ["secrets/**"]

[[rule]]
name = "described"
expr = 'change.description.size() >= 0'
"#;

/// A merge of `parents` with their merged tree, outside the daemon.
fn merge_commit(root: &Path, parents: &[String]) -> String {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::repo::Repo as _;
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
    let ids: Vec<CommitId> = parents
        .iter()
        .map(|id| CommitId::try_from_hex(id).unwrap())
        .collect();
    let commits: Vec<_> = ids
        .iter()
        .map(|id| repo.store().get_commit(id).unwrap())
        .collect();
    let tree =
        pollster::block_on(jj_lib::rewrite::merge_commit_trees(repo.as_ref(), &commits)).unwrap();
    let mut tx = repo.start_transaction();
    let merge = pollster::block_on(
        tx.repo_mut()
            .new_commit(ids, tree)
            .set_description("merge the branch")
            .write(),
    )
    .unwrap();
    pollster::block_on(tx.commit("merge")).unwrap();
    merge.id().hex()
}

/// `main`'s history, oldest first, and the merge at its head.
struct History {
    /// base, policy, crates, secrets, big, tested, left, right.
    commits: Vec<v1::Change>,
    merge: String,
}

impl History {
    fn commit(&self, at: usize) -> &str {
        &self.commits[at].commit_id
    }
}

async fn history(daemon: &TestDaemon, repo: &SyntheticRepo) -> History {
    let client = &daemon.client;
    create_identity(client, "ci-main", Kind::Ci).await;
    create_identity(client, "alice", Kind::Person).await;
    let root = change_at(client, &repo.id, "root()").await;
    let mut commits = Vec::new();
    let big = "line\n".repeat(5_000);
    let steps: [&[(&str, &str)]; 5] = [
        &[("README.md", "base\n")],
        &[("reef-policy.toml", POLICY)],
        &[("crates/a/src/lib.rs", "pub fn a() {}\n")],
        &[("secrets/key.pem", "-----BEGIN-----\n")],
        &[("docs/big.txt", big.as_str())],
    ];
    let mut on = root.commit_id.clone();
    for files in steps {
        let change = commit_on(client, &repo.id, &on, files).await;
        on.clone_from(&change.commit_id);
        commits.push(change);
    }
    let tested = commit_on(
        client,
        &repo.id,
        &on,
        &[("crates/b/src/lib.rs", "pub fn b() {}\n")],
    )
    .await;
    let ci = remote_client(daemon, "ci", RemoteScope::Write, "ci-main").await;
    let key = ci
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    add(&ci, &repo.id, &tested.tree_id, "test", &key.signing_key).await;
    let left = commit_on(
        client,
        &repo.id,
        &tested.commit_id,
        &[("docs/left.md", "l\n")],
    )
    .await;
    let right = commit_on(
        client,
        &repo.id,
        &tested.commit_id,
        &[("docs/right.md", "r\n")],
    )
    .await;
    commits.extend([tested, left, right]);
    let (root_dir, parents) = (
        repo.root.clone(),
        vec![commits[6].commit_id.clone(), commits[7].commit_id.clone()],
    );
    let merge = tokio::task::spawn_blocking(move || merge_commit(&root_dir, &parents))
        .await
        .unwrap();
    bookmark(repo, "main", &merge).await;
    History { commits, merge }
}

fn simulate(repo: &str, policy: &str) -> v1::SimulatePolicyRequest {
    v1::SimulatePolicyRequest {
        repo: repo.to_owned(),
        policy: policy.as_bytes().to_vec(),
        ..v1::SimulatePolicyRequest::default()
    }
}

async fn simulated(
    client: &Client,
    request: v1::SimulatePolicyRequest,
) -> v1::SimulatePolicyResponse {
    client.simulate_policy(request).await.unwrap()
}

fn landing<'a>(
    response: &'a v1::SimulatePolicyResponse,
    commit: &str,
) -> &'a v1::PolicySimulatedLanding {
    response
        .landings
        .iter()
        .find(|landing| landing.commit_id == commit)
        .unwrap_or_else(|| panic!("no landing {commit}: {response:#?}"))
}

fn unmet_names(landing: &v1::PolicySimulatedLanding) -> Vec<&str> {
    landing
        .unmet
        .iter()
        .map(|rule| rule.name.as_str())
        .collect()
}

// 1 to 3, 8.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one history through every outcome")]
async fn simulate_over_landings() {
    use v1::PolicySimulationOutcome as Outcome;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let history = history(&daemon, &repo).await;

    let response = simulated(client, simulate(&repo.id, PROPOSED)).await;
    // Trend data, not a gate (docs/benchmarks.md): the median of five
    // simulations of the eight landings, socket, warm cache.
    let mut times = Vec::new();
    for _ in 0..5 {
        let started = Instant::now();
        simulated(client, simulate(&repo.id, PROPOSED)).await;
        times.push(started.elapsed());
    }
    times.sort();
    println!("SimulatePolicy median, 8 landings: {:?}", times[2]);
    assert_eq!(response.proposed_decision(), PolicyDecision::Allow);
    let proposed = response.proposed.clone().unwrap();
    assert!(proposed.from_request);
    assert_eq!(
        proposed.sha256,
        reef_policy::sha256_hex(PROPOSED.as_bytes())
    );
    let baseline = response.baseline.clone().unwrap();
    assert_eq!(baseline.revision, "trunk()");
    assert_eq!(baseline.commit_id, history.merge);
    assert_eq!(baseline.sha256, reef_policy::sha256_hex(POLICY.as_bytes()));
    assert_eq!(response.baseline_state(), PolicyDecision::Allow);

    // 1. The window: every non-merge ancestor of main, newest first.
    let commits: Vec<&str> = response
        .landings
        .iter()
        .map(|landing| landing.commit_id.as_str())
        .collect();
    assert_eq!(commits.len(), 8, "{commits:?}");
    assert!(!commits.contains(&history.merge.as_str()));
    assert_eq!(commits[7], history.commit(0));
    assert_eq!(commits[6], history.commit(1));
    assert_eq!(commits[2], history.commit(5));
    assert_eq!(response.next_page_token, "");
    let first = landing(&response, history.commit(0));
    assert_eq!(first.subject, "");
    assert!(first.author.contains('<'));
    assert!(first.committed_at.is_some());

    // 2. Each landing against main's policy.
    let crates = landing(&response, history.commit(2));
    assert_eq!(crates.outcome(), Outcome::StillBlocked);
    assert_eq!(unmet_names(crates), ["tests"]);
    assert_eq!(crates.baseline_unmet, ["tests"]);
    // 3. Blocked only for evidence its tree does not have.
    assert!(crates.missing_evidence_only);
    let secrets = landing(&response, history.commit(3));
    assert_eq!(secrets.outcome(), Outcome::NewlyBlocked);
    assert_eq!(unmet_names(secrets), ["secrets"]);
    assert!(!secrets.missing_evidence_only);
    assert_eq!(secrets.unmet[0].paths, ["secrets/key.pem"]);
    let big = landing(&response, history.commit(4));
    assert_eq!(big.outcome(), Outcome::NewlyBlocked);
    assert_eq!(unmet_names(big), ["limits"]);
    let tested = landing(&response, history.commit(5));
    assert_eq!(tested.outcome(), Outcome::StillAllowed, "{tested:#?}");
    // 8. The landing that adds the policy needs the owners, as
    // EvaluatePolicy says.
    let policy = landing(&response, history.commit(1));
    assert_eq!(policy.outcome(), Outcome::StillBlocked);
    assert_eq!(unmet_names(policy), ["builtin:owners"]);
    assert!(!policy.missing_evidence_only);
    for at in [0, 6, 7] {
        assert_eq!(
            landing(&response, history.commit(at)).outcome(),
            Outcome::StillAllowed
        );
    }
    let summary = response.summary.clone().unwrap();
    assert_eq!(
        (
            summary.landings,
            summary.blocked,
            summary.missing_evidence_only,
            summary.newly_blocked,
            summary.still_blocked,
            summary.newly_allowed,
            summary.still_allowed
        ),
        (8, 4, 1, 2, 2, 0, 4)
    );
    let counts: Vec<(&str, u32)> = summary
        .rules
        .iter()
        .filter(|rule| rule.landings > 0)
        .map(|rule| (rule.name.as_str(), rule.landings))
        .collect();
    let every: Vec<&str> = summary
        .rules
        .iter()
        .map(|rule| rule.name.as_str())
        .collect();
    assert_eq!(
        every,
        [
            "builtin:input",
            "builtin:conflicts",
            "builtin:description",
            "builtin:policy-file",
            "builtin:owners",
            "tests",
            "limits",
            "secrets",
            "described"
        ]
    );
    assert_eq!(
        counts,
        [
            ("builtin:owners", 1),
            ("tests", 1),
            ("limits", 1),
            ("secrets", 1)
        ]
    );
    assert!(response.cost > 0);
    assert_eq!(
        response.cost,
        response
            .landings
            .iter()
            .map(|landing| landing.cost)
            .sum::<u64>()
    );

    // No baseline: BLOCKED and ALLOWED, evaluated once.
    let alone = simulated(
        client,
        v1::SimulatePolicyRequest {
            no_baseline: true,
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    assert!(alone.baseline.is_none());
    assert_eq!(alone.baseline_state(), PolicyDecision::Unspecified);
    let secrets = landing(&alone, history.commit(3));
    assert_eq!(secrets.outcome(), Outcome::Blocked);
    assert_eq!(secrets.baseline_decision(), PolicyDecision::Unspecified);
    assert_eq!(
        landing(&alone, history.commit(0)).outcome(),
        Outcome::Allowed
    );

    // A baseline with no policy compares with nothing.
    let unpoliced = simulated(
        client,
        v1::SimulatePolicyRequest {
            baseline_revision: history.commit(0).to_owned(),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    assert_eq!(unpoliced.baseline_state(), PolicyDecision::NoPolicy);
    assert_eq!(
        unpoliced.baseline.as_ref().unwrap().commit_id,
        history.commit(0)
    );
    let crates = landing(&unpoliced, history.commit(2));
    assert_eq!(crates.outcome(), Outcome::Blocked);
    assert_eq!(crates.baseline_decision(), PolicyDecision::NoPolicy);
    let summary = unpoliced.summary.unwrap();
    assert_eq!((summary.newly_blocked, summary.still_allowed), (0, 0));

    // The proposed policy as the baseline's own weaker policy: what it
    // newly allows.
    let weak = simulated(client, simulate(&repo.id, WEAK)).await;
    assert_eq!(
        landing(&weak, history.commit(2)).outcome(),
        Outcome::NewlyAllowed
    );

    // A revision narrows the window.
    let narrow = simulated(
        client,
        v1::SimulatePolicyRequest {
            revisions: format!("{}::{}", history.commit(3), history.commit(4)),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    let commits: Vec<&str> = narrow
        .landings
        .iter()
        .map(|landing| landing.commit_id.as_str())
        .collect();
    assert_eq!(commits, [history.commit(4), history.commit(3)]);

    // 3. A trusted failure is a reason of its own, not missing evidence.
    let ci = remote_client(&daemon, "ci-fail", RemoteScope::Write, "ci-main").await;
    let key = ci
        .issue_session_key("", Actor::Executor, None, "")
        .await
        .unwrap();
    add_outcome(
        &ci,
        &repo.id,
        &history.commits[2].tree_id,
        "test",
        reef_evidence::Outcome::Fail,
        &key.signing_key,
    )
    .await;
    let failing = simulated(
        client,
        v1::SimulatePolicyRequest {
            revisions: history.commit(2).to_owned(),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    let crates = landing(&failing, history.commit(2));
    assert_eq!(unmet_names(crates), ["tests"]);
    assert_eq!(crates.unmet[0].failed.len(), 1);
    assert!(!crates.missing_evidence_only);
    daemon.stop().await;
}

// 4, 5.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "paging and every bound")]
async fn simulation_pages_and_bounds() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let history = history(&daemon, &repo).await;
    let all = simulated(client, simulate(&repo.id, PROPOSED)).await;
    let every: Vec<String> = all
        .landings
        .iter()
        .map(|landing| landing.commit_id.clone())
        .collect();

    // 4. Pages of three walk the window once, at one operation, even after
    // the repository moves on.
    let mut seen = Vec::new();
    let mut token = String::new();
    let mut operations = Vec::new();
    let mut pages = 0;
    loop {
        let page = simulated(
            client,
            v1::SimulatePolicyRequest {
                page_size: 3,
                page_token: token.clone(),
                ..simulate(&repo.id, PROPOSED)
            },
        )
        .await;
        pages += 1;
        assert!(page.landings.len() <= 3);
        assert_eq!(
            page.summary.as_ref().unwrap().landings as usize,
            page.landings.len()
        );
        seen.extend(
            page.landings
                .iter()
                .map(|landing| landing.commit_id.clone()),
        );
        operations.push(page.at_operation.clone());
        if pages == 1 {
            // A new landing after the first page is not in this walk.
            let newer =
                commit_on(client, &repo.id, &history.merge, &[("docs/new.md", "n\n")]).await;
            bookmark(&repo, "main", &newer.commit_id).await;
        }
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(pages, 3);
    assert_eq!(seen, every);
    assert!(operations.iter().all(|op| *op == operations[0]));
    // 1. At head the new landing leads; at the first page's operation it
    // is not there.
    let now = simulated(client, simulate(&repo.id, PROPOSED)).await;
    assert_eq!(now.landings.len(), every.len() + 1);
    let then = simulated(
        client,
        v1::SimulatePolicyRequest {
            at_operation: operations[0].clone(),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    assert_eq!(then.at_operation, operations[0]);
    let then: Vec<String> = then
        .landings
        .iter()
        .map(|landing| landing.commit_id.clone())
        .collect();
    assert_eq!(then, every);
    bookmark(&repo, "main", &history.merge).await;

    // A token with other fields, a bad token, a negative size.
    let first = simulated(
        client,
        v1::SimulatePolicyRequest {
            page_size: 3,
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    let error = client
        .simulate_policy(v1::SimulatePolicyRequest {
            page_size: 3,
            page_token: first.next_page_token.clone(),
            no_baseline: true,
            ..simulate(&repo.id, PROPOSED)
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::INVALID_PAGE_TOKEN);
    let error = client
        .simulate_policy(v1::SimulatePolicyRequest {
            page_size: -1,
            ..simulate(&repo.id, PROPOSED)
        })
        .await
        .unwrap_err();
    assert_eq!(
        reef_lib::error::Classify::kind(&error),
        reef_lib::error::ErrorKind::InvalidArgument
    );
    // 0 is 20 and 500 is 100, over a window of 32 commits.
    let mut on = history.commit(7).to_owned();
    for n in 0..25 {
        let change = commit_on(client, &repo.id, &on, &[("docs/n.md", &format!("{n}\n"))]).await;
        on = change.commit_id;
    }
    let window = format!("::{on} ~ root()");
    let default = simulated(
        client,
        v1::SimulatePolicyRequest {
            revisions: window.clone(),
            no_baseline: true,
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    assert_eq!(default.landings.len(), 20);
    assert_ne!(default.next_page_token, "");
    let clamped = simulated(
        client,
        v1::SimulatePolicyRequest {
            revisions: window,
            no_baseline: true,
            page_size: 500,
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    assert_eq!(clamped.landings.len(), 32);

    // 5. The page's cost and path bounds end it after the landing that
    // passes them, the same on every run.
    let policy = &daemon.handle.state().policy;
    policy
        .page_cost
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let costly = simulated(client, simulate(&repo.id, PROPOSED)).await;
    assert_eq!(costly.landings.len(), 1);
    assert!(costly.cost > 1);
    assert_ne!(costly.next_page_token, "");
    let again = simulated(client, simulate(&repo.id, PROPOSED)).await;
    assert_eq!(again.landings, costly.landings);
    // Following the tokens from a page the bound ended walks every landing
    // once.
    let mut walked = Vec::new();
    let mut token = String::new();
    loop {
        let page = simulated(
            client,
            v1::SimulatePolicyRequest {
                page_token: token.clone(),
                ..simulate(&repo.id, PROPOSED)
            },
        )
        .await;
        assert_eq!(page.landings.len(), 1);
        walked.extend(
            page.landings
                .iter()
                .map(|landing| landing.commit_id.clone()),
        );
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(walked, every);
    policy
        .page_cost
        .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    policy
        .page_paths
        .store(1, std::sync::atomic::Ordering::Relaxed);
    let wide = simulated(
        client,
        v1::SimulatePolicyRequest {
            revisions: format!("{}::{}", history.commit(3), history.commit(5)),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    // One path each: the page ends after the second.
    assert_eq!(wide.landings.len(), 2);
    assert_ne!(wide.next_page_token, "");
    // The rows read: the tested landing's evidence passes a bound of 0.
    policy
        .page_paths
        .store(u64::MAX, std::sync::atomic::Ordering::Relaxed);
    policy
        .page_rows
        .store(0, std::sync::atomic::Ordering::Relaxed);
    let rows = simulated(
        client,
        v1::SimulatePolicyRequest {
            revisions: format!("{}::{}", history.commit(4), history.commit(6)),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    let commits: Vec<&str> = rows
        .landings
        .iter()
        .map(|landing| landing.commit_id.as_str())
        .collect();
    assert_eq!(commits, [history.commit(6), history.commit(5)]);
    assert_ne!(rows.next_page_token, "");
    daemon.stop().await;
}

// 6, 7.
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every source and the parity check")]
async fn simulation_sources() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let history = history(&daemon, &repo).await;

    // From a revision: a change on main that proposes the policy.
    let proposal = commit_on(
        client,
        &repo.id,
        &history.merge,
        &[("reef-policy.toml", PROPOSED)],
    )
    .await;
    let compiles = daemon.handle.state().policy.compiles();
    let from_text = simulated(client, simulate(&repo.id, PROPOSED)).await;
    let from_revision = simulated(
        client,
        v1::SimulatePolicyRequest {
            policy: Vec::new(),
            policy_revision: proposal.commit_id.clone(),
            ..simulate(&repo.id, "")
        },
    )
    .await;
    // Main's policy and the proposed one, each compiled once.
    assert!(daemon.handle.state().policy.compiles() <= compiles + 2);
    assert_eq!(from_text.landings, from_revision.landings);
    let source = from_revision.proposed.unwrap();
    assert_eq!(source.commit_id, proposal.commit_id);
    assert_eq!(source.path, "reef-policy.toml");
    assert_eq!(source.blob_id.len(), 40);
    assert!(!source.from_request);

    // Errors.
    let both = client
        .simulate_policy(v1::SimulatePolicyRequest {
            policy_revision: proposal.commit_id.clone(),
            ..simulate(&repo.id, PROPOSED)
        })
        .await
        .unwrap_err();
    assert_eq!(both.reason(), reef_api::error::POLICY_SOURCE_CONFLICT);
    let neither = client
        .simulate_policy(simulate(&repo.id, ""))
        .await
        .unwrap_err();
    assert_eq!(neither.reason(), reef_api::error::POLICY_SOURCE_MISSING);
    assert!(neither.advice().is_some());
    let large = client
        .simulate_policy(v1::SimulatePolicyRequest {
            policy: vec![b'#'; 257 * 1024],
            ..simulate(&repo.id, "")
        })
        .await
        .unwrap_err();
    assert_eq!(large.reason(), reef_api::error::POLICY_TOO_LARGE);

    // A proposed policy that does not load, or is not there: answered at
    // once.
    let invalid = simulated(client, simulate(&repo.id, "version = 2\n")).await;
    assert_eq!(invalid.proposed_decision(), PolicyDecision::InvalidPolicy);
    assert_ne!(invalid.diagnostics.len(), 0);
    assert_eq!(invalid.landings.len(), 0);
    assert_eq!(invalid.next_page_token, "");
    let missing = simulated(
        client,
        v1::SimulatePolicyRequest {
            policy_revision: history.commit(0).to_owned(),
            ..simulate(&repo.id, "")
        },
    )
    .await;
    assert_eq!(missing.proposed_decision(), PolicyDecision::NoPolicy);
    assert_eq!(missing.landings.len(), 0);

    // An invalid baseline blocks everything it judges.
    let broken = commit_on(
        client,
        &repo.id,
        &history.merge,
        &[("reef-policy.toml", "version = 2\n")],
    )
    .await;
    let against_broken = simulated(
        client,
        v1::SimulatePolicyRequest {
            baseline_revision: broken.commit_id.clone(),
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    assert_eq!(
        against_broken.baseline_state(),
        PolicyDecision::InvalidPolicy
    );
    let broken_source = against_broken.baseline.clone().unwrap();
    assert_eq!(
        broken_source.sha256,
        reef_policy::sha256_hex(b"version = 2\n")
    );
    assert_eq!(broken_source.version, 1);
    assert_eq!(
        landing(&against_broken, history.commit(0)).outcome(),
        v1::PolicySimulationOutcome::NewlyAllowed
    );
    assert_eq!(
        landing(&against_broken, history.commit(3)).outcome(),
        v1::PolicySimulationOutcome::StillBlocked
    );

    // 7. Each landing's evaluation is EvaluatePolicy's, with the same text.
    let full = simulated(
        client,
        v1::SimulatePolicyRequest {
            include_evaluations: true,
            ..simulate(&repo.id, PROPOSED)
        },
    )
    .await;
    for landing in &full.landings {
        let simulated = landing.evaluation.clone().unwrap();
        let evaluated = evaluated(
            client,
            EvaluatePolicyRequest {
                policy: PROPOSED.as_bytes().to_vec(),
                include_input: false,
                ..on(&repo.id, &landing.commit_id)
            },
        )
        .await;
        assert_eq!(simulated.decision, evaluated.decision);
        assert_eq!(simulated.rules, evaluated.rules);
        assert_eq!(simulated.input, evaluated.input);
        assert_eq!(simulated.cost, evaluated.cost);
        assert_eq!(simulated.diagnostics, evaluated.diagnostics);
        assert_eq!(simulated.commit_id, evaluated.commit_id);
        assert_eq!(simulated.tree_id, evaluated.tree_id);
        assert_eq!(simulated.source, evaluated.source);
        assert_eq!(simulated.at_operation, evaluated.at_operation);
        assert_eq!(simulated.at_operation, full.at_operation);
        let baseline = landing.baseline_evaluation.clone().unwrap();
        let main = evaluated_main(client, &repo.id, &landing.commit_id).await;
        assert_eq!(baseline.rules, main.rules);
        assert_eq!(baseline.source, main.source);
        assert_eq!(baseline.source, full.baseline);
    }
    daemon.stop().await;
}

async fn evaluated_main(client: &Client, repo: &str, commit: &str) -> v1::PolicyEvaluation {
    evaluated(
        client,
        EvaluatePolicyRequest {
            include_input: false,
            ..on(repo, commit)
        },
    )
    .await
}

// 9.
#[tokio::test(flavor = "multi_thread")]
async fn simulation_remote() {
    let (daemon, repo) = fixture().await;
    let history = history(&daemon, &repo).await;
    let reader = remote_client(&daemon, "reader", RemoteScope::Read, "").await;
    let response = simulated(&reader, simulate(&repo.id, PROPOSED)).await;
    assert_eq!(response.landings.len(), 8);
    assert_eq!(
        landing(&response, history.commit(3)).outcome(),
        v1::PolicySimulationOutcome::NewlyBlocked
    );
    // Each evaluation takes one of the daemon's turns: with three held
    // elsewhere it runs, with all four held it waits.
    let turns = std::sync::Arc::clone(&daemon.handle.state().policy.turns);
    let three = turns.acquire_many(3).await.unwrap();
    let alone = simulated(&reader, simulate(&repo.id, PROPOSED)).await;
    assert_eq!(alone.landings.len(), 8);
    let fourth = turns.acquire().await.unwrap();
    let waiting = {
        let reader = reader.clone();
        let request = simulate(&repo.id, PROPOSED);
        tokio::spawn(async move { reader.simulate_policy(request).await })
    };
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!waiting.is_finished());
    drop((three, fourth));
    assert_eq!(waiting.await.unwrap().unwrap().landings.len(), 8);
    assert_eq!(
        turns.available_permits(),
        reef_daemon::policy::CONCURRENT_EVALUATIONS
    );
    daemon.stop().await;
}
