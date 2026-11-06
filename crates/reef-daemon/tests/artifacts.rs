//! `reef.api.v1.ArtifactService` and GC's evidence and artifacts step
//! through the daemon (R3.02, tests 3 to 15): the upload protocol, records
//! and states, streamed reads, damage, the evidence index, retention's
//! pins, sweep, cap, tidy and floor, the lock against GC, forget, backup and
//! restore, and the remote and web listeners' rules; and the review's
//! cases: the upload grace, size damage, forget racing an upload, and dedup
//! before the size limit.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use futures::FutureExt as _;
use reef_api::v1::{
    self, AbandonChangeRequest, AddEvidenceRequest, ArtifactState, CreateWorkspaceRequest,
    EditFilesRequest, FileEdit, GetChangeRequest, ListEvidenceRequest, RemoteScope,
    UploadArtifactRequest, UploadArtifactResponse, Workspace, edit_files_request, file_edit,
};
use reef_daemon::artifacts::{Holder, LockHook};
use reef_daemon::client::remote::{RemoteTarget, RemoteUrl, Token};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::config::{ByteSize, Config};
use reef_daemon::testing::{SyntheticRepo, TestDaemon, no_automatic_snapshots, test_settings};
use reef_evidence::{Artifact, Claim, Outcome, SigningKey, TreeDigest};
use reef_lib::error::{Classify as _, ErrorKind};
use reef_testing::synth::Preset;
use serde_json::Map;

const OWNER: &str = "tester@host";
const MIB: usize = 1 << 20;
const DAY: Duration = Duration::from_hours(24);

async fn fixture_with(configure: impl FnOnce(&mut Config)) -> (TestDaemon, SyntheticRepo) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_env("TEST_LOG"))
        .with_test_writer()
        .try_init();
    let (daemon, repo) = TestDaemon::with_synthetic_repo_and(Preset::Small, |config| {
        no_automatic_snapshots(config);
        config.file.history.enabled = false;
        config.file.symbols.enabled = false;
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

/// `len` bytes that depend on `seed`.
fn bytes(len: usize, seed: u64) -> Vec<u8> {
    let mut state = seed.wrapping_mul(0x9e37_79b9_7f4a_7c15) | 1;
    (0..len)
        .map(|_| {
            state ^= state << 13;
            state ^= state >> 7;
            state ^= state << 17;
            state.to_le_bytes()[0]
        })
        .collect()
}

fn sha256(bytes: &[u8]) -> String {
    reef_evidence::sha256_hex(bytes)
}

fn size_of(bytes: &[u8]) -> u64 {
    u64::try_from(bytes.len()).unwrap()
}

fn chunk(repo: &str, sha: &str, size: u64, offset: u64, data: &[u8]) -> UploadArtifactRequest {
    UploadArtifactRequest {
        repo: repo.to_owned(),
        sha256: sha.to_owned(),
        size,
        offset,
        data: data.to_vec(),
    }
}

/// Uploads `data` in 1 MiB chunks from the probe's `received`; the last
/// response.
async fn upload(client: &Client, repo: &str, data: &[u8]) -> UploadArtifactResponse {
    let (sha, size) = (sha256(data), size_of(data));
    let mut response = client
        .upload_artifact(chunk(repo, &sha, size, 0, &[]))
        .await
        .unwrap();
    while response.artifact.is_none() {
        let at = usize::try_from(response.received).unwrap();
        let end = (at + MIB).min(data.len());
        response = client
            .upload_artifact(chunk(repo, &sha, size, response.received, &data[at..end]))
            .await
            .unwrap();
    }
    response
}

fn reason(error: &ClientError) -> &str {
    error.reason()
}

fn meta<'a>(error: &'a ClientError, key: &str) -> &'a str {
    error.metadata().get(key).map_or("", String::as_str)
}

/// A timestamp as unix milliseconds.
fn ms(at: &prost_types::Timestamp) -> i64 {
    at.seconds * 1000 + i64::from(at.nanos / 1_000_000)
}

fn median(mut samples: Vec<Duration>) -> Duration {
    samples.sort();
    samples[samples.len() / 2]
}

/// The repository's artifacts directory.
fn store_dir(daemon: &TestDaemon, repo: &SyntheticRepo) -> PathBuf {
    daemon
        .handle
        .state()
        .artifacts
        .dir(repo.id.parse().unwrap())
}

fn stored_path(daemon: &TestDaemon, repo: &SyntheticRepo, sha: &str) -> PathBuf {
    store_dir(daemon, repo)
        .join("sha256")
        .join(&sha[..2])
        .join(&sha[2..])
}

/// Every message of a read, or the error that ended it with the bytes
/// before it.
async fn read(
    client: &Client,
    repo: &str,
    sha: &str,
    offset: u64,
    length: u64,
) -> Result<(Vec<v1::ReadArtifactResponse>, Option<ClientError>), ClientError> {
    let mut stream = client.read_artifact(repo, sha, offset, length).await?;
    let mut messages = Vec::new();
    loop {
        match stream.message().await {
            Ok(Some(message)) => messages.push(message),
            Ok(None) => return Ok((messages, None)),
            Err(status) => return Ok((messages, Some(ClientError::from(status)))),
        }
    }
}

fn joined(messages: &[v1::ReadArtifactResponse]) -> Vec<u8> {
    messages.iter().flat_map(|m| m.data.clone()).collect()
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

async fn edit(client: &Client, ws: &Workspace, path: &str, contents: &str) -> v1::Change {
    std::fs::write(Path::new(&ws.path).join(path), contents).unwrap();
    client.snapshot_workspace("", &ws.id, "").await.unwrap();
    change(client, ws, "@").await
}

/// A new commit on `on` writing `path`, outside any workspace.
async fn commit_on(client: &Client, repo: &str, on: &str, path: &str, text: &str) -> v1::Change {
    client
        .edit_files(EditFilesRequest {
            repo: repo.to_owned(),
            target: Some(edit_files_request::Target::NewOn(on.to_owned())),
            edits: vec![FileEdit {
                path: path.to_owned(),
                action: Some(file_edit::Action::Content(text.as_bytes().to_vec())),
                executable: None,
            }],
            ..EditFilesRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap()
}

fn key() -> SigningKey {
    SigningKey::from_seed(&[3; 32]).unwrap()
}

/// A claim naming `artifacts` (digests), its target `target`.
fn claim(target: &str, artifacts: &[&str]) -> Claim {
    let at = |seconds| UNIX_EPOCH + Duration::from_secs(seconds);
    Claim {
        kind: "test".to_owned(),
        result: Outcome::Pass,
        target: Some(target.to_owned()),
        started_on: at(1_793_872_800),
        finished_on: at(1_793_872_872),
        artifacts: artifacts
            .iter()
            .enumerate()
            .map(|(n, sha)| Artifact {
                name: format!("a{n}.log"),
                sha256: (*sha).to_owned(),
                uri: None,
                media_type: None,
            })
            .collect(),
        annotations: Map::new(),
    }
}

/// Adds evidence about `tree` naming `artifacts`, linked to `revision`
/// (empty for none); its id.
async fn add(
    client: &Client,
    repo: &str,
    tree: &str,
    revision: &str,
    workspace: &str,
    target: &str,
    artifacts: &[&str],
) -> String {
    let signer = key();
    let envelope = claim(target, artifacts)
        .sign(&TreeDigest::git(tree).unwrap(), &signer)
        .unwrap()
        .to_bytes();
    client
        .add_evidence(AddEvidenceRequest {
            repo: repo.to_owned(),
            workspace: workspace.to_owned(),
            envelope,
            public_key: signer.public_key().spki().to_vec(),
            revision: revision.to_owned(),
        })
        .await
        .unwrap()
        .evidence
        .unwrap()
        .evidence_id
}

/// The database file, for rows changed behind the daemon.
fn sqlite(daemon: &TestDaemon) -> rusqlite::Connection {
    rusqlite::Connection::open(daemon.state_dir().join("reef.sqlite3")).unwrap()
}

/// Unix milliseconds `ago` before now.
fn ms_ago(ago: Duration) -> i64 {
    let at = SystemTime::now() - ago;
    i64::try_from(at.duration_since(UNIX_EPOCH).unwrap().as_millis()).unwrap()
}

fn age_evidence(daemon: &TestDaemon, ids: &[&String], ago: Duration) {
    let conn = sqlite(daemon);
    for id in ids {
        conn.execute(
            "UPDATE evidence SET added_at = ?1 WHERE id = ?2",
            rusqlite::params![ms_ago(ago), id],
        )
        .unwrap();
    }
}

fn age_artifact(daemon: &TestDaemon, sha: &str, ago: Duration) {
    sqlite(daemon)
        .execute(
            "UPDATE artifacts SET used_at = ?1, stored_at = ?1 WHERE sha256 = ?2",
            rusqlite::params![ms_ago(ago), sha],
        )
        .unwrap();
}

async fn gc(client: &Client, repo: &str, dry_run: bool) -> v1::GcReport {
    let mut reports = client.collect_garbage(repo, dry_run).await.unwrap().reports;
    assert_eq!(reports.len(), 1);
    let report = reports.remove(0);
    assert_eq!(report.evidence_error, "", "{report:?}");
    report
}

/// Every artifact and evidence index row, and every file under the store
/// with its contents: what a dry run must leave as it was.
type StoreState = (
    Vec<(String, i64, i64, i64, Option<i64>)>,
    i64,
    Vec<(PathBuf, Vec<u8>)>,
);

fn store_state(daemon: &TestDaemon, repo: &SyntheticRepo) -> StoreState {
    let conn = sqlite(daemon);
    let mut stmt = conn
        .prepare(
            "SELECT sha256, size, stored_at, used_at, collected_at FROM artifacts ORDER BY sha256",
        )
        .unwrap();
    let rows = stmt
        .query_map([], |row| {
            Ok((
                row.get(0)?,
                row.get(1)?,
                row.get(2)?,
                row.get(3)?,
                row.get(4)?,
            ))
        })
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    let evidence: i64 = conn
        .query_row(
            "SELECT (SELECT count(*) FROM evidence) + (SELECT count(*) FROM evidence_artifacts)",
            [],
            |row| row.get(0),
        )
        .unwrap();
    let mut files: Vec<(PathBuf, Vec<u8>)> = walkdir::WalkDir::new(store_dir(daemon, repo))
        .into_iter()
        .filter_map(Result::ok)
        .filter(|entry| entry.file_type().is_file())
        .map(|entry| {
            (
                entry.path().to_owned(),
                std::fs::read(entry.path()).unwrap(),
            )
        })
        .collect();
    files.sort();
    (rows, evidence, files)
}

/// A dry run's report, after asserting it changed nothing.
async fn dry_run(daemon: &TestDaemon, repo: &SyntheticRepo) -> v1::GcReport {
    let before = store_state(daemon, repo);
    let report = gc(&daemon.client, &repo.id, true).await;
    assert!(report.dry_run);
    assert_eq!(
        store_state(daemon, repo),
        before,
        "a dry run changes nothing"
    );
    report
}

/// The counts a dry run and the real run must agree on.
fn counts(report: &v1::GcReport) -> (u32, u32, u64) {
    (
        report.evidence_removed,
        report.artifacts_collected,
        report.artifact_bytes,
    )
}

async fn state_of(client: &Client, repo: &str, sha: &str) -> Result<v1::Artifact, ClientError> {
    Ok(client
        .get_artifact(repo, sha)
        .await?
        .artifact
        .unwrap_or_default())
}

async fn evidence_exists(client: &Client, repo: &str, id: &str) -> bool {
    match client.get_evidence(repo, id).await {
        Ok(_) => true,
        Err(error) if reason(&error) == reef_api::error::EVIDENCE_NOT_FOUND => false,
        Err(error) => panic!("{error}"),
    }
}

// 3
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one protocol, step by step")]
async fn upload_protocol() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.artifacts.max_object_size = ByteSize(8 << 20);
    })
    .await;
    let client = daemon.client.clone().with_agent("uploader").unwrap();
    let data = bytes(5 * MIB, 1);
    let (sha, size) = (sha256(&data), size_of(&data));

    // An empty probe: nothing received.
    let probe = client
        .upload_artifact(chunk(&repo.id, &sha, size, 0, &[]))
        .await
        .unwrap();
    assert_eq!((probe.received, probe.artifact.is_none()), (0, true));

    // 1 MiB chunks; the last stores it.
    let mut times = Vec::new();
    let mut last = probe;
    for n in 0..5 {
        let start = Instant::now();
        last = client
            .upload_artifact(chunk(
                &repo.id,
                &sha,
                size,
                u64::try_from(n * MIB).unwrap(),
                &data[n * MIB..(n + 1) * MIB],
            ))
            .await
            .unwrap();
        times.push(start.elapsed());
        assert_eq!(last.received, u64::try_from((n + 1) * MIB).unwrap());
        assert_eq!(last.artifact.is_some(), n == 4);
    }
    assert!(last.created);
    let stored = last.artifact.unwrap();
    assert_eq!(stored.state(), ArtifactState::Stored);
    assert_eq!((stored.sha256.as_str(), stored.size), (sha.as_str(), size));
    assert_eq!(stored.added_by_agent, "uploader");
    assert_eq!(
        std::fs::read(stored_path(&daemon, &repo, &sha)).unwrap(),
        data
    );
    eprintln!("UploadArtifact median {:?} per 1 MiB chunk", median(times));

    // A probe now: stored, not created, and used again.
    tokio::time::sleep(Duration::from_millis(5)).await;
    let again = client
        .upload_artifact(chunk(&repo.id, &sha, size, 0, &[]))
        .await
        .unwrap();
    assert_eq!((again.received, again.created), (size, false));
    let used = again.artifact.unwrap().used_at.unwrap();
    assert!(ms(&used) > ms(&stored.used_at.unwrap()), "{used:?}");
    // A repeated last chunk (a lost response) finds it stored.
    let repeated = client
        .upload_artifact(chunk(&repo.id, &sha, size, 4 << 20, &data[4 * MIB..]))
        .await
        .unwrap();
    assert_eq!((repeated.received, repeated.created), (size, false));

    // A chunk at the wrong offset, and a repeated middle chunk.
    let other = bytes(2 * MIB + 10, 2);
    let (other_sha, other_size) = (sha256(&other), size_of(&other));
    client
        .upload_artifact(chunk(&repo.id, &other_sha, other_size, 0, &other[..MIB]))
        .await
        .unwrap();
    for offset in [0, 5, u64::try_from(2 * MIB).unwrap()] {
        let at = usize::try_from(offset).unwrap();
        let error = client
            .upload_artifact(chunk(
                &repo.id,
                &other_sha,
                other_size,
                offset,
                &other[at..at + 10],
            ))
            .await
            .unwrap_err();
        assert_eq!(reason(&error), reef_api::error::ARTIFACT_OFFSET_MISMATCH);
        assert_eq!(error.kind(), ErrorKind::Conflict);
        assert_eq!(meta(&error, "received"), MIB.to_string());
    }

    // A restart in the middle: the probe says where to resume.
    let daemon = daemon.restart().await;
    let client = daemon.client.clone();
    let resumed = client
        .upload_artifact(chunk(&repo.id, &other_sha, other_size, 0, &[]))
        .await
        .unwrap();
    assert_eq!(resumed.received, u64::try_from(MIB).unwrap());
    let done = upload(&client, &repo.id, &other).await;
    assert!(done.created);

    // Two clients uploading the same 3 MiB at once store it once.
    let shared = bytes(3 * MIB, 3);
    let file = daemon.tmp.path().join("shared.bin");
    std::fs::write(&file, &shared).unwrap();
    let (a, b) = (
        client.clone(),
        Client::connect(daemon.handle.socket_path()).await.unwrap(),
    );
    let (one, two) = tokio::join!(
        a.upload_artifact_file(&repo.id, &file),
        b.upload_artifact_file(&repo.id, &file)
    );
    let (one, two) = (one.unwrap(), two.unwrap());
    assert_eq!(
        u8::from(one.created) + u8::from(two.created),
        1,
        "one stored it, the other found it stored"
    );
    assert_eq!(one.artifact.sha256, sha256(&shared));
    assert_eq!(
        std::fs::read(stored_path(&daemon, &repo, &sha256(&shared))).unwrap(),
        shared
    );

    // Wrong bytes: refused, and the partial upload is gone.
    let claimed = sha256(b"something else entirely");
    let wrong = bytes(1000, 4);
    let error = client
        .upload_artifact(chunk(&repo.id, &claimed, 1000, 0, &wrong))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_DIGEST_MISMATCH);
    assert_eq!(meta(&error, "actual_sha256"), sha256(&wrong));
    let after = client
        .upload_artifact(chunk(&repo.id, &claimed, 1000, 0, &[]))
        .await
        .unwrap();
    assert_eq!(after.received, 0, "no partial upload is left");

    // Bounds: a chunk over 1 MiB, past the size, and a size over the limit.
    let big = vec![0u8; MIB + 1];
    let error = client
        .upload_artifact(chunk(&repo.id, &sha256(&big), size_of(&big), 0, &big))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::INVALID_ARTIFACT_UPLOAD);
    assert_eq!(meta(&error, "field"), "data");
    let error = client
        .upload_artifact(chunk(&repo.id, &claimed, 5, 0, b"123456"))
        .await
        .unwrap_err();
    assert_eq!(meta(&error, "field"), "size");
    let huge = 9 << 20;
    let error = client
        .upload_artifact(chunk(&repo.id, &claimed, huge, 0, b"x"))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_TOO_LARGE);
    assert_eq!(meta(&error, "max_object_size"), (8u64 << 20).to_string());
    assert!(
        !store_dir(&daemon, &repo)
            .join("incoming")
            .join(format!("{claimed}.{huge}"))
            .exists(),
        "refused before anything is written"
    );
    let error = client
        .upload_artifact(chunk(&repo.id, &sha.to_uppercase(), size, 0, &[]))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::INVALID_ARTIFACT_DIGEST);

    // A zero-byte artifact is one call.
    let empty = client
        .upload_artifact(chunk(&repo.id, &sha256(b""), 0, 0, &[]))
        .await
        .unwrap();
    assert!(empty.created);
    assert_eq!(empty.artifact.unwrap().size, 0);
    daemon.stop().await;
}

// 4
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "each state, by get and by evidence")]
async fn get_and_states() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let stored_bytes = bytes(1000, 5);
    let stored = sha256(&stored_bytes);
    let absent = sha256(b"never uploaded");
    upload(client, &repo.id, &stored_bytes).await;
    let first = add(
        client,
        &repo.id,
        &at.tree_id,
        "",
        &ws.id,
        "t0",
        &[&stored, &absent],
    )
    .await;

    for text in [&stored[..], &stored[..8]] {
        let got = state_of(client, &repo.id, text).await.unwrap();
        assert_eq!(got.state(), ArtifactState::Stored);
        assert_eq!(got.size, 1000);
        assert_eq!(got.evidence_count, 1);
        assert_eq!(got.evidence_ids, std::slice::from_ref(&first));
    }
    let got = state_of(client, &repo.id, &absent).await.unwrap();
    assert_eq!(got.state(), ArtifactState::Absent);
    assert_eq!((got.size, got.evidence_count), (0, 1));
    let error = state_of(client, &repo.id, &absent[..8]).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_NOT_FOUND);
    let error = state_of(client, &repo.id, &sha256(b"nothing names this"))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_NOT_FOUND);
    assert_eq!(error.kind(), ErrorKind::NotFound);
    for bad in ["abc", "xyzw", &"a".repeat(65)] {
        let error = state_of(client, &repo.id, bad).await.unwrap_err();
        assert_eq!(
            reason(&error),
            reef_api::error::INVALID_ARTIFACT_DIGEST,
            "{bad}"
        );
    }

    // The newest 20 of 23 evidence objects naming it.
    let mut ids = vec![first.clone()];
    for n in 1..23 {
        tokio::time::sleep(Duration::from_millis(2)).await;
        ids.push(
            add(
                client,
                &repo.id,
                &at.tree_id,
                "",
                &ws.id,
                &format!("t{n}"),
                &[&stored],
            )
            .await,
        );
    }
    let got = state_of(client, &repo.id, &stored).await.unwrap();
    assert_eq!(got.evidence_count, 23);
    let newest: Vec<String> = ids.iter().rev().take(20).cloned().collect();
    assert_eq!(got.evidence_ids, newest);

    // Evidence reads report each artifact's state from the index.
    let evidence = client
        .get_evidence(&repo.id, &first)
        .await
        .unwrap()
        .evidence
        .unwrap();
    let states: Vec<(ArtifactState, u64)> = evidence
        .artifacts
        .iter()
        .map(|a| (a.state(), a.size))
        .collect();
    assert_eq!(
        states,
        [(ArtifactState::Stored, 1000), (ArtifactState::Absent, 0)]
    );
    // Collected: a tombstone, as retention leaves one.
    sqlite(&daemon)
        .execute(
            "UPDATE artifacts SET collected_at = ?1 WHERE sha256 = ?2",
            rusqlite::params![ms_ago(Duration::ZERO), stored],
        )
        .unwrap();
    let listed = client
        .list_evidence(
            ListEvidenceRequest {
                repo: repo.id.clone(),
                ..ListEvidenceRequest::default()
            },
            0,
        )
        .await
        .unwrap()
        .items;
    assert_eq!(listed.len(), 23);
    for evidence in &listed {
        let artifact = &evidence.artifacts[0];
        assert_eq!(artifact.state(), ArtifactState::Collected);
        assert_eq!(artifact.size, 1000);
        assert!(artifact.collected_at.is_some());
    }
    daemon.stop().await;
}

// 5
#[tokio::test(flavor = "multi_thread")]
async fn read_stream() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let data = bytes(5 * MIB + 123, 6);
    let (sha, size) = (sha256(&data), size_of(&data));
    upload(client, &repo.id, &data).await;

    let start = Instant::now();
    let (messages, error) = read(client, &repo.id, &sha[..10], 0, 0).await.unwrap();
    eprintln!("ReadArtifact of 5 MiB in {:?}", start.elapsed());
    assert!(error.is_none(), "{error:?}");
    assert_eq!(joined(&messages), data);
    assert!(messages.iter().all(|m| m.data.len() <= MIB));
    assert_eq!(messages.len(), 6);
    assert_eq!(messages[0].artifact.as_ref().unwrap().size, size);
    assert!(messages[1..].iter().all(|m| m.artifact.is_none()));
    let mut offset = 0;
    for message in &messages {
        assert_eq!(message.offset, offset);
        offset += size_of(&message.data);
    }

    for (offset, length) in [(1000, 3 << 20), (1000, 0), (size - 10, 100), (0, 1)] {
        let (messages, error) = read(client, &repo.id, &sha, offset, length).await.unwrap();
        assert!(error.is_none());
        let from = usize::try_from(offset).unwrap();
        let to = if length == 0 {
            data.len()
        } else {
            (from + usize::try_from(length).unwrap()).min(data.len())
        };
        assert_eq!(joined(&messages), &data[from..to], "{offset} {length}");
    }
    let (messages, error) = read(client, &repo.id, &sha, size, 0).await.unwrap();
    assert!(error.is_none());
    assert_eq!(messages.len(), 1, "the record, with no data");
    assert!(messages[0].artifact.is_some() && messages[0].data.is_empty());
    let error = read(client, &repo.id, &sha, size + 1, 0).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::INVALID_ARTIFACT_RANGE);
    assert_eq!(meta(&error, "size"), size.to_string());

    // Absent and collected.
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let absent = sha256(b"absent");
    add(client, &repo.id, &at.tree_id, "", &ws.id, "t", &[&absent]).await;
    let error = read(client, &repo.id, &absent, 0, 0).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_NOT_FOUND);
    sqlite(&daemon)
        .execute(
            "UPDATE artifacts SET collected_at = 1793872800000 WHERE sha256 = ?1",
            [&sha],
        )
        .unwrap();
    let error = read(client, &repo.id, &sha, 0, 0).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_COLLECTED);
    assert_eq!(meta(&error, "collected_at"), "2026-11-05T10:00:00.000Z");
    daemon.stop().await;
}

// 6
#[tokio::test(flavor = "multi_thread")]
async fn damaged() {
    use std::os::unix::fs::PermissionsExt as _;
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let data = bytes(2 * MIB + 7, 7);
    let sha = sha256(&data);
    upload(client, &repo.id, &data).await;
    let path = stored_path(&daemon, &repo, &sha);

    // One byte flipped behind the daemon.
    std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();
    let mut changed = data.clone();
    changed[MIB + 3] ^= 1;
    std::fs::write(&path, &changed).unwrap();
    // A ranged read does not check.
    let (messages, error) = read(client, &repo.id, &sha, 1, 0).await.unwrap();
    assert!(error.is_none());
    assert_eq!(joined(&messages), &changed[1..]);
    // A whole read ends in the error, after the data.
    let (messages, error) = read(client, &repo.id, &sha, 0, 0).await.unwrap();
    assert_eq!(joined(&messages), changed);
    let error = error.expect("the damage is reported");
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_DAMAGED);
    assert_eq!(meta(&error, "damage"), "digest");
    assert_eq!(meta(&error, "actual_sha256"), sha256(&changed));
    assert_eq!(error.kind(), ErrorKind::Internal);
    // The damaged bytes were dropped: the artifact reads as collected.
    assert!(!path.exists());
    let got = state_of(client, &repo.id, &sha).await.unwrap();
    assert_eq!(got.state(), ArtifactState::Collected);
    assert!(got.collected_at.is_some());
    let error = read(client, &repo.id, &sha, 0, 0).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_COLLECTED);
    // Uploading the genuine bytes again repairs it.
    let repaired = upload(client, &repo.id, &data).await;
    assert!(repaired.created, "the damaged file did not count as stored");
    let (messages, error) = read(client, &repo.id, &sha, 0, 0).await.unwrap();
    assert!(error.is_none());
    assert_eq!(joined(&messages), data);

    // A deleted file: the error before any data, and an upload repairs it.
    std::fs::remove_file(&path).unwrap();
    let error = read(client, &repo.id, &sha, 0, 0).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_DAMAGED);
    assert_eq!(meta(&error, "damage"), "missing");
    assert!(upload(client, &repo.id, &data).await.created);
    let (messages, error) = read(client, &repo.id, &sha, 0, 0).await.unwrap();
    assert!(error.is_none());
    assert_eq!(joined(&messages), data);
    daemon.stop().await;
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn add_evidence_indexes_artifacts() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let stored_bytes = bytes(100, 8);
    let stored = sha256(&stored_bytes);
    let absent = sha256(b"absent");
    upload(client, &repo.id, &stored_bytes).await;
    age_artifact(&daemon, &stored, 10 * DAY);
    let before = state_of(client, &repo.id, &stored).await.unwrap().used_at;
    let id = add(
        client,
        &repo.id,
        &at.tree_id,
        "",
        &ws.id,
        "t",
        &[&stored, &absent],
    )
    .await;
    let db = &daemon.handle.state().db;
    let repo_id = repo.id.parse().unwrap();
    let mut expected = vec![stored.clone(), absent.clone()];
    expected.sort();
    assert_eq!(db.artifacts_of_evidence(repo_id, &id).unwrap(), expected);
    let after = state_of(client, &repo.id, &stored).await.unwrap().used_at;
    assert!(
        ms(&after.unwrap()) > ms(&before.unwrap()),
        "{after:?} after {before:?}"
    );

    // Rows changed behind the daemon; the genuine re-add rewrites them.
    sqlite(&daemon)
        .execute(
            "UPDATE evidence_artifacts SET sha256 = 'tampered' WHERE evidence_id = ?1 AND sha256 = ?2",
            [&id, &absent],
        )
        .unwrap();
    assert_ne!(db.artifacts_of_evidence(repo_id, &id).unwrap(), expected);
    let again = add(
        client,
        &repo.id,
        &at.tree_id,
        "",
        &ws.id,
        "t",
        &[&stored, &absent],
    )
    .await;
    assert_eq!(again, id);
    assert_eq!(db.artifacts_of_evidence(repo_id, &id).unwrap(), expected);
    daemon.stop().await;
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

/// Sets the remote bookmark `name@origin`, untracked, from outside the
/// daemon, as a fetch would: no local bookmark follows it.
fn set_remote_bookmark(root: &Path, name: &str, commit: &str) {
    use jj_lib::backend::CommitId;
    use jj_lib::default_backend_factories::{
        default_backend_factories, default_working_copy_factories,
    };
    use jj_lib::op_store::{RefTarget, RemoteRef, RemoteRefState};
    use jj_lib::ref_name::{RefName, RemoteName};
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
    tx.repo_mut().set_remote_bookmark(
        RefName::new(name).to_remote_symbol(RemoteName::new("origin")),
        RemoteRef {
            target: RefTarget::normal(CommitId::try_from_hex(commit).unwrap()),
            state: RemoteRefState::New,
        },
    );
    pollster::block_on(tx.commit("remote bookmark set")).unwrap();
}

async fn bookmark(repo: &SyntheticRepo, name: &str, commit: &str) {
    let (root, name, commit) = (repo.root.clone(), name.to_owned(), commit.to_owned());
    tokio::task::spawn_blocking(move || set_bookmark(&root, &name, &commit))
        .await
        .unwrap();
}

// 8
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one fixture covering each pin")]
async fn retention_pins() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = DAY;
        config.file.evidence.retention = DAY;
    })
    .await;
    let client = &daemon.client;
    // Trunk: t1 <- t2 <- t3 = main@origin, with no local `main`, so only
    // `trunk()` reaches t3 (`bookmarks()` is local bookmarks); `release` on
    // t1. The default workspace's working copy is t1's parent's child, the
    // snapshot.
    let ws1 = create(client, &repo.id).await;
    let base = change(client, &ws1, "@-").await;
    let t1 = commit_on(client, &repo.id, &base.commit_id, "trunk.txt", "1\n").await;
    let t2 = commit_on(client, &repo.id, &t1.commit_id, "trunk.txt", "2\n").await;
    let t3 = commit_on(client, &repo.id, &t2.commit_id, "trunk.txt", "3\n").await;
    {
        let (root, commit) = (repo.root.clone(), t3.commit_id.clone());
        tokio::task::spawn_blocking(move || set_remote_bookmark(&root, "main", &commit))
            .await
            .unwrap();
    }
    bookmark(&repo, "release", &t1.commit_id).await;
    // Work: ws1 on a mutable ancestor m1; ws2 linked, then moved on;
    // an abandoned change.
    let m1 = commit_on(client, &repo.id, &t3.commit_id, "m.txt", "m1\n").await;
    client
        .move_workspace(v1::MoveWorkspaceRequest {
            repo: repo.id.clone(),
            workspace: ws1.id.clone(),
            target: Some(v1::move_workspace_request::Target::NewOn(
                m1.commit_id.clone(),
            )),
            ..v1::MoveWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let w1 = edit(client, &ws1, "w1.txt", "w1\n").await;
    let ws2 = create(client, &repo.id).await;
    let w2 = edit(client, &ws2, "w2.txt", "w2 first\n").await;
    let abandoned = commit_on(client, &repo.id, &t3.commit_id, "gone.txt", "gone\n").await;

    let on = |tree: &str| tree.to_owned();
    let mut ids = Vec::new();
    for (target, tree, revision, workspace) in [
        ("trunk", on(&t3.tree_id), "", ""),
        ("bookmark", on(&t1.tree_id), "", ""),
        ("ws1", on(&w1.tree_id), "", ""),
        ("mutable", on(&m1.tree_id), "", ""),
        ("linked", on(&w2.tree_id), "@", ws2.id.as_str()),
        ("abandoned", on(&abandoned.tree_id), "", ""),
        ("old-trunk", on(&t2.tree_id), "", ""),
    ] {
        let artifact = sha256(target.as_bytes());
        ids.push(
            add(
                client,
                &repo.id,
                &tree,
                revision,
                workspace,
                target,
                &[&artifact],
            )
            .await,
        );
    }
    // ws2 moves on: its change's tree is no longer the linked evidence's.
    let w2b = edit(client, &ws2, "w2.txt", "w2 second\n").await;
    assert_eq!(w2b.change_id, w2.change_id);
    assert_ne!(w2b.tree_id, w2.tree_id);
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: abandoned.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    age_evidence(&daemon, &ids.iter().collect::<Vec<_>>(), 30 * DAY);
    let (_, links_before, keys) = daemon.handle.state().db.evidence_counts().unwrap();

    let start = Instant::now();
    let dry = gc(client, &repo.id, true).await;
    eprintln!("GC dry run with the evidence step in {:?}", start.elapsed());
    assert_eq!(dry.evidence_removed, 2, "{dry:?}");
    for id in &ids {
        assert!(
            evidence_exists(client, &repo.id, id).await,
            "a dry run deletes nothing"
        );
    }
    let report = gc(client, &repo.id, false).await;
    assert_eq!(report.evidence_removed, 2);
    let kept: Vec<bool> = {
        let mut kept = Vec::new();
        for id in &ids {
            kept.push(evidence_exists(client, &repo.id, id).await);
        }
        kept
    };
    assert_eq!(kept, [true, true, true, true, true, false, false]);
    let db = &daemon.handle.state().db;
    let (_, links, keys_after) = db.evidence_counts().unwrap();
    assert_eq!(keys_after, keys, "evidence keys stay");
    assert_eq!(
        links, links_before,
        "only the linked evidence had a link, and it stays"
    );
    let repo_id = repo.id.parse().unwrap();
    for id in &ids[5..] {
        assert_eq!(db.artifacts_of_evidence(repo_id, id).unwrap().len(), 0);
    }
    for id in &ids[..5] {
        assert_eq!(db.artifacts_of_evidence(repo_id, id).unwrap().len(), 1);
    }
    daemon.stop().await;
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn retention_artifacts() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = DAY;
        config.file.evidence.retention = 20 * DAY;
        config.file.artifacts.retention = 5 * DAY;
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let live = edit(client, &ws, "live.txt", "live\n").await;
    let gone = commit_on(client, &repo.id, &live.commit_id, "dead.txt", "dead\n").await;
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: gone.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let blobs: Vec<Vec<u8>> = (0..5)
        .map(|n| bytes(100 + n, 20 + u64::try_from(n).unwrap()))
        .collect();
    let shas: Vec<String> = blobs.iter().map(|b| sha256(b)).collect();
    for blob in &blobs {
        upload(client, &repo.id, blob).await;
    }
    let [unreferenced, unpinned, pinned, recent, tombstoned] = [0, 1, 2, 3, 4].map(|n| &shas[n]);
    // Unpinned evidence names two; pinned evidence names one; one is used
    // within the undo window by unpinned evidence.
    let unpinned_evidence = add(
        client,
        &repo.id,
        &gone.tree_id,
        "",
        "",
        "u",
        &[unpinned, recent, tombstoned],
    )
    .await;
    add(client, &repo.id, &live.tree_id, "", "", "p", &[pinned]).await;
    for sha in [unreferenced, unpinned, pinned, tombstoned] {
        age_artifact(&daemon, sha, 10 * DAY);
    }
    age_artifact(&daemon, recent, Duration::from_hours(1));
    age_evidence(&daemon, &[&unpinned_evidence], 10 * DAY);

    let dry = dry_run(&daemon, &repo).await;
    let report = gc(client, &repo.id, false).await;
    assert_eq!(counts(&dry), counts(&report), "{dry:?} {report:?}");
    assert_eq!(report.evidence_removed, 0, "within [evidence] retention");
    assert_eq!(report.artifacts_collected, 3, "{report:?}");
    let error = state_of(client, &repo.id, unreferenced).await.unwrap_err();
    assert_eq!(
        reason(&error),
        reef_api::error::ARTIFACT_NOT_FOUND,
        "file and row went"
    );
    assert!(!stored_path(&daemon, &repo, unreferenced).exists());
    for sha in [unpinned, tombstoned] {
        let got = state_of(client, &repo.id, sha).await.unwrap();
        assert_eq!(got.state(), ArtifactState::Collected);
        assert!(!stored_path(&daemon, &repo, sha).exists());
        let error = read(client, &repo.id, sha, 0, 0).await.unwrap_err();
        assert_eq!(reason(&error), reef_api::error::ARTIFACT_COLLECTED);
    }
    for sha in [pinned, recent] {
        assert_eq!(
            state_of(client, &repo.id, sha).await.unwrap().state(),
            ArtifactState::Stored
        );
    }
    // Uploading collected bytes stores them again.
    let again = upload(client, &repo.id, &blobs[4]).await;
    assert!(again.created);
    assert_eq!(
        state_of(client, &repo.id, tombstoned)
            .await
            .unwrap()
            .state(),
        ArtifactState::Stored
    );

    // The tombstone goes once its evidence does.
    age_evidence(&daemon, &[&unpinned_evidence], 30 * DAY);
    let dry = dry_run(&daemon, &repo).await;
    let report = gc(client, &repo.id, false).await;
    assert_eq!(counts(&dry), counts(&report), "{dry:?} {report:?}");
    assert_eq!(report.evidence_removed, 1);
    let got = state_of(client, &repo.id, unpinned).await.unwrap_err();
    assert_eq!(reason(&got), reef_api::error::ARTIFACT_NOT_FOUND);
    // The re-stored one is now named by nothing, and used just now: kept.
    assert_eq!(
        state_of(client, &repo.id, tombstoned)
            .await
            .unwrap()
            .state(),
        ArtifactState::Stored
    );
    daemon.stop().await;
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn retention_cap() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = DAY;
        config.file.artifacts.retention = Duration::ZERO;
        config.file.artifacts.max_size = ByteSize(64 << 20);
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let live = edit(client, &ws, "live.txt", "live\n").await;
    let dead = commit_on(client, &repo.id, &live.commit_id, "dead.txt", "dead\n").await;
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: dead.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    // Ten 10 MiB artifacts named by unpinned evidence, oldest first.
    let mut shas = Vec::new();
    for n in 0..10u64 {
        let blob = bytes(10 * MIB, 100 + n);
        upload(client, &repo.id, &blob).await;
        shas.push(sha256(&blob));
    }
    let names: Vec<&str> = shas.iter().map(String::as_str).collect();
    add(client, &repo.id, &dead.tree_id, "", "", "u", &names).await;
    for (n, sha) in shas.iter().enumerate() {
        age_artifact(&daemon, sha, (20 - u32::try_from(n).unwrap()) * DAY);
    }
    // The newest is recent: never collected.
    age_artifact(&daemon, &shas[9], Duration::from_hours(1));
    let dry = dry_run(&daemon, &repo).await;
    let report = gc(client, &repo.id, false).await;
    assert_eq!(counts(&dry), counts(&report), "{dry:?} {report:?}");
    assert_eq!(report.artifacts_collected, 4, "{report:?}");
    assert_eq!(report.artifact_bytes, 60 << 20);
    for (n, sha) in shas.iter().enumerate() {
        let state = state_of(client, &repo.id, sha).await.unwrap().state();
        let expected = if n < 4 {
            ArtifactState::Collected
        } else {
            ArtifactState::Stored
        };
        assert_eq!(state, expected, "{n}: least recently used first");
    }

    // Pinned bytes past the cap stay, and the report says so.
    let mut pinned = Vec::new();
    for n in 0..8u64 {
        let blob = bytes(10 * MIB, 200 + n);
        upload(client, &repo.id, &blob).await;
        pinned.push(sha256(&blob));
    }
    let names: Vec<&str> = pinned.iter().map(String::as_str).collect();
    add(client, &repo.id, &live.tree_id, "", "", "p", &names).await;
    for sha in &pinned {
        age_artifact(&daemon, sha, 30 * DAY);
    }
    let dry = dry_run(&daemon, &repo).await;
    let report = gc(client, &repo.id, false).await;
    assert_eq!(counts(&dry), counts(&report), "{dry:?} {report:?}");
    for sha in &pinned {
        assert_eq!(
            state_of(client, &repo.id, sha).await.unwrap().state(),
            ArtifactState::Stored
        );
    }
    assert_eq!(
        report.artifact_bytes,
        (80 << 20) + (10 << 20),
        "pinned 80 MiB and the recent one"
    );
    daemon.stop().await;
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn retention_tidy() {
    let (daemon, repo) = fixture_with(|config| config.file.gc.undo_window = DAY).await;
    let client = &daemon.client;
    let dir = store_dir(&daemon, &repo);
    let set_mtime = |path: &Path, ago: Duration| {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(SystemTime::now() - ago)
            .unwrap();
    };
    std::fs::create_dir_all(dir.join("incoming")).unwrap();
    let old_partial = dir.join("incoming").join(format!("{}.10", sha256(b"old")));
    let new_partial = dir.join("incoming").join(format!("{}.10", sha256(b"new")));
    for path in [&old_partial, &new_partial] {
        std::fs::write(path, b"part").unwrap();
    }
    set_mtime(&old_partial, 2 * DAY);
    set_mtime(&new_partial, Duration::from_hours(1));
    // Stored files with no row: an orphan of 2 hours, one of a minute.
    let orphan = |text: &[u8], ago: Duration| {
        let sha = sha256(text);
        let path = dir.join("sha256").join(&sha[..2]).join(&sha[2..]);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, text).unwrap();
        set_mtime(&path, ago);
        path
    };
    let old_orphan = orphan(b"old orphan", Duration::from_hours(2));
    let new_orphan = orphan(b"new orphan", Duration::from_mins(1));
    // A stored row with no file.
    let lost = bytes(50, 30);
    upload(client, &repo.id, &lost).await;
    std::fs::remove_file(stored_path(&daemon, &repo, &sha256(&lost))).unwrap();

    gc(client, &repo.id, false).await;
    assert!(!old_partial.exists());
    assert!(new_partial.exists());
    assert!(!old_orphan.exists());
    assert!(new_orphan.exists());
    let got = state_of(client, &repo.id, &sha256(&lost)).await.unwrap();
    assert_eq!(got.state(), ArtifactState::Collected, "a tombstone, logged");
    let error = read(client, &repo.id, &sha256(&lost), 0, 0)
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_COLLECTED);
    // Uploading the bytes stores them again.
    assert!(upload(client, &repo.id, &lost).await.created);
    daemon.stop().await;
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn retention_floor() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = DAY;
        config.file.backup.retention = 30 * DAY;
        config.file.evidence.retention = DAY;
        config.file.artifacts.retention = DAY;
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let live = edit(client, &ws, "live.txt", "live\n").await;
    let dead = commit_on(client, &repo.id, &live.commit_id, "dead.txt", "dead\n").await;
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: dead.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let named = bytes(10, 40);
    let unnamed = bytes(11, 41);
    upload(client, &repo.id, &named).await;
    upload(client, &repo.id, &unnamed).await;
    let evidence = add(
        client,
        &repo.id,
        &dead.tree_id,
        "",
        "",
        "u",
        &[&sha256(&named)],
    )
    .await;
    for sha in [sha256(&named), sha256(&unnamed)] {
        age_artifact(&daemon, &sha, 20 * DAY);
    }
    age_evidence(&daemon, &[&evidence], 20 * DAY);
    // A backup 5 days ago widens the floor to 30 days.
    sqlite(&daemon)
        .execute(
            "INSERT INTO meta(key, value) VALUES ('last_backup_at', ?1)
             ON CONFLICT(key) DO UPDATE SET value = excluded.value",
            [ms_ago(5 * DAY).to_string()],
        )
        .unwrap();
    let report = gc(client, &repo.id, false).await;
    assert_eq!(
        (report.evidence_removed, report.artifacts_collected),
        (0, 0),
        "{report:?}"
    );
    assert!(evidence_exists(client, &repo.id, &evidence).await);

    // Past the floor, retention applies; `0s` keeps everything by age.
    sqlite(&daemon)
        .execute("DELETE FROM meta WHERE key = 'last_backup_at'", [])
        .unwrap();
    let daemon = {
        let mut stopped = daemon.stop_keeping().await;
        stopped.config.file.evidence.retention = Duration::ZERO;
        stopped.config.file.artifacts.retention = Duration::ZERO;
        stopped.start().await
    };
    let client = &daemon.client;
    let report = gc(client, &repo.id, false).await;
    assert_eq!(report.evidence_removed, 0);
    assert_eq!(
        report.artifacts_collected, 1,
        "only what nothing names goes"
    );
    assert_eq!(
        state_of(client, &repo.id, &sha256(&named))
            .await
            .unwrap()
            .state(),
        ArtifactState::Stored
    );
    daemon.stop().await;
}

/// A hook that, for `holder`, says it holds the lock and waits for the
/// test's go-ahead.
fn pause_at(
    holder: Holder,
) -> (
    LockHook,
    tokio::sync::mpsc::Receiver<()>,
    Arc<tokio::sync::Notify>,
) {
    let (held, held_rx) = tokio::sync::mpsc::channel(1);
    let go = Arc::new(tokio::sync::Notify::new());
    let resume = Arc::clone(&go);
    let hook: LockHook = Arc::new(move |who: Holder| {
        let (held, resume) = (held.clone(), Arc::clone(&resume));
        async move {
            if who == holder {
                let _ = held.send(()).await;
                resume.notified().await;
            }
        }
        .boxed()
    });
    (hook, held_rx, go)
}

// 13
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "three races, each forced")]
async fn gc_lock_and_race() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = DAY;
        config.file.evidence.retention = DAY;
    })
    .await;
    let client = daemon.client.clone();
    let repo_id = repo.id.parse().unwrap();
    let lock = daemon.handle.state().artifacts.lock(repo_id);

    // GC holds the lock while it collects an old unreferenced artifact; a
    // probe meanwhile waits, and then finds it gone rather than stored.
    let blob = bytes(100, 50);
    let sha = sha256(&blob);
    upload(&client, &repo.id, &blob).await;
    age_artifact(&daemon, &sha, 10 * DAY);
    let (hook, mut held, go) = pause_at(Holder::Gc);
    lock.set_hook(Some(hook));
    let gc_task = tokio::spawn({
        let (client, id) = (client.clone(), repo.id.clone());
        async move { gc(&client, &id, false).await }
    });
    held.recv().await.unwrap();
    let probe = tokio::spawn({
        let (client, id, sha) = (client.clone(), repo.id.clone(), sha.clone());
        async move { client.upload_artifact(chunk(&id, &sha, 100, 0, &[])).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert!(!probe.is_finished(), "the probe waits for GC's collection");
    lock.set_hook(None);
    go.notify_one();
    assert_eq!(gc_task.await.unwrap().artifacts_collected, 1);
    let probe = probe.await.unwrap().unwrap();
    assert_eq!(
        (probe.received, probe.artifact.is_none()),
        (0, true),
        "never `stored`"
    );

    // An upload's check holds it first: it marks the artifact used, and GC
    // then keeps it.
    upload(&client, &repo.id, &blob).await;
    age_artifact(&daemon, &sha, 10 * DAY);
    let (hook, mut held, go) = pause_at(Holder::Upload);
    lock.set_hook(Some(hook));
    let probe = tokio::spawn({
        let (client, id, sha) = (client.clone(), repo.id.clone(), sha.clone());
        async move { client.upload_artifact(chunk(&id, &sha, 100, 0, &[])).await }
    });
    held.recv().await.unwrap();
    lock.set_hook(None);
    let gc_task = tokio::spawn({
        let (client, id) = (client.clone(), repo.id.clone());
        async move { gc(&client, &id, false).await }
    });
    tokio::time::sleep(Duration::from_millis(200)).await;
    go.notify_one();
    let probe = probe.await.unwrap().unwrap();
    assert!(probe.artifact.is_some(), "stored");
    assert_eq!(gc_task.await.unwrap().artifacts_collected, 0);
    assert!(
        stored_path(&daemon, &repo, &sha).exists(),
        "what it was told is stored, is"
    );

    // AddEvidence between the evidence sweep's choice and its deletion,
    // paused there: each object is checked again as it is deleted, so old
    // evidence linked to a live change meanwhile stays, evidence added
    // meanwhile stays, and a plain re-add (which keeps the first add's
    // time) goes as chosen.
    let ws = create(&client, &repo.id).await;
    let first = edit(&client, &ws, "w.txt", "first\n").await;
    let dead = commit_on(&client, &repo.id, &first.commit_id, "dead.txt", "dead\n").await;
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.id.clone(),
            revision: dead.commit_id.clone(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
    let (mut linked, mut plain) = (Vec::new(), Vec::new());
    for n in 0..5 {
        let target = format!("l{n}");
        linked.push(add(&client, &repo.id, &first.tree_id, "", "", &target, &[]).await);
        let target = format!("p{n}");
        plain.push(add(&client, &repo.id, &dead.tree_id, "", "", &target, &[]).await);
    }
    // The workspace moves on: `first`'s tree is no longer live, and nothing
    // links the evidence on it to the workspace's change.
    let second = edit(&client, &ws, "w.txt", "second\n").await;
    assert_eq!(second.change_id, first.change_id);
    age_evidence(
        &daemon,
        &linked.iter().chain(&plain).collect::<Vec<_>>(),
        30 * DAY,
    );
    let (hook, mut held, go) = pause_at(Holder::EvidenceSweep);
    lock.set_hook(Some(hook));
    let gc_task = tokio::spawn({
        let (client, id) = (client.clone(), repo.id.clone());
        async move { gc(&client, &id, false).await }
    });
    held.recv().await.unwrap();
    for n in 0..5 {
        // Linked to the workspace's live change, by its earlier commit.
        let target = format!("l{n}");
        let again = add(
            &client,
            &repo.id,
            &first.tree_id,
            &first.commit_id,
            &ws.id,
            &target,
            &[],
        )
        .await;
        assert_eq!(again, linked[n]);
        let target = format!("p{n}");
        assert_eq!(
            add(&client, &repo.id, &dead.tree_id, "", "", &target, &[]).await,
            plain[n]
        );
    }
    let fresh = add(&client, &repo.id, &dead.tree_id, "", "", "fresh", &[]).await;
    lock.set_hook(None);
    go.notify_one();
    let report = gc_task.await.unwrap();
    assert_eq!(report.evidence_removed, 5, "{report:?}");
    for id in linked.iter().chain([&fresh]) {
        assert!(evidence_exists(&client, &repo.id, id).await, "{id} stays");
    }
    for id in &plain {
        assert!(!evidence_exists(&client, &repo.id, id).await, "{id} goes");
    }
    daemon.stop().await;
}

// 14
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "forget, backup, two restores and an old backup"
)]
async fn forget_and_backup() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let at = edit(client, &ws, "a.txt", "a\n").await;
    let blobs: Vec<Vec<u8>> = (0..3)
        .map(|n| bytes(MIB + n, 60 + u64::try_from(n).unwrap()))
        .collect();
    for blob in &blobs {
        upload(client, &repo.id, blob).await;
    }
    let names: Vec<String> = blobs.iter().map(|b| sha256(b)).collect();
    let refs: Vec<&str> = names.iter().map(String::as_str).collect();
    add(client, &repo.id, &at.tree_id, "", &ws.id, "t", &refs).await;
    // A partial upload, which stays out of backups.
    let partial = bytes(2 * MIB, 70);
    client
        .upload_artifact(chunk(
            &repo.id,
            &sha256(&partial),
            size_of(&partial),
            0,
            &partial[..MIB],
        ))
        .await
        .unwrap();

    let dest = daemon.tmp.path().join("backup");
    let mut stream = client.backup(&dest, "").await.unwrap();
    let mut steps = Vec::new();
    while let Some(message) = stream.message().await.unwrap() {
        steps.push((message.step(), message.files));
    }
    assert!(steps.contains(&(v1::BackupStep::Artifacts, 3)), "{steps:?}");
    let backed_up = reef_daemon::maintenance::backup::repo_dir(&dest, repo.id.parse().unwrap())
        .join("artifacts");
    assert!(backed_up.join("sha256").is_dir());
    assert!(!backed_up.join("incoming").exists());
    let manifest = reef_daemon::maintenance::backup::Manifest::read(&dest).unwrap();
    let entry = manifest.repo(&repo.id).unwrap();
    assert_eq!(entry.artifact_files, 3);
    assert_eq!(
        entry.artifact_bytes,
        blobs.iter().map(|b| size_of(b)).sum::<u64>()
    );

    // Restored into an empty state directory, and into a second one, every
    // artifact reads back byte for byte.
    for name in ["empty", "another"] {
        let into = daemon.tmp.path().join(name);
        let root = daemon.tmp.path().join(format!("{name}-root"));
        std::fs::create_dir_all(&root).unwrap();
        let root = Some(root);
        let options = reef_daemon::maintenance::restore::RestoreOptions {
            into: into.clone(),
            repo: Some(repo.id.parse().unwrap()),
            root,
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
        for blob in &blobs {
            let sha = sha256(blob);
            let path = into
                .join("repos")
                .join(&repo.id)
                .join("artifacts/sha256")
                .join(&sha[..2])
                .join(&sha[2..]);
            assert_eq!(std::fs::read(&path).unwrap(), *blob, "{name}");
        }
        let restored = reef_daemon::db::Db::open(&into.join("reef.sqlite3")).unwrap();
        assert_eq!(restored.artifact_counts().unwrap(), (3, 3), "{name}");
    }

    // A backup from before R3.02: no artifacts, no manifest fields.
    let old = daemon.tmp.path().join("old-backup");
    copy_dir(&dest, &old);
    std::fs::remove_dir_all(
        reef_daemon::maintenance::backup::repo_dir(&old, repo.id.parse().unwrap())
            .join("artifacts"),
    )
    .unwrap();
    let text = std::fs::read_to_string(old.join("manifest.json")).unwrap();
    let mut value: serde_json::Value = serde_json::from_str(&text).unwrap();
    for repo in value["repos"].as_array_mut().unwrap() {
        let object = repo.as_object_mut().unwrap();
        object.remove("artifact_files");
        object.remove("artifact_bytes");
    }
    std::fs::write(old.join("manifest.json"), value.to_string()).unwrap();
    let conn = rusqlite::Connection::open(old.join("reef.sqlite3")).unwrap();
    conn.execute("DELETE FROM artifacts", []).unwrap();
    drop(conn);
    let into = daemon.tmp.path().join("from-old");
    let options = reef_daemon::maintenance::restore::RestoreOptions {
        into: into.clone(),
        repo: Some(repo.id.parse().unwrap()),
        root: Some(daemon.tmp.path().join("root-old")),
        force: false,
        lease: daemon.config.file.lease.clone(),
        settings: test_settings(),
    };
    std::fs::create_dir_all(daemon.tmp.path().join("root-old")).unwrap();
    tokio::task::spawn_blocking(move || reef_daemon::maintenance::restore::restore(&old, &options))
        .await
        .unwrap()
        .unwrap();
    assert!(!into.join("repos").join(&repo.id).join("artifacts").exists());
    let restored = reef_daemon::db::Db::open(&into.join("reef.sqlite3")).unwrap();
    assert_eq!(
        restored.artifact_counts().unwrap(),
        (0, 3),
        "its evidence reads absent"
    );
    drop(restored);

    // Forget deletes this repository's rows and files, and leaves another's.
    let other_root = daemon.tmp.path().join("other");
    reef_daemon::testing::init_synthetic_repo(&other_root, Preset::Small);
    let other = client
        .register_repository(&other_root, v1::IfNotJj::InitAlongsideGit)
        .await
        .unwrap()
        .repository
        .unwrap();
    upload(client, &other.id, &blobs[0]).await;
    client
        .release_workspace(&repo.id, &ws.id, false, true)
        .await
        .unwrap();
    client.forget_repository(&repo.id).await.unwrap();
    assert_eq!(daemon.handle.state().db.artifact_counts().unwrap(), (1, 0));
    assert!(!store_dir(&daemon, &repo).exists());
    let kept = state_of(client, &other.id, &sha256(&blobs[0]))
        .await
        .unwrap();
    assert_eq!(kept.state(), ArtifactState::Stored);
    daemon.stop().await;
}

fn copy_dir(from: &Path, to: &Path) {
    reef_daemon::maintenance::backup::copy_tree(from, to, &[]).unwrap();
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

/// gRPC-web over the web listener with a session token.
mod web {
    use hyper_util::client::legacy::Client as HttpClient;
    use hyper_util::client::legacy::connect::HttpConnector;
    use hyper_util::rt::TokioExecutor;
    use reef_api::v1::artifact_service_client::ArtifactServiceClient;
    use tonic::body::Body;
    use tonic::service::Interceptor;
    use tonic::service::interceptor::InterceptedService;
    use tonic::{Request, Status};
    use tonic_web::{GrpcWebClientLayer, GrpcWebClientService};
    use tower::Layer as _;

    #[derive(Clone)]
    pub(super) struct Bearer(String);

    impl Interceptor for Bearer {
        fn call(&mut self, mut request: Request<()>) -> Result<Request<()>, Status> {
            request.metadata_mut().insert(
                "authorization",
                format!("Bearer {}", self.0).parse().unwrap(),
            );
            Ok(request)
        }
    }

    pub(super) type Web = InterceptedService<
        GrpcWebClientService<HttpClient<HttpConnector, tonic_web::GrpcWebCall<Body>>>,
        Bearer,
    >;

    /// The artifact service over the web listener at `url` with `token`.
    pub(super) fn artifacts(url: &str, token: &str) -> ArtifactServiceClient<Web> {
        let client = HttpClient::builder(TokioExecutor::new()).build_http();
        let service = GrpcWebClientLayer::new().layer(client);
        ArtifactServiceClient::with_origin(
            InterceptedService::new(service, Bearer(token.to_owned())),
            url.parse().unwrap(),
        )
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
    let blob = bytes(2 * MIB + 5, 80);
    let sha = sha256(&blob);
    let (writer, writer_id) = remote_client(&daemon, "writer", RemoteScope::Write).await;
    let stored = upload(&writer, &repo.id, &blob).await.artifact.unwrap();
    assert_eq!(stored.remote_token_id, writer_id);

    let (reader, _) = remote_client(&daemon, "reader", RemoteScope::Read).await;
    let error = reader
        .upload_artifact(chunk(&repo.id, &sha256(b"x"), 1, 0, b"x"))
        .await
        .unwrap_err();
    assert_eq!(error.kind(), ErrorKind::PermissionDenied, "{error}");
    let (messages, error) = read(&reader, &repo.id, &sha, 0, 0).await.unwrap();
    assert!(error.is_none());
    assert_eq!(joined(&messages), blob);

    // The web listener serves reads to a session, and refuses uploads.
    let session = daemon.client.create_web_session(false).await.unwrap();
    let token = session
        .session_url
        .split_once("#session=")
        .unwrap()
        .1
        .to_owned();
    let mut web = web::artifacts(&session.url, &token);
    let mut stream = web
        .read_artifact(v1::ReadArtifactRequest {
            repo: repo.id.clone(),
            sha256: sha.clone(),
            offset: 0,
            length: 0,
        })
        .await
        .unwrap()
        .into_inner();
    let mut read_back = Vec::new();
    while let Some(message) = stream.message().await.unwrap() {
        read_back.extend(message.data);
    }
    assert_eq!(read_back, blob);
    let status = web
        .upload_artifact(chunk(&repo.id, &sha, size_of(&blob), 0, &[]))
        .await
        .unwrap_err();
    let detail = reef_daemon::error::detail_of(&status).unwrap();
    assert_eq!(detail.reason, reef_api::error::WEB_ACCESS_DENIED);
    daemon.stop().await;

    let text = String::from_utf8(sink.0.lock().unwrap().clone()).unwrap();
    assert!(
        text.lines()
            .filter(|line| line.contains("reef_daemon::remote::audit"))
            .any(|line| line.contains("remote write") && line.contains("UploadArtifact")),
        "{text}"
    );
}

/// Abandons `revision`, so its tree is no longer live.
async fn abandon(client: &Client, repo: &str, revision: &str) {
    client
        .abandon_change(AbandonChangeRequest {
            repo: repo.to_owned(),
            revision: revision.to_owned(),
            ..AbandonChangeRequest::default()
        })
        .await
        .unwrap();
}

// R3.02 review: with `[gc] undo_window = "0s"` (the fixture's), `keep_newer`
// is now; an upload waiting for its evidence keeps the upload grace, from
// the sweep of what nothing names and from the cap.
#[tokio::test(flavor = "multi_thread")]
async fn upload_grace() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.artifacts.retention = Duration::ZERO;
        config.file.artifacts.max_size = ByteSize(1);
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let live = edit(client, &ws, "live.txt", "live\n").await;
    let dead = commit_on(client, &repo.id, &live.commit_id, "dead.txt", "dead\n").await;
    abandon(client, &repo.id, &dead.commit_id).await;
    let (waiting, named) = (bytes(100, 80), bytes(100, 81));
    upload(client, &repo.id, &waiting).await;
    upload(client, &repo.id, &named).await;
    add(
        client,
        &repo.id,
        &dead.tree_id,
        "",
        "",
        "u",
        &[&sha256(&named)],
    )
    .await;
    let shas = [sha256(&waiting), sha256(&named)];

    let dry = dry_run(&daemon, &repo).await;
    let report = gc(client, &repo.id, false).await;
    assert_eq!(counts(&dry), counts(&report));
    assert_eq!(report.artifacts_collected, 0, "{report:?}");
    for sha in &shas {
        assert_eq!(
            state_of(client, &repo.id, sha).await.unwrap().state(),
            ArtifactState::Stored,
            "within the grace"
        );
    }

    // Past the grace, the unnamed one goes and the cap collects the other.
    for sha in &shas {
        age_artifact(&daemon, sha, Duration::from_hours(25));
    }
    let report = gc(client, &repo.id, false).await;
    assert_eq!(report.artifacts_collected, 2, "{report:?}");
    let error = state_of(client, &repo.id, &shas[0]).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_NOT_FOUND);
    assert_eq!(
        state_of(client, &repo.id, &shas[1]).await.unwrap().state(),
        ArtifactState::Collected
    );
    daemon.stop().await;
}

// R3.02 review: a row whose size disagrees with its file is damage, read
// before any data, and an upload of the genuine bytes repairs it.
#[tokio::test(flavor = "multi_thread")]
async fn size_damage() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let data = bytes(1000, 82);
    let sha = sha256(&data);
    upload(client, &repo.id, &data).await;
    sqlite(&daemon)
        .execute("UPDATE artifacts SET size = 999 WHERE sha256 = ?1", [&sha])
        .unwrap();
    let error = read(client, &repo.id, &sha, 0, 0).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_DAMAGED);
    assert_eq!(meta(&error, "damage"), "size");
    assert_eq!(meta(&error, "actual_size"), "1000");
    let repaired = upload(client, &repo.id, &data).await;
    assert!(repaired.created, "not found stored");
    assert_eq!(repaired.artifact.unwrap().size, 1000);
    let (messages, error) = read(client, &repo.id, &sha, 0, 0).await.unwrap();
    assert!(error.is_none(), "{error:?}");
    assert_eq!(joined(&messages), data);
    daemon.stop().await;
}

// R3.02 review: an upload that resolved its repository before
// `ForgetRepository` removed it stores nothing: its commit, queued behind
// forget on the artifact lock, finds the repository gone.
#[tokio::test(flavor = "multi_thread")]
async fn forget_racing_an_upload() {
    let (daemon, repo) = fixture().await;
    let client = daemon.client.clone();
    let lock = daemon
        .handle
        .state()
        .artifacts
        .lock(repo.id.parse().unwrap());
    let data = bytes(2 * MIB, 83);
    let (sha, size) = (sha256(&data), size_of(&data));
    client
        .upload_artifact(chunk(&repo.id, &sha, size, 0, &data[..MIB]))
        .await
        .unwrap();
    // The test holds the lock: forget queues for it first, then the last
    // chunk, appended and hashed without it.
    let held = lock.acquire(Holder::Gc).await;
    let forget = tokio::spawn({
        let (client, id) = (client.clone(), repo.id.clone());
        async move { client.forget_repository(&id).await }
    });
    tokio::time::sleep(Duration::from_millis(300)).await;
    let last = tokio::spawn({
        let (client, id, sha, rest) = (
            client.clone(),
            repo.id.clone(),
            sha.clone(),
            data[MIB..].to_vec(),
        );
        async move {
            client
                .upload_artifact(chunk(&id, &sha, size, u64::try_from(MIB).unwrap(), &rest))
                .await
        }
    });
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!forget.is_finished() && !last.is_finished());
    drop(held);
    forget.await.unwrap().unwrap();
    let error = last.await.unwrap().unwrap_err();
    assert_eq!(reason(&error), reef_api::error::REPOSITORY_NOT_FOUND);
    assert_eq!(daemon.handle.state().db.artifact_counts().unwrap(), (0, 0));
    let dir = store_dir(&daemon, &repo);
    assert!(!dir.parent().unwrap().exists(), "nothing recreated it");
    daemon.stop().await;
}

// R3.02 review: the dedup check comes before the size limit, so a stored
// artifact is still found once `max_object_size` is lowered; bytes that
// are not the digest the caller hashed are refused, and nothing is stored.
#[tokio::test(flavor = "multi_thread")]
async fn dedup_before_the_size_limit() {
    let (daemon, repo) = fixture().await;
    let data = bytes(4096, 84);
    let (sha, size) = (sha256(&data), size_of(&data));
    upload(&daemon.client, &repo.id, &data).await;
    let daemon = {
        let mut stopped = daemon.stop_keeping().await;
        stopped.config.file.artifacts.max_object_size = ByteSize(1024);
        stopped.start().await
    };
    let client = &daemon.client;
    let probe = client
        .upload_artifact(chunk(&repo.id, &sha, size, 0, &[]))
        .await
        .unwrap();
    assert_eq!((probe.received, probe.created), (size, false));
    assert!(probe.artifact.is_some());
    let other = bytes(4096, 85);
    let error = client
        .upload_artifact(chunk(&repo.id, &sha256(&other), 4096, 0, &[]))
        .await
        .unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_TOO_LARGE);

    // A file whose bytes are not the digest it is uploaded as.
    let small = bytes(500, 86);
    let file = daemon.tmp.path().join("changed.bin");
    std::fs::write(&file, &small).unwrap();
    let claimed = sha256(b"what was hashed");
    let error = client
        .upload_artifact_file_as(&repo.id, &file, Some(&claimed))
        .await
        .unwrap_err();
    let reef_daemon::client::UploadError::Client(error) = error else {
        panic!("{error:?}");
    };
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_DIGEST_MISMATCH);
    assert_eq!(meta(&error, "actual_sha256"), sha256(&small));
    let error = state_of(client, &repo.id, &claimed).await.unwrap_err();
    assert_eq!(reason(&error), reef_api::error::ARTIFACT_NOT_FOUND);
    daemon.stop().await;
}

// Benchmarks (R3.02 "Performance expectations"): trend data, not gates.
// Run in release, one at a time:
// cargo nextest run --release -p reef-daemon --test artifacts --run-ignored only --success-output immediate
#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark; see docs/benchmarks.md"]
async fn bench_upload_dedup_read() {
    let (daemon, repo) = fixture().await;
    let client = &daemon.client;
    let data = bytes(256 * MIB, 90);
    let sha = sha256(&data);
    let file = daemon.tmp.path().join("big.bin");
    std::fs::write(&file, &data).unwrap();
    drop(data);
    let mib = 256.0_f64;

    let start = Instant::now();
    let uploaded = client.upload_artifact_file(&repo.id, &file).await.unwrap();
    let upload = start.elapsed();
    assert!(uploaded.created);

    let start = Instant::now();
    let hashed = reef_evidence::sha256_hex_of(std::fs::File::open(&file).unwrap()).unwrap();
    let hashing = start.elapsed();
    assert_eq!(hashed, sha);
    let start = Instant::now();
    let again = client.upload_artifact_file(&repo.id, &file).await.unwrap();
    let dedup = start.elapsed();
    assert!(!again.created);
    assert_eq!(again.sent, 0);

    let start = Instant::now();
    let mut stream = client.read_artifact(&repo.id, &sha, 0, 0).await.unwrap();
    let mut read = 0usize;
    while let Some(message) = stream.message().await.unwrap() {
        read += message.data.len();
    }
    let reading = start.elapsed();
    assert_eq!(read, 256 * MIB);
    eprintln!(
        "upload 256 MiB: {upload:?} ({:.0} MiB/s, hash included); dedup: {dedup:?} (hashing alone {hashing:?}, so the probe {:?}); read: {reading:?} ({:.0} MiB/s, re-hashed)",
        mib / upload.as_secs_f64(),
        dedup.saturating_sub(hashing),
        mib / reading.as_secs_f64(),
    );
    daemon.stop().await;
}

/// `count` commits in a chain on `parent`, written with jj-lib from
/// outside the daemon; the last one's id.
fn chain(root: &Path, parent: &str, count: usize) -> String {
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
    let mut tx = repo.start_transaction();
    let store = tx.repo().store().clone();
    let mut head = store
        .get_commit(&CommitId::try_from_hex(parent).unwrap())
        .unwrap();
    for n in 0..count {
        head = pollster::block_on(
            tx.repo_mut()
                .new_commit(vec![head.id().clone()], head.tree())
                .set_description(format!("live {n}"))
                .write(),
        )
        .unwrap();
    }
    pollster::block_on(tx.commit("a chain")).unwrap();
    head.id().hex()
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "a benchmark; see docs/benchmarks.md"]
async fn bench_gc_step() {
    let (daemon, repo) = fixture_with(|config| {
        config.file.gc.undo_window = DAY;
        config.file.evidence.retention = DAY;
        config.file.artifacts.retention = DAY;
    })
    .await;
    let client = &daemon.client;
    let ws = create(client, &repo.id).await;
    let base = change(client, &ws, "@-").await;
    // 2,000 live commits: a mutable chain with no trunk. They share one
    // tree, so evidence is pinned through the links below as well.
    let (root, parent) = (repo.root.clone(), base.commit_id.clone());
    let head = tokio::task::spawn_blocking(move || chain(&root, &parent, 2_000))
        .await
        .unwrap();
    let live_tree = commit_on(client, &repo.id, &head, "live.txt", "live\n").await;
    // 100,000 evidence objects, about half on the live tree and the rest
    // on trees no commit has; 20,000 artifacts, each named by five of them,
    // as files.
    let conn = sqlite(&daemon);
    let repo_id = repo.id.clone();
    let old = ms_ago(100 * DAY);
    conn.execute_batch("BEGIN").unwrap();
    conn.execute(
        "INSERT INTO evidence_keys(id, algorithm, public_key, first_seen_at)
         VALUES ('k', 'ed25519', x'00', 1) ON CONFLICT DO NOTHING",
        [],
    )
    .unwrap();
    let dir = store_dir(&daemon, &repo).join("sha256");
    let mut doomed = 0u32;
    for n in 0..20_000u64 {
        let content = format!("artifact {n}\n");
        let sha = sha256(content.as_bytes());
        let path = dir.join(&sha[..2]).join(&sha[2..]);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, &content).unwrap();
        conn.execute(
            "INSERT INTO artifacts(repo_id, sha256, size, stored_at, used_at)
             VALUES (?1, ?2, ?3, ?4, ?4)",
            rusqlite::params![repo_id, sha, i64::try_from(content.len()).unwrap(), old],
        )
        .unwrap();
        for k in 0..5u64 {
            let id = sha256(format!("evidence {n} {k}").as_bytes());
            // A quarter of the artifacts are named only by evidence that
            // goes; the rest by some that stays.
            let pinned = n % 4 != 0 && (n + k) % 2 == 0;
            if !pinned {
                doomed += 1;
            }
            let tree = if pinned {
                live_tree.tree_id.clone()
            } else {
                sha256(id.as_bytes())[..40].to_owned()
            };
            conn.execute(
                "INSERT INTO evidence(repo_id, id, envelope, format, tree_id, kind, result, key_id,
                                      started_at, finished_at, added_at)
                 VALUES (?1, ?2, x'7b7d', 1, ?3, 'test', 'pass', 'k', 1, 2, ?4)",
                rusqlite::params![repo_id, id, tree, old],
            )
            .unwrap();
            conn.execute(
                "INSERT INTO evidence_artifacts(repo_id, evidence_id, sha256) VALUES (?1, ?2, ?3)",
                rusqlite::params![repo_id, id, sha],
            )
            .unwrap();
        }
    }
    conn.execute_batch("COMMIT").unwrap();
    drop(conn);

    let state = daemon.handle.state();
    let handle = state.repos.get(repo.id.parse().unwrap()).unwrap();
    let head = state.reads.current(&handle).await.unwrap();
    let start = Instant::now();
    let live = state
        .reads
        .run(&handle, move || async move {
            reef_lib::jj::files::live_set(&head).await
        })
        .await
        .unwrap()
        .unwrap();
    let live_ms = start.elapsed();
    assert!(live.commit_count >= 2_000, "{}", live.commit_count);
    let keep_newer = SystemTime::now() - DAY;
    let start = Instant::now();
    let report =
        reef_daemon::artifacts::retention::run(state, repo.id.parse().unwrap(), keep_newer, false)
            .await
            .unwrap();
    let step = start.elapsed();
    assert_eq!(report.evidence_removed, doomed);
    assert_eq!(report.artifacts_collected, 5_000);
    eprintln!(
        "gc step: {step:?} for 100,000 evidence objects, 20,000 artifacts and {} live commits (live_set alone {live_ms:?}); removed {} evidence, collected {} artifacts",
        live.commit_count, report.evidence_removed, report.artifacts_collected
    );
    daemon.stop().await;
}
