//! `reef.api.v1.EvidenceService` through the daemon (R3.01, tests 9 to 15):
//! adds and reads, refusals with their reasons, links across rewrites,
//! filters and pages, tampered rows, GC, backup and forget, and the remote
//! listener's rules.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::collections::BTreeSet;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use reef_api::v1::{
    self, AbandonChangeRequest, AddEvidenceRequest, AddEvidenceResponse, CreateWorkspaceRequest,
    DescribeChangeRequest, EditFilesRequest, EvidenceResult, FileEdit, GetChangeRequest,
    ListEvidenceRequest, ListOperationsRequest, RemoteScope, SignatureCheck, UndoOperationRequest,
    Workspace, edit_files_request, file_edit,
};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::Config;
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::{Artifact, Claim, Outcome, SigningKey, TreeDigest};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_testing::synth::Preset;
use serde_json::{Map, Value, json};

const OWNER: &str = "tester@host";

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reef-evidence/tests/fixtures"
);

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.gc.undo_window = Duration::ZERO;
        configure(config);
    })
    .await;
    daemon.handle.state().manager.scheduler().pause_watcher();
    (daemon, repo)
}

async fn fixture() -> (TestDaemon, SyntheticRepo) {
    fixture_with(|_| {}).await
}

async fn create(client: &Client, repo: &str) -> Workspace {
    client
        .create_workspace(CreateWorkspaceRequest {
            repo: repo.to_owned(),
            owner: OWNER.to_owned(),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap()
}

fn write(ws: &Workspace, path: &str, contents: &str) {
    std::fs::write(Path::new(&ws.path).join(path), contents).unwrap();
}

/// The change `revision` names, as the daemon reads it, without a
/// snapshot.
async fn change(client: &Client, ws: &Workspace, revision: &str) -> v1::Change {
    client
        .get_change(GetChangeRequest {
            workspace: ws.id.clone(),
            revision: revision.to_owned(),
            skip_snapshot: true,
            ..GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

/// `ws` snapshotted after writing `path`; its change.
async fn edit(client: &Client, ws: &Workspace, path: &str, contents: &str) -> v1::Change {
    write(ws, path, contents);
    client.snapshot_workspace("", &ws.id, "").await.unwrap();
    change(client, ws, "@").await
}

fn key(seed: u8) -> SigningKey {
    SigningKey::from_seed(&[seed; 32]).unwrap()
}

fn claim(kind: &str, result: Outcome, target: Option<&str>) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: kind.to_owned(),
        result,
        target: target.map(str::to_owned),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: vec![Artifact {
            name: "test.log".into(),
            sha256: reef_evidence::sha256_hex(b"log\n"),
            uri: None,
            media_type: Some("text/plain".into()),
        }],
        annotations: Map::from_iter([("runner".to_owned(), json!("ci-7"))]),
    }
}

/// `claim` about `tree`, signed by `key`.
fn envelope(claim: &Claim, tree: &str, key: &SigningKey) -> Vec<u8> {
    claim
        .sign(&TreeDigest::git(tree).unwrap(), key)
        .unwrap()
        .to_bytes()
}

fn add_request(
    ws: &Workspace,
    envelope: Vec<u8>,
    key: &SigningKey,
    revision: &str,
) -> AddEvidenceRequest {
    AddEvidenceRequest {
        repo: String::new(),
        workspace: ws.id.clone(),
        envelope,
        public_key: key.public_key().spki().to_vec(),
        revision: revision.to_owned(),
    }
}

async fn add(
    client: &Client,
    ws: &Workspace,
    envelope: Vec<u8>,
    key: &SigningKey,
    revision: &str,
) -> AddEvidenceResponse {
    client
        .add_evidence(add_request(ws, envelope, key, revision))
        .await
        .unwrap()
}

fn list(ws: &Workspace) -> ListEvidenceRequest {
    ListEvidenceRequest {
        workspace: ws.id.clone(),
        ..ListEvidenceRequest::default()
    }
}

async fn listed(client: &Client, request: ListEvidenceRequest) -> Vec<v1::Evidence> {
    client.list_evidence(request, 0).await.unwrap().items
}

fn ids(evidence: &[v1::Evidence]) -> BTreeSet<String> {
    evidence.iter().map(|e| e.evidence_id.clone()).collect()
}

fn meta<'a>(error: &'a ClientError, key: &str) -> &'a str {
    error.metadata().get(key).map_or("", String::as_str)
}

fn sha256(bytes: &[u8]) -> String {
    reef_evidence::sha256_hex(bytes)
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

fn counts(daemon: &TestDaemon) -> (u64, u64, u64) {
    daemon.handle.state().db.evidence_counts().unwrap()
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn add_and_get() {
    let (daemon, repo) = fixture().await;
    let client = daemon.client.clone().with_agent("tester-agent").unwrap();
    let ws = create(&client, &repo.id).await;
    let at = edit(&client, &ws, "a.txt", "a\n").await;
    let signer = key(1);
    let bytes = envelope(
        &claim("test", Outcome::Pass, Some("unit")),
        &at.tree_id,
        &signer,
    );
    let first = add(&client, &ws, bytes.clone(), &signer, &at.commit_id).await;
    assert!(first.created);
    let evidence = first.evidence.unwrap();
    assert_eq!(evidence.evidence_id, sha256(&bytes));
    assert_eq!(evidence.tree_id, at.tree_id);
    assert_eq!(evidence.kind, "test");
    assert_eq!(evidence.result(), EvidenceResult::Pass);
    assert_eq!(evidence.target, "unit");
    assert_eq!(
        evidence.key_id,
        signer.public_key().id().as_str(),
        "the key, never a name"
    );
    assert_eq!(evidence.signature(), SignatureCheck::Valid);
    assert_eq!(evidence.change_ids, std::slice::from_ref(&at.change_id));
    assert_eq!(evidence.added_by_agent, "tester-agent");
    assert_eq!(evidence.predicate_type, reef_evidence::PREDICATE_TYPE);
    assert_eq!(evidence.artifacts.len(), 1);
    assert_eq!(evidence.artifacts[0].sha256, sha256(b"log\n"));
    assert_eq!(evidence.envelope, b"");
    assert_eq!(
        evidence.started_at.unwrap().seconds,
        1_793_872_800,
        "the claim's times"
    );

    // Idempotent.
    let again = add(&client, &ws, bytes.clone(), &signer, &at.commit_id).await;
    assert!(!again.created);
    assert_eq!(again.evidence.unwrap(), evidence);
    assert_eq!(counts(&daemon), (1, 1, 1));

    for id in [&evidence.evidence_id[..], &evidence.evidence_id[..8]] {
        let got = client.get_evidence(&repo.id, id).await.unwrap();
        let got = got.evidence.unwrap();
        assert_eq!(sha256(&got.envelope), evidence.evidence_id);
        assert_eq!(
            got.envelope, bytes,
            "a canonical envelope is stored as sent"
        );
        assert_eq!(got.signature(), SignatureCheck::Valid);
    }

    // Trend data: the median add and get over 50 calls.
    let mut adds = Vec::new();
    let mut gets = Vec::new();
    for n in 0..50 {
        let bytes = envelope(
            &claim("test", Outcome::Pass, Some(&format!("t{n}"))),
            &at.tree_id,
            &signer,
        );
        let start = Instant::now();
        let added = add(&client, &ws, bytes, &signer, "").await;
        adds.push(start.elapsed());
        let id = added.evidence.unwrap().evidence_id;
        let start = Instant::now();
        client.get_evidence(&repo.id, &id).await.unwrap();
        gets.push(start.elapsed());
    }
    eprintln!(
        "AddEvidence median {:?}, GetEvidence median {:?} over 50 calls",
        median(adds),
        median(gets)
    );
    daemon.stop().await;
}

/// A conflicted merge commit of two sides of `on`, made outside the
/// daemon as a user's `jj` would; its id.
fn conflicted_commit(root: &Path, on: &str) -> String {
    use jj_lib::backend::{CopyId, TreeValue};
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::object_id::ObjectId as _;
    use jj_lib::repo::Repo as _;
    use jj_lib::repo_path::RepoPathBuf;
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
    let store = tx.repo().store().clone();
    let base = store
        .get_commit(&jj_lib::backend::CommitId::try_from_hex(on).unwrap())
        .unwrap();
    let mut sides = Vec::new();
    for text in ["x\n", "y\n"] {
        let path = RepoPathBuf::from_internal_string("conflicted.txt").unwrap();
        let id = pollster::block_on(store.write_file(&path, &mut text.as_bytes())).unwrap();
        let mut builder = jj_lib::merged_tree_builder::MergedTreeBuilder::new(base.tree());
        builder.set_or_remove(
            path,
            Some(TreeValue::File {
                id,
                executable: false,
                copy_id: CopyId::placeholder(),
            }),
        );
        let tree = pollster::block_on(builder.write_tree()).unwrap();
        sides.push(
            pollster::block_on(
                tx.repo_mut()
                    .new_commit(vec![base.id().clone()], tree)
                    .set_description(text.trim())
                    .write(),
            )
            .unwrap(),
        );
    }
    let merged =
        pollster::block_on(jj_lib::rewrite::merge_commit_trees(tx.repo(), &sides)).unwrap();
    let merge = pollster::block_on(
        tx.repo_mut()
            .new_commit(sides.iter().map(|c| c.id().clone()).collect(), merged)
            .set_description("merge")
            .write(),
    )
    .unwrap();
    pollster::block_on(tx.commit("conflict")).unwrap();
    merge.id().hex()
}

// 10
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "every refusal in turn")]
async fn add_refusals_through_the_api() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let parent = change(client, &ws, "@-").await;
    assert_ne!(at.tree_id, parent.tree_id);
    let before = counts(&daemon);

    // Every fixture of reef-evidence's test 3, through the API.
    let manifest: Value =
        serde_json::from_slice(&std::fs::read(Path::new(FIXTURES).join("refusals.json")).unwrap())
            .unwrap();
    let refusals = manifest["refusals"].as_array().unwrap();
    assert!(refusals.len() >= 29, "{}", refusals.len());
    let mut checks = BTreeSet::new();
    for refusal in refusals {
        let file = refusal["file"].as_str().unwrap();
        let request = AddEvidenceRequest {
            workspace: ws.id.clone(),
            envelope: std::fs::read(Path::new(FIXTURES).join(file)).unwrap(),
            public_key: STANDARD
                .decode(refusal["public_key"].as_str().unwrap())
                .unwrap(),
            ..AddEvidenceRequest::default()
        };
        let error = client.add_evidence(request).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{file}: {error}");
        if refusal["signature_invalid"].as_bool().unwrap() {
            assert_eq!(error.reason(), reef_api::error::SIGNATURE_INVALID, "{file}");
            assert_eq!(
                meta(&error, "key_id"),
                manifest["valid"]["key_id"].as_str().unwrap()
            );
            continue;
        }
        assert_eq!(error.reason(), reef_api::error::INVALID_EVIDENCE, "{file}");
        let check = refusal["check"].as_str().unwrap();
        assert_eq!(meta(&error, "check"), check, "{file}: {error}");
        assert_eq!(
            meta(&error, "field"),
            refusal["field"].as_str().unwrap_or_default(),
            "{file}: {error}"
        );
        assert!(!error.advice().unwrap().hint.is_empty(), "{file}");
        checks.insert(check.to_owned());
    }
    assert_eq!(checks.len(), 10, "every check: {checks:?}");

    let signer = key(1);
    // A tree the repository does not hold.
    let missing = "1".repeat(40);
    let bytes = envelope(&claim("test", Outcome::Pass, None), &missing, &signer);
    let error = client
        .add_evidence(add_request(&ws, bytes, &signer, ""))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound, "{error}");
    assert_eq!(error.reason(), reef_api::error::TREE_NOT_FOUND);
    assert_eq!(meta(&error, "tree_id"), missing);
    // A blob's id where a tree's belongs.
    let blob = blob_id(&repo.root, &at.commit_id, "a.txt");
    let bytes = envelope(&claim("test", Outcome::Pass, None), &blob, &signer);
    let error = client
        .add_evidence(add_request(&ws, bytes, &signer, ""))
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::TREE_NOT_FOUND, "{error}");
    assert!(error.to_string().contains("not a tree"), "{error}");

    // A revision with another tree.
    let bytes = envelope(&claim("test", Outcome::Pass, None), &at.tree_id, &signer);
    let error = client
        .add_evidence(add_request(&ws, bytes.clone(), &signer, &parent.commit_id))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert_eq!(error.reason(), reef_api::error::EVIDENCE_TREE_MISMATCH);
    assert_eq!(meta(&error, "tree_id"), at.tree_id);
    assert_eq!(meta(&error, "commit_id"), parent.commit_id);
    assert_eq!(meta(&error, "commit_tree_id"), parent.tree_id);

    // A conflicted revision.
    let merge = conflicted_commit(&repo.root, &at.commit_id);
    let error = client
        .add_evidence(add_request(&ws, bytes.clone(), &signer, &merge))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
    assert_eq!(error.reason(), reef_api::error::CONFLICTED_REVISION);
    assert_eq!(meta(&error, "commit_id"), merge);
    // Listing a conflicted revision is an empty page, not an error.
    let page = listed(
        client,
        ListEvidenceRequest {
            revision: merge.clone(),
            ..list(&ws)
        },
    )
    .await;
    assert_eq!(ids(&page), BTreeSet::new());

    // A revision that does not resolve.
    let error = client
        .add_evidence(add_request(&ws, bytes, &signer, "nonesuch"))
        .await
        .unwrap_err();
    assert_eq!(
        error.reason(),
        reef_api::error::REVISION_NOT_FOUND,
        "{error}"
    );

    assert_eq!(counts(&daemon), before, "no refused add wrote anything");

    // Lookups by id.
    let error = client.get_evidence(&repo.id, "abc").await.unwrap_err();
    assert_eq!(
        error.reason(),
        reef_api::error::INVALID_EVIDENCE_ID,
        "{error}"
    );
    let error = client.get_evidence(&repo.id, "abcd").await.unwrap_err();
    assert_eq!(error.kind(), ErrorKind::NotFound);
    assert_eq!(
        error.reason(),
        reef_api::error::EVIDENCE_NOT_FOUND,
        "{error}"
    );
    assert_eq!(
        error.advice().unwrap().next,
        ["reef", "evidence", "list"],
        "{error}"
    );
    daemon.stop().await;
}

/// The git blob id of `path` in `commit`.
fn blob_id(root: &Path, commit: &str, path: &str) -> String {
    let output = std::process::Command::new("git")
        .arg("-C")
        .arg(root)
        .args(["rev-parse", &format!("{commit}:{path}")])
        .env("GIT_DIR", root.join(".jj/repo/store/git"))
        .output()
        .unwrap();
    assert!(output.status.success(), "{output:?}");
    String::from_utf8(output.stdout).unwrap().trim().to_owned()
}

// 11
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one change through its rewrites")]
async fn links_survive_rewrites() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let c = edit(client, &ws, "a.txt", "a\n").await;
    let tree = c.tree_id.clone();
    let signer = key(1);
    let added = add(
        client,
        &ws,
        envelope(&claim("test", Outcome::Pass, None), &tree, &signer),
        &signer,
        &c.commit_id,
    )
    .await;
    let id = added.evidence.unwrap().evidence_id;
    let by_revision = |revision: &str| ListEvidenceRequest {
        revision: revision.to_owned(),
        skip_snapshot: true,
        ..list(&ws)
    };
    let by_change = ListEvidenceRequest {
        change_id: c.change_id.clone(),
        ..list(&ws)
    };

    // Describe: the tree stays, and so does the evidence.
    client
        .describe_change(DescribeChangeRequest {
            workspace: ws.id.clone(),
            revision: "@".into(),
            description: "described".into(),
            ..DescribeChangeRequest::default()
        })
        .await
        .unwrap();
    let described = change(client, &ws, "@").await;
    assert_ne!(described.commit_id, c.commit_id);
    assert_eq!(described.tree_id, tree);
    assert_eq!(
        ids(&listed(client, by_revision(&c.change_id)).await),
        [id.clone()].into()
    );
    let linked = listed(client, by_change.clone()).await;
    assert_eq!(ids(&linked), [id.clone()].into());
    assert!(linked[0].current);

    // An edit and a snapshot: the change has moved on.
    let moved = edit(client, &ws, "a.txt", "changed\n").await;
    assert_ne!(moved.tree_id, tree);
    assert_eq!(
        ids(&listed(client, by_revision(&c.change_id)).await),
        BTreeSet::new()
    );
    let linked = listed(client, by_change.clone()).await;
    assert_eq!(ids(&linked), [id.clone()].into());
    assert!(!linked[0].current, "not on the change's tree now");
    let by_tree = ListEvidenceRequest {
        tree_id: tree.clone(),
        ..list(&ws)
    };
    assert_eq!(
        ids(&listed(client, by_tree.clone()).await),
        [id.clone()].into()
    );

    // Abandon, then undo the abandon: nothing about the evidence changes.
    let abandoned = client
        .abandon_change(AbandonChangeRequest {
            workspace: ws.id.clone(),
            revision: c.change_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let linked = listed(client, by_change.clone()).await;
    assert_eq!(ids(&linked), [id.clone()].into());
    assert!(!linked[0].current, "an abandoned change has no tree now");
    assert_eq!(linked[0].change_ids, std::slice::from_ref(&c.change_id));
    client
        .undo_operation(UndoOperationRequest {
            repo: repo.id.clone(),
            operation_id: abandoned.operation.unwrap().id,
            ..UndoOperationRequest::default()
        })
        .await
        .unwrap();
    assert_eq!(
        ids(&listed(client, by_change.clone()).await),
        [id.clone()].into()
    );
    assert_eq!(ids(&listed(client, by_tree).await), [id.clone()].into());
    assert_eq!(counts(&daemon).0, 1);
    let operations = client
        .list_operations_limited(
            ListOperationsRequest {
                repo: repo.id.clone(),
                ..ListOperationsRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    for operation in &operations.items {
        assert!(
            !operation.description.to_lowercase().contains("evidence"),
            "{operation:?}"
        );
    }

    // A second change with tree T shares the evidence.
    let parent = c.parent_commit_ids[0].clone();
    let twin = client
        .edit_files(EditFilesRequest {
            repo: repo.id.clone(),
            target: Some(edit_files_request::Target::NewOn(parent)),
            edits: vec![FileEdit {
                path: "a.txt".into(),
                action: Some(file_edit::Action::Content(b"a\n".to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_ne!(twin.change_id, c.change_id);
    assert_eq!(twin.tree_id, tree);
    let shared = listed(client, by_revision(&twin.commit_id)).await;
    assert_eq!(ids(&shared), [id].into());
    assert!(
        !shared[0].current,
        "`current` is only for a listing by change"
    );
    daemon.stop().await;
}

/// An object test 12 added: its id, tree, result, kind and key id.
type Added = (String, String, Outcome, String, String);

// 12
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "each filter, then pages")]
async fn list_filters_and_pages() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let parent = change(client, &ws, "@-").await;
    let trees = [at.tree_id.clone(), parent.tree_id.clone()];
    let kinds = ["test", "build", "lint"];
    let results = [Outcome::Pass, Outcome::Fail, Outcome::Error];
    let keys = [key(1), key(2)];
    // 120 objects: each (tree, kind, result, key) combination five times.
    let mut all: Vec<Added> = Vec::new();
    for n in 0..120_usize {
        let tree = &trees[n % 2];
        let kind = kinds[(n / 2) % 3];
        let result = results[(n / 6) % 3];
        let signer = &keys[(n / 18) % 2];
        let target = format!("t{n}");
        let added = add(
            client,
            &ws,
            envelope(&claim(kind, result, Some(&target)), tree, signer),
            signer,
            "",
        )
        .await;
        let id = added.evidence.unwrap().evidence_id;
        all.push((
            id,
            tree.clone(),
            result,
            kind.to_owned(),
            signer.public_key().id().to_string(),
        ));
    }
    let expect = |keep: &dyn Fn(&Added) -> bool| {
        all.iter()
            .filter(|e| keep(e))
            .map(|e| e.0.clone())
            .collect::<BTreeSet<_>>()
    };
    let result_value = |outcome: Outcome| {
        i32::from(match outcome {
            Outcome::Pass => EvidenceResult::Pass,
            Outcome::Fail => EvidenceResult::Fail,
            Outcome::Error => EvidenceResult::Error,
        })
    };
    let everything = listed(client, list(&ws)).await;
    assert_eq!(everything.len(), 120);
    let cases: Vec<(ListEvidenceRequest, BTreeSet<String>)> = vec![
        (
            ListEvidenceRequest {
                tree_id: trees[1].clone(),
                ..list(&ws)
            },
            expect(&|e| e.1 == trees[1]),
        ),
        (
            ListEvidenceRequest {
                revision: "@".into(),
                skip_snapshot: true,
                ..list(&ws)
            },
            expect(&|e| e.1 == trees[0]),
        ),
        (
            ListEvidenceRequest {
                kinds: vec!["lint".into(), "build".into()],
                ..list(&ws)
            },
            expect(&|e| e.3 != "test"),
        ),
        (
            ListEvidenceRequest {
                results: vec![result_value(Outcome::Error)],
                ..list(&ws)
            },
            expect(&|e| e.2 == Outcome::Error),
        ),
        (
            ListEvidenceRequest {
                key_ids: vec![keys[1].public_key().id().to_string()],
                ..list(&ws)
            },
            expect(&|e| e.4 == keys[1].public_key().id().as_str()),
        ),
        (
            ListEvidenceRequest {
                target: "t7".into(),
                ..list(&ws)
            },
            [all[7].0.clone()].into(),
        ),
        // Two combined.
        (
            ListEvidenceRequest {
                tree_id: trees[0].clone(),
                kinds: vec!["build".into()],
                ..list(&ws)
            },
            expect(&|e| e.1 == trees[0] && e.3 == "build"),
        ),
        (
            ListEvidenceRequest {
                results: vec![result_value(Outcome::Pass), result_value(Outcome::Fail)],
                key_ids: vec![keys[0].public_key().id().to_string()],
                ..list(&ws)
            },
            expect(&|e| e.2 != Outcome::Error && e.4 == keys[0].public_key().id().as_str()),
        ),
    ];
    for (request, expected) in cases {
        assert!(!expected.is_empty());
        assert_eq!(
            ids(&listed(client, request.clone()).await),
            expected,
            "{request:?}"
        );
    }

    // Pages of 50, newest first, with 10 more added between pages.
    let mut request = ListEvidenceRequest {
        page_size: 50,
        ..list(&ws)
    };
    let mut seen = Vec::new();
    let mut pages = 0;
    let mut timings = Vec::new();
    loop {
        let start = Instant::now();
        let page = client.list_evidence_page(request.clone()).await.unwrap();
        timings.push(start.elapsed());
        assert!(page.evidence.len() <= 50);
        let added: Vec<i64> = page
            .evidence
            .iter()
            .map(|e| reef_daemon::client::json::millis_of(e.added_at.as_ref().unwrap()))
            .collect();
        assert!(added.is_sorted_by(|a, b| a >= b), "newest first");
        seen.extend(page.evidence.into_iter().map(|e| e.evidence_id));
        pages += 1;
        if page.next_page_token.is_empty() {
            break;
        }
        for n in 0..10 {
            let signer = &keys[0];
            let target = format!("late{pages}-{n}");
            add(
                client,
                &ws,
                envelope(
                    &claim("test", Outcome::Pass, Some(&target)),
                    &trees[0],
                    signer,
                ),
                signer,
                "",
            )
            .await;
        }
        request.page_token = page.next_page_token;
    }
    assert_eq!(pages, 3);
    let unique: BTreeSet<&String> = seen.iter().collect();
    assert_eq!(unique.len(), seen.len(), "no duplicates");
    assert_eq!(unique.len(), 120, "the objects there at the first page");
    eprintln!(
        "ListEvidence page of 50, with re-verification: median {:?}",
        median(timings)
    );

    // `revision` and `tree_id` together.
    let error = client
        .list_evidence_page(ListEvidenceRequest {
            revision: "@".into(),
            tree_id: trees[0].clone(),
            ..list(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    let error = client
        .list_evidence_page(ListEvidenceRequest {
            tree_id: trees[0].to_uppercase(),
            ..list(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(error.reason(), reef_api::error::INVALID_TREE_ID, "{error}");
    let error = client
        .list_evidence_page(ListEvidenceRequest {
            change_id: "abc".into(),
            ..list(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(
        error.reason(),
        reef_api::error::INVALID_CHANGE_ID,
        "{error}"
    );
    // Review F7: a short key id is refused, not matched against nothing.
    let error = client
        .list_evidence_page(ListEvidenceRequest {
            key_ids: vec![keys[0].public_key().id().as_str()[..12].to_owned()],
            ..list(&ws)
        })
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::InvalidArgument, "{error}");
    assert!(
        error.advice().unwrap().hint.contains("full key id"),
        "{error}"
    );

    // `-r @` snapshots first, unless asked not to.
    write(&ws, "new.txt", "new\n");
    let skipped = listed(
        client,
        ListEvidenceRequest {
            revision: "@".into(),
            skip_snapshot: true,
            ..list(&ws)
        },
    )
    .await;
    assert!(!skipped.is_empty(), "the last snapshot's tree has evidence");
    let snapshotted = listed(
        client,
        ListEvidenceRequest {
            revision: "@".into(),
            ..list(&ws)
        },
    )
    .await;
    assert_eq!(ids(&snapshotted), BTreeSet::new(), "the new tree has none");
    assert_ne!(change(client, &ws, "@").await.tree_id, at.tree_id);
    daemon.stop().await;
}

// 13
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "an envelope, then columns, then the repair"
)]
async fn tampering_shows_invalid() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let signer = key(1);
    let mut added = Vec::new();
    let mut envelopes = Vec::new();
    for kind in ["test", "lint", "build"] {
        let bytes = envelope(&claim(kind, Outcome::Pass, None), &at.tree_id, &signer);
        let response = add(client, &ws, bytes.clone(), &signer, "").await;
        added.push(response.evidence.unwrap().evidence_id);
        envelopes.push(bytes);
    }
    // Flip a signature byte behind the daemon.
    let path = daemon.state_dir().join("reef.sqlite3");
    let conn = rusqlite::Connection::open(&path).unwrap();
    let stored: Vec<u8> = conn
        .query_row(
            "SELECT envelope FROM evidence WHERE id = ?1",
            [&added[0]],
            |row| row.get(0),
        )
        .unwrap();
    let mut value: Value = serde_json::from_slice(&stored).unwrap();
    let mut sig = STANDARD
        .decode(value["signatures"][0]["sig"].as_str().unwrap())
        .unwrap();
    sig[10] ^= 0x01;
    value["signatures"][0]["sig"] = json!(STANDARD.encode(sig));
    conn.execute(
        "UPDATE evidence SET envelope = ?1 WHERE id = ?2",
        rusqlite::params![serde_json::to_vec(&value).unwrap(), added[0]],
    )
    .unwrap();
    drop(conn);

    let got = client.get_evidence(&repo.id, &added[0]).await.unwrap();
    let got = got.evidence.unwrap();
    assert_eq!(got.signature(), SignatureCheck::Invalid);
    assert_eq!(got.kind, "test", "the row still reads");
    for id in &added[1..] {
        let got = client.get_evidence(&repo.id, id).await.unwrap();
        assert_eq!(got.evidence.unwrap().signature(), SignatureCheck::Valid);
    }
    for evidence in listed(client, list(&ws)).await {
        let expected = if evidence.evidence_id == added[0] {
            SignatureCheck::Invalid
        } else {
            SignatureCheck::Valid
        };
        assert_eq!(evidence.signature(), expected, "{}", evidence.evidence_id);
    }

    // Review F1: change the index columns, not the envelope. Reads report
    // the payload's values, invalid, and a filter on the changed columns
    // does not present the row as matching.
    let parent = change(client, &ws, "@-").await;
    let conn = rusqlite::Connection::open(&path).unwrap();
    conn.execute(
        "UPDATE evidence SET result = 'fail', kind = 'build', tree_id = ?1 WHERE id = ?2",
        rusqlite::params![parent.tree_id, added[1]],
    )
    .unwrap();
    // Review F6: a key id that names no key, and times that are not
    // integers, still read, as invalid.
    conn.pragma_update(None, "foreign_keys", false).unwrap();
    conn.execute(
        "UPDATE evidence SET key_id = ?1, started_at = 'soon' WHERE id = ?2",
        rusqlite::params!["0".repeat(64), added[2]],
    )
    .unwrap();
    drop(conn);
    let got = client.get_evidence(&repo.id, &added[1]).await.unwrap();
    let got = got.evidence.unwrap();
    assert_eq!(got.signature(), SignatureCheck::Invalid);
    assert_eq!(got.kind, "lint", "the payload's kind");
    assert_eq!(got.result(), EvidenceResult::Pass, "the payload's result");
    assert_eq!(got.tree_id, at.tree_id, "the payload's tree");
    let got = client.get_evidence(&repo.id, &added[2]).await.unwrap();
    let got = got.evidence.unwrap();
    assert_eq!(got.signature(), SignatureCheck::Invalid);
    assert_eq!(got.started_at.unwrap().seconds, 1_793_872_800);
    assert_eq!(listed(client, list(&ws)).await.len(), 3, "nothing vanishes");
    let builds = listed(
        client,
        ListEvidenceRequest {
            kinds: vec!["build".into()],
            ..list(&ws)
        },
    )
    .await;
    assert_eq!(
        ids(&builds),
        [added[2].clone()].into(),
        "not the changed lint"
    );
    for request in [
        ListEvidenceRequest {
            tree_id: parent.tree_id.clone(),
            ..list(&ws)
        },
        ListEvidenceRequest {
            results: vec![i32::from(EvidenceResult::Fail)],
            ..list(&ws)
        },
    ] {
        assert_eq!(ids(&listed(client, request).await), BTreeSet::new());
    }

    // Re-adding the genuine envelopes repairs the rows.
    for bytes in &envelopes {
        let again = add(client, &ws, bytes.clone(), &signer, "").await;
        assert!(!again.created);
        assert_eq!(again.evidence.unwrap().signature(), SignatureCheck::Valid);
    }
    for evidence in listed(client, list(&ws)).await {
        assert_eq!(evidence.signature(), SignatureCheck::Valid, "{evidence:?}");
    }
    daemon.stop().await;
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn survives_gc_backup_and_forget() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let c = edit(client, &ws, "unique.txt", "only here\n").await;
    let signer = key(1);
    let id = add(
        client,
        &ws,
        envelope(&claim("test", Outcome::Pass, None), &c.tree_id, &signer),
        &signer,
        &c.commit_id,
    )
    .await
    .evidence
    .unwrap()
    .evidence_id;
    // Abandon the change and collect garbage: its tree may leave the store.
    client
        .abandon_change(AbandonChangeRequest {
            workspace: ws.id.clone(),
            revision: c.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    client.collect_garbage(&repo.id, false).await.unwrap();
    let got = client.get_evidence(&repo.id, &id).await.unwrap();
    assert_eq!(got.evidence.unwrap().signature(), SignatureCheck::Valid);
    let by_tree = listed(
        client,
        ListEvidenceRequest {
            tree_id: c.tree_id.clone(),
            ..list(&ws)
        },
    )
    .await;
    assert_eq!(ids(&by_tree), [id.clone()].into());

    // A backup holds the three tables; restored elsewhere, so do they.
    let dest = daemon.tmp.path().join("backup");
    let mut stream = client.backup(&dest, &repo.id).await.unwrap();
    while stream.message().await.unwrap().is_some() {}
    let elsewhere = daemon.tmp.path().join("elsewhere");
    let root = daemon.tmp.path().join("root");
    std::fs::create_dir(&root).unwrap();
    let options = reef_daemon::maintenance::restore::RestoreOptions {
        into: elsewhere.clone(),
        repo: Some(repo.id.parse().unwrap()),
        root: Some(root.clone()),
        force: false,
        lease: daemon.config.file.lease.clone(),
        settings: test_settings(),
    };
    let from = dest.clone();
    tokio::task::spawn_blocking(move || {
        reef_daemon::maintenance::restore::restore(&from, &options)
    })
    .await
    .unwrap()
    .unwrap();
    let restored = reef_daemon::db::Db::open(&elsewhere.join("reef.sqlite3")).unwrap();
    assert_eq!(restored.evidence_counts().unwrap(), (1, 1, 1));
    drop(restored);

    // Forget deletes this repository's evidence and links, keeps the keys,
    // and leaves another repository's alone.
    let other_root = daemon.tmp.path().join("other");
    reef_daemon::testing::init_synthetic_repo(&other_root, Preset::Small);
    let other = client
        .register_repository(&other_root, v1::IfNotJj::InitAlongsideGit)
        .await
        .unwrap()
        .repository
        .unwrap();
    let other_ws = create(client, &other.id).await;
    let other_change = change(client, &other_ws, "@").await;
    add(
        client,
        &other_ws,
        envelope(
            &claim("test", Outcome::Pass, None),
            &other_change.tree_id,
            &signer,
        ),
        &signer,
        "@",
    )
    .await;
    assert_eq!(counts(&daemon), (2, 2, 1));
    client
        .release_workspace(&repo.id, &ws.id, false, true)
        .await
        .unwrap();
    client.forget_repository(&repo.id).await.unwrap();
    assert_eq!(counts(&daemon), (1, 1, 1), "the key stays");
    let kept = listed(client, list(&other_ws)).await;
    assert_eq!(kept.len(), 1);
    assert_eq!(kept[0].change_ids, [other_change.change_id]);
    daemon.stop().await;
}

fn remote_on(config: &mut Config) {
    config.file.remote.enabled = true;
    "127.0.0.1:0".clone_into(&mut config.file.remote.listen);
    config.file.remote.names = vec!["localhost".to_owned()];
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

async fn remote_client(daemon: &TestDaemon, name: &str, scope: RemoteScope) -> (Client, String) {
    let addr = listening(daemon).await;
    let status = daemon.client.daemon_status(true).await.unwrap();
    let fingerprint = status.remote.unwrap().certificate.unwrap().fingerprint;
    let created = daemon
        .client
        .create_remote_token(name, scope, Some(Duration::from_hours(1)))
        .await
        .unwrap();
    let token_id = created.token.unwrap().id;
    let target = RemoteTarget {
        name: None,
        url: RemoteUrl {
            host: addr.ip().to_string(),
            port: addr.port(),
        },
        fingerprint: Some(fingerprint),
        token: Some(Token::new(&created.secret)),
    };
    (Client::connect_remote(&target).await.unwrap(), token_id)
}

/// A log sink for [`remote_and_access`]'s audit line.
#[derive(Clone, Default)]
struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

impl std::io::Write for Sink {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.0.lock().unwrap().extend_from_slice(buf);
        Ok(buf.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for Sink {
    type Writer = Sink;

    fn make_writer(&'a self) -> Sink {
        self.clone()
    }
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn remote_and_access() {
    let sink = Sink::default();
    tracing_subscriber::fmt()
        .with_writer(sink.clone())
        .with_max_level(tracing::Level::INFO)
        .with_ansi(false)
        .init();
    let (daemon, repo) = fixture_with(remote_on).await;
    let ws = create(&daemon.client, &repo.id).await;
    let at = edit(&daemon.client, &ws, "a.txt", "a\n").await;
    let signer = key(1);
    let (writer, writer_id) = remote_client(&daemon, "writer", RemoteScope::Write).await;
    let added = add(
        &writer,
        &ws,
        envelope(&claim("test", Outcome::Pass, None), &at.tree_id, &signer),
        &signer,
        "@",
    )
    .await
    .evidence
    .unwrap();
    assert_eq!(added.remote_token_id, writer_id);

    let (reader, _) = remote_client(&daemon, "reader", RemoteScope::Read).await;
    let error = reader
        .add_evidence(add_request(
            &ws,
            envelope(&claim("lint", Outcome::Pass, None), &at.tree_id, &signer),
            &signer,
            "",
        ))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{error}");
    let listed = reader
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap();
    assert_eq!(listed.items.len(), 1);
    let got = reader
        .get_evidence(&repo.id, &added.evidence_id)
        .await
        .unwrap();
    assert_eq!(got.evidence.unwrap().remote_token_id, writer_id);
    daemon.stop().await;

    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    let audit: Vec<&str> = text
        .lines()
        .filter(|line| line.contains("reef_daemon::remote::audit"))
        .collect();
    assert!(
        audit
            .iter()
            .any(|line| line.contains("remote write") && line.contains("AddEvidence")),
        "{audit:#?}"
    );
}
