//! `reef.api.v1.SymbolService` through the daemon (R2.14, tests 12 to 21):
//! indexing on demand and in the background, sharing entries across trees,
//! SCIP uploads checked against the tree, listings by path, name and
//! symbol, skips, GC, the cache file's lifecycle, the disabled index, and
//! one pass per tree; and R2.16's test 21, name and symbol listings read in
//! path order from SQL.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::path::{Path, PathBuf};
use std::time::Duration;

use prost::Message as _;
use reef_api::v1::{
    self, CreateWorkspaceRequest, EditFilesRequest, FileEdit, IfNotJj, IndexSymbolsRequest,
    ListSymbolsRequest, ScipDocument, ScipTool, SymbolIndexState, SymbolRole, SymbolSource,
    UploadScipDocumentsRequest, edit_files_request, file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::symbols::SymbolIndex;
use reef_daemon::testing::TestDaemon;
use reef_lib::error::{Classify as _, ErrorKind};
use reef_symbols::scip::proto as scip;
use reef_testing::git::{blob, commit_tree, init_repo, raw_tree};

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reef-symbols/tests/fixtures"
);

/// The fixtures of `reef-symbols`' tests 1 to 4, at their paths here.
const FILES: [(&str, &str); 7] = [
    ("src/parser.py", "parser.py"),
    ("web/shapes.ts", "shapes.ts"),
    ("web/view.tsx", "view.tsx"),
    ("web/app.js", "app.js"),
    ("pkg/store/store.go", "pkg/store/store.go"),
    ("pkg/store/close.go", "pkg/store/close.go"),
    ("src/lib.rs", "lib.rs"),
];

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(FIXTURES).join(name)).unwrap()
}

struct Fixture {
    daemon: TestDaemon,
    repo: String,
    root: PathBuf,
    main: String,
}

async fn start(configure: impl FnOnce(&mut reef_daemon::config::Config)) -> Fixture {
    let daemon = TestDaemon::start_with(|config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        configure(config);
    })
    .await;
    let root = daemon.tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let readme = init_repo(&root, &[("README.md", "# fixture\n")]);
    // A symlink with a supported name, never read.
    let target = blob(&root, b"README.md");
    let readme_blob = blob(&root, b"# fixture\n");
    let tree = raw_tree(
        &root,
        &[
            ("100644", b"README.md", &readme_blob),
            ("120000", b"link.py", &target),
        ],
    );
    let main = commit_tree(&root, "refs/heads/main", &tree, Some(&readme));
    let registered = daemon
        .client
        .register_repository(&root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    Fixture {
        repo: registered.repository.unwrap().id,
        root: root.canonicalize().unwrap(),
        daemon,
        main,
    }
}

impl Fixture {
    fn client(&self) -> &Client {
        &self.daemon.client
    }

    fn extracted(&self) -> u64 {
        self.daemon.handle.state().symbols.extracted()
    }

    /// A new commit on `on` writing `files`; its commit id.
    async fn commit(&self, on: &str, files: &[(&str, &[u8])]) -> String {
        self.commit_change(on, files).await.commit_id
    }

    /// A new commit on `on` writing `files`; its change.
    async fn commit_change(&self, on: &str, files: &[(&str, &[u8])]) -> v1::Change {
        self.edit_change(edit_files_request::Target::NewOn(on.to_owned()), files)
            .await
    }

    async fn edit(&self, target: edit_files_request::Target, files: &[(&str, &[u8])]) -> String {
        self.edit_change(target, files).await.commit_id
    }

    async fn edit_change(
        &self,
        target: edit_files_request::Target,
        files: &[(&str, &[u8])],
    ) -> v1::Change {
        let edits = files
            .iter()
            .map(|(path, bytes)| FileEdit {
                path: (*path).to_owned(),
                action: Some(if bytes.is_empty() {
                    file_edit::Action::Delete(v1::FileDelete::default())
                } else {
                    file_edit::Action::Content(bytes.to_vec())
                }),
                executable: None,
            })
            .collect();
        self.client()
            .edit_files(EditFilesRequest {
                repo: self.repo.clone(),
                target: Some(target),
                edits,
                ..EditFilesRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
    }

    /// The fixtures on `main`.
    async fn fixtures(&self) -> String {
        let files: Vec<(&str, Vec<u8>)> = FILES
            .iter()
            .map(|(at, name)| (*at, fixture(name)))
            .collect();
        let refs: Vec<(&str, &[u8])> = files.iter().map(|(p, b)| (*p, b.as_slice())).collect();
        self.commit(&self.main, &refs).await
    }

    async fn index(&self, revision: &str) -> v1::SymbolIndexStatus {
        self.index_waiting(revision, Some(Duration::from_secs(120)))
            .await
    }

    async fn index_waiting(&self, revision: &str, wait: Option<Duration>) -> v1::SymbolIndexStatus {
        self.client()
            .index_symbols(IndexSymbolsRequest {
                repo: self.repo.clone(),
                revision: revision.to_owned(),
                wait: wait.map(|w| w.try_into().unwrap()),
                ..IndexSymbolsRequest::default()
            })
            .await
            .unwrap()
            .status
            .unwrap()
    }

    fn list_request(&self, revision: &str) -> ListSymbolsRequest {
        ListSymbolsRequest {
            repo: self.repo.clone(),
            revision: revision.to_owned(),
            ..ListSymbolsRequest::default()
        }
    }

    async fn list(
        &self,
        request: ListSymbolsRequest,
    ) -> Result<v1::ListSymbolsResponse, ClientError> {
        self.client().list_symbols(request).await
    }

    async fn path(&self, revision: &str, path: &str) -> Vec<v1::SymbolOccurrence> {
        self.list(ListSymbolsRequest {
            path: path.to_owned(),
            ..self.list_request(revision)
        })
        .await
        .unwrap()
        .symbols
    }

    async fn named(&self, revision: &str, name: &str) -> Vec<v1::SymbolOccurrence> {
        self.list(ListSymbolsRequest {
            name: name.to_owned(),
            ..self.list_request(revision)
        })
        .await
        .unwrap()
        .symbols
    }
}

fn reason(error: &ClientError) -> String {
    error.reason().to_owned()
}

fn metadata(error: &ClientError, key: &str) -> String {
    error.metadata().get(key).cloned().unwrap_or_default()
}

fn count(status: &v1::SymbolIndexStatus, language: &str) -> u64 {
    status
        .languages
        .iter()
        .find(|l| l.language == language)
        .map_or(0, |l| l.files)
}

fn names(occurrences: &[v1::SymbolOccurrence]) -> Vec<(&str, &str, i32)> {
    occurrences
        .iter()
        .map(|o| (o.path.as_str(), o.name.as_str(), o.role))
        .collect()
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn index_on_demand() {
    let fx = start(|_| {}).await;
    let a = fx.fixtures().await;
    let status = fx.index(&a).await;
    assert_eq!(
        status.state,
        SymbolIndexState::Complete as i32,
        "{status:?}"
    );
    assert_eq!(status.commit_id, a);
    assert_eq!(status.files, 7, "README.md is not counted, nor the symlink");
    assert_eq!(status.files_indexed, 7);
    assert_eq!(status.files_skipped, 0);
    for (language, n) in [
        ("python", 1),
        ("typescript", 1),
        ("tsx", 1),
        ("javascript", 1),
        ("go", 2),
        ("rust", 1),
    ] {
        assert_eq!(count(&status, language), n, "{language}");
    }
    assert!(status.indexed_at.is_some());
    assert_eq!(fx.extracted(), 7);

    // A listing of a file is reef-symbols' extraction, with sources.
    let listed = fx.path(&a, "src/parser.py").await;
    let expected = reef_symbols::extract(
        reef_symbols::Language::Python,
        "src/parser.py",
        &fixture("parser.py"),
        &reef_symbols::Limits::default(),
    )
    .unwrap();
    assert_eq!(listed.len(), expected.occurrences.len());
    for (got, want) in listed.iter().zip(&expected.occurrences) {
        assert_eq!(got.name, want.name);
        assert_eq!(got.symbol, want.symbol.clone().unwrap_or_default());
        assert_eq!(got.source, SymbolSource::Tags as i32);
        assert_eq!(got.language, "python");
        assert_eq!(
            got.range.as_ref().unwrap().start_line,
            want.range.start_line
        );
    }
    let parse_call = listed.iter().find(|o| o.name == "tokenize").unwrap();
    assert_eq!(
        parse_call.enclosing_symbol,
        "reef-tags . . . src/`parser.py`/Parser#parse()."
    );

    // A second call extracts nothing.
    let again = fx.index(&a).await;
    assert_eq!(again.state, SymbolIndexState::Complete as i32);
    assert_eq!(fx.extracted(), 7);

    // `wait: 0` returns at once; a large fresh tree is still indexing.
    let big: Vec<(String, Vec<u8>)> = (0..400)
        .map(|i| (format!("gen/m{i}.py"), fixture("parser.py")))
        .collect();
    let refs: Vec<(&str, &[u8])> = big
        .iter()
        .map(|(p, b)| (p.as_str(), b.as_slice()))
        .collect();
    let b = fx.commit(&a, &refs).await;
    let quick = fx.index_waiting(&b, Some(Duration::ZERO)).await;
    assert_ne!(quick.state, SymbolIndexState::NotIndexed as i32);
    let done = fx.index(&b).await;
    assert_eq!(done.state, SymbolIndexState::Complete as i32);
    assert_eq!(done.files, 407);
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn incremental_and_shared() {
    let fx = start(|_| {}).await;
    let a = fx.fixtures().await;
    fx.index(&a).await;
    let before = fx.extracted();

    // A child changing one file extracts that file only, from the parent's counts.
    let b = fx
        .commit(&a, &[("src/parser.py", b"def changed():\n    pass\n")])
        .await;
    let status = fx.index(&b).await;
    assert_eq!(fx.extracted() - before, 1);
    assert_eq!(status.files, 7);
    assert_eq!(status.files_indexed, 7);

    // A file removed and a file added.
    let c = fx
        .commit(
            &b,
            &[
                ("web/app.js", b""),
                ("web/extra.ts", b"export function extra() {}\n"),
            ],
        )
        .await;
    let status = fx.index(&c).await;
    assert_eq!(status.files, 7);
    assert_eq!(count(&status, "javascript"), 0);
    assert_eq!(count(&status, "typescript"), 2);
    assert_eq!(fx.extracted() - before, 2);

    // A rename extracts once, under the new path; an identical tree reuses its row.
    let d = fx
        .commit(
            &c,
            &[
                ("src/parser.py", b""),
                ("src/moved.py", b"def changed():\n    pass\n"),
            ],
        )
        .await;
    fx.index(&d).await;
    assert_eq!(fx.extracted() - before, 3);
    let same = fx.commit(&c, &[]).await;
    let status = fx.index(&same).await;
    assert_eq!(status.state, SymbolIndexState::Complete as i32);
    assert_eq!(fx.extracted() - before, 3, "the same tree as c");

    // The same file in two commits is one entry: no extraction at a sibling.
    let sibling = fx.commit(&a, &[("README.md", b"other\n")]).await;
    fx.index(&sibling).await;
    assert_eq!(fx.extracted() - before, 3);
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn background_indexing() {
    let fx = start(|config| config.file.symbols.background = true).await;
    let a = fx.fixtures().await;
    // An EditFiles commit is indexed without being asked.
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = fx.index_waiting(&a, Some(Duration::ZERO)).await;
        if status.state == SymbolIndexState::Complete as i32 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "not indexed within 30 s: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    let after_edit = fx.extracted();
    assert_eq!(after_edit, 7);

    // A workspace's snapshot is indexed too.
    let ws = fx
        .client()
        .create_workspace(CreateWorkspaceRequest {
            repo: fx.repo.clone(),
            owner: "tester@host".into(),
            start: Some(v1::create_workspace_request::Start::NewOn(a.clone())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap()
        .workspace
        .unwrap();
    std::fs::write(
        Path::new(&ws.path).join("src/new.py"),
        "def fresh():\n    pass\n",
    )
    .unwrap();
    let snapshot = fx
        .client()
        .snapshot_workspace(&fx.repo, &ws.id, "")
        .await
        .unwrap();
    let commit = snapshot.commit_id;
    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    loop {
        let status = fx.index_waiting(&commit, Some(Duration::ZERO)).await;
        if status.state == SymbolIndexState::Complete as i32 {
            assert_eq!(status.files, 8);
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the snapshot was not indexed: {status:?}"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    assert_eq!(
        fx.extracted(),
        after_edit + 1,
        "only the file the snapshot added"
    );
}

// 14, background off
#[tokio::test(flavor = "multi_thread")]
async fn nothing_is_indexed_unasked_with_background_off() {
    let fx = start(|_| {}).await;
    let a = fx.fixtures().await;
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert_eq!(fx.extracted(), 0);
    let status = fx
        .list(ListSymbolsRequest {
            name: "Parser".into(),
            ..fx.list_request(&a)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&status), "SYMBOLS_NOT_INDEXED");
}

/// A SCIP document defining `Parser` and referencing it, for `src/parser.py`.
fn scip_document(path: &str) -> Vec<u8> {
    let symbol = "scip-python python fixture 0.1 `src.parser`/Parser#";
    scip::Document {
        relative_path: path.into(),
        position_encoding: scip::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
        occurrences: vec![
            scip::Occurrence {
                symbol: symbol.into(),
                symbol_roles: scip::SymbolRole::Definition as i32,
                typed_range: Some(scip::occurrence::TypedRange::SingleLineRange(
                    scip::SingleLineRange {
                        line: 6,
                        start_character: 6,
                        end_character: 12,
                    },
                )),
                ..scip::Occurrence::default()
            },
            scip::Occurrence {
                symbol: symbol.into(),
                typed_range: Some(scip::occurrence::TypedRange::SingleLineRange(
                    scip::SingleLineRange {
                        line: 24,
                        start_character: 11,
                        end_character: 17,
                    },
                )),
                ..scip::Occurrence::default()
            },
        ],
        ..scip::Document::default()
    }
    .encode_to_vec()
}

fn document(path: &str, bytes: &[u8], scip: Vec<u8>) -> ScipDocument {
    ScipDocument {
        path: path.into(),
        content_digest: reef_symbols::content_digest(bytes),
        document: scip,
    }
}

fn upload_request(
    fx: &Fixture,
    revision: &str,
    documents: Vec<ScipDocument>,
) -> UploadScipDocumentsRequest {
    UploadScipDocumentsRequest {
        repo: fx.repo.clone(),
        revision: revision.into(),
        tool: Some(ScipTool {
            name: "scip-python".into(),
            version: "0.6.0".into(),
        }),
        documents,
        ..UploadScipDocumentsRequest::default()
    }
}

// 15
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one case per rejection")]
async fn upload_scip() {
    let fx = start(|_| {}).await;
    let parser = fixture("parser.py");
    let big = vec![b'#'; 2 << 20];
    let a = fx.fixtures().await;
    let a = fx.commit(&a, &[("big.py", &big)]).await;

    let ok = document("src/parser.py", &parser, scip_document("parser.py"));
    let response = fx
        .client()
        .upload_scip_documents(upload_request(
            &fx,
            &a,
            vec![
                ok.clone(),
                ScipDocument {
                    path: "../out.py".into(),
                    ..ok.clone()
                },
                ok.clone(),
                ScipDocument {
                    path: "missing.py".into(),
                    ..ok.clone()
                },
                ScipDocument {
                    path: "src".into(),
                    ..ok.clone()
                },
                ScipDocument {
                    path: "link.py".into(),
                    ..ok.clone()
                },
                document("big.py", &big, scip_document("big.py")),
                ScipDocument {
                    path: "src/lib.rs".into(),
                    ..ok.clone()
                },
                document(
                    "web/app.js",
                    &fixture("app.js"),
                    b"\xff\xff garbage".to_vec(),
                ),
                document("web/shapes.ts", &fixture("shapes.ts"), {
                    let mut d = scip::Document::decode(scip_document("x").as_slice()).unwrap();
                    d.occurrences[0].typed_range = Some(
                        scip::occurrence::TypedRange::SingleLineRange(scip::SingleLineRange {
                            line: 9999,
                            start_character: 0,
                            end_character: 1,
                        }),
                    );
                    d.encode_to_vec()
                }),
            ],
        ))
        .await
        .unwrap();
    assert_eq!(response.accepted, 1, "{response:?}");
    let reasons: Vec<(&str, i32)> = response
        .rejected
        .iter()
        .map(|r| (r.path.as_str(), r.reason))
        .collect();
    assert_eq!(
        reasons,
        [
            ("../out.py", v1::ScipRejection::InvalidPath as i32),
            ("src/parser.py", v1::ScipRejection::DuplicatePath as i32),
            ("missing.py", v1::ScipRejection::PathNotFound as i32),
            ("src", v1::ScipRejection::PathNotFound as i32),
            ("link.py", v1::ScipRejection::NotAFile as i32),
            ("big.py", v1::ScipRejection::FileTooLarge as i32),
            ("src/lib.rs", v1::ScipRejection::ContentMismatch as i32),
            ("web/app.js", v1::ScipRejection::InvalidDocument as i32),
            ("web/shapes.ts", v1::ScipRejection::RangeOutOfBounds as i32),
        ]
    );
    assert_eq!(response.commit_id, a);

    // The accepted document answers for its file; others are tags.
    let listed = fx.path(&a, "src/parser.py").await;
    assert_eq!(listed.len(), 2);
    assert!(listed.iter().all(|o| o.source == SymbolSource::Scip as i32));
    let definition = &listed[0];
    assert_eq!(definition.name, "Parser");
    assert_eq!(definition.role, SymbolRole::Definition as i32);
    assert_eq!(
        definition.definition_range.as_ref().unwrap().end_line,
        19,
        "the class's range, from tags"
    );
    let tags = fx
        .list(ListSymbolsRequest {
            path: "src/parser.py".into(),
            source: SymbolSource::Tags as i32,
            ..fx.list_request(&a)
        })
        .await
        .unwrap()
        .symbols;
    assert!(tags.len() > 2 && tags.iter().all(|o| o.source == SymbolSource::Tags as i32));
    let rust = fx.path(&a, "src/lib.rs").await;
    assert!(rust.iter().all(|o| o.source == SymbolSource::Tags as i32));

    // Refusals of the whole request.
    for (documents, field) in [(vec![], "documents"), (vec![ok.clone(); 1001], "documents")] {
        let error = fx
            .client()
            .upload_scip_documents(upload_request(&fx, &a, documents))
            .await
            .unwrap_err();
        assert_eq!(reason(&error), "INVALID_UPLOAD");
        assert_eq!(metadata(&error, "field"), field);
    }
    let error = fx
        .client()
        .upload_scip_documents(UploadScipDocumentsRequest {
            tool: None,
            ..upload_request(&fx, &a, vec![ok.clone()])
        })
        .await
        .unwrap_err();
    assert_eq!(metadata(&error, "field"), "tool");

    // A re-upload replaces the entry; it applies wherever the file is the same.
    let mut one = scip::Document::decode(scip_document("parser.py").as_slice()).unwrap();
    one.occurrences.truncate(1);
    fx.client()
        .upload_scip_documents(upload_request(
            &fx,
            &a,
            vec![document("src/parser.py", &parser, one.encode_to_vec())],
        ))
        .await
        .unwrap();
    assert_eq!(fx.path(&a, "src/parser.py").await.len(), 1);
    let b = fx.commit(&a, &[("README.md", b"same parser\n")]).await;
    assert_eq!(
        fx.path(&b, "src/parser.py").await[0].source,
        SymbolSource::Scip as i32
    );
    let c = fx
        .commit(&a, &[("src/parser.py", b"def other():\n    pass\n")])
        .await;
    assert!(
        fx.path(&c, "src/parser.py")
            .await
            .iter()
            .all(|o| o.source == SymbolSource::Tags as i32)
    );
}

// 16
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one case per query form")]
async fn list_queries() {
    let fx = start(|_| {}).await;
    let a = fx.fixtures().await;
    let a = fx
        .commit(&a, &[("srcx/other.py", b"def tokenize():\n    pass\n")])
        .await;

    // A name query on a tree not indexed refuses, with what to run.
    let error = fx
        .list(ListSymbolsRequest {
            name: "tokenize".into(),
            ..fx.list_request(&a)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "SYMBOLS_NOT_INDEXED");
    assert_eq!(metadata(&error, "commit_id"), a);
    assert_eq!(metadata(&error, "missing"), "8");
    assert_eq!(
        error.advice().unwrap().next,
        ["reef", "symbols", "index", "-r", a.as_str()]
    );

    // A path query extracts the few files it needs inline.
    let src = fx.path(&a, "src").await;
    assert!(
        src.iter().all(|o| o.path.starts_with("src/")),
        "srcx is not under src"
    );
    assert_eq!(fx.extracted(), 2);
    fx.index(&a).await;

    // By name: definitions and references in this tree's files only.
    let tokenize = fx.named(&a, "tokenize").await;
    assert_eq!(
        names(&tokenize),
        [
            ("src/parser.py", "tokenize", SymbolRole::Reference as i32),
            ("srcx/other.py", "tokenize", SymbolRole::Definition as i32),
        ]
    );
    let b = fx.commit(&a, &[("srcx/other.py", b"")]).await;
    fx.index(&b).await;
    assert_eq!(
        fx.named(&b, "tokenize").await.len(),
        1,
        "the definition was removed at b"
    );
    assert_eq!(fx.named(&a, "tokenize").await.len(), 2, "and is still at a");

    // By symbol, with the path as a limit, and filters.
    let store = fx
        .list(ListSymbolsRequest {
            symbol: "reef-tags . . . pkg/store/Store#".into(),
            ..fx.list_request(&a)
        })
        .await
        .unwrap()
        .symbols;
    assert_eq!(
        names(&store),
        [("pkg/store/store.go", "Store", SymbolRole::Definition as i32)]
    );
    let methods = fx
        .list(ListSymbolsRequest {
            path: "pkg".into(),
            roles: vec![SymbolRole::Definition as i32],
            kinds: vec![v1::SymbolKind::Method as i32],
            ..fx.list_request(&a)
        })
        .await
        .unwrap()
        .symbols;
    let mut method_names: Vec<&str> = methods.iter().map(|o| o.name.as_str()).collect();
    method_names.sort_unstable();
    assert_eq!(method_names, ["Close", "Flush", "Get"]);

    // Pages concatenate to the whole listing; a token is bound to its request.
    let whole = fx.path(&a, "web").await;
    let mut paged = Vec::new();
    let mut token = String::new();
    loop {
        let page = fx
            .client()
            .list_symbols_page(ListSymbolsRequest {
                path: "web".into(),
                page_size: 7,
                page_token: token.clone(),
                ..fx.list_request(&a)
            })
            .await
            .unwrap();
        assert!(page.symbols.len() <= 7);
        paged.extend(page.symbols);
        if page.next_page_token.is_empty() {
            break;
        }
        token = page.next_page_token;
    }
    assert_eq!(paged, whole);
    let first = fx
        .client()
        .list_symbols_page(ListSymbolsRequest {
            path: "web".into(),
            page_size: 7,
            ..fx.list_request(&a)
        })
        .await
        .unwrap();
    let error = fx
        .client()
        .list_symbols_page(ListSymbolsRequest {
            path: "src".into(),
            page_size: 7,
            page_token: first.next_page_token,
            ..fx.list_request(&a)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "INVALID_PAGE_TOKEN");

    // Malformed queries.
    for (request, field) in [
        (fx.list_request(&a), "path"),
        (
            ListSymbolsRequest {
                name: "x".into(),
                symbol: "reef-tags . . . x.".into(),
                ..fx.list_request(&a)
            },
            "symbol",
        ),
        (
            ListSymbolsRequest {
                symbol: "not a symbol".into(),
                ..fx.list_request(&a)
            },
            "symbol",
        ),
    ] {
        let error = fx.list(request).await.unwrap_err();
        assert_eq!(error.kind(), ErrorKind::InvalidArgument);
        assert_eq!(reason(&error), "INVALID_SYMBOL_QUERY");
        assert_eq!(metadata(&error, "field"), field);
    }
}

// 17
#[tokio::test(flavor = "multi_thread")]
async fn skips_in_trees() {
    let fx = start(|_| {}).await;
    let big = vec![b'#'; 2 << 20];
    let a = fx
        .commit(
            &fx.main,
            &[
                ("big.py", &big),
                ("bin.go", b"package x\n\x00\x01"),
                ("ok.rs", b"fn ok() {}\n"),
            ],
        )
        .await;
    let status = fx.index(&a).await;
    assert_eq!(status.files, 3, "the symlink link.py is not counted");
    assert_eq!(status.files_indexed, 3);
    assert_eq!(status.files_skipped, 2);
    let mut skipped: Vec<(i32, u64)> = status.skipped.iter().map(|s| (s.reason, s.files)).collect();
    skipped.sort_unstable();
    assert_eq!(
        skipped,
        [
            (v1::SymbolSkipReason::TooLarge as i32, 1),
            (v1::SymbolSkipReason::Binary as i32, 1)
        ]
    );
    assert_eq!(fx.path(&a, "ok.rs").await.len(), 1);
    assert_eq!(fx.path(&a, "big.py").await.len(), 0);
    assert_eq!(fx.path(&a, "link.py").await.len(), 0);

    // A conflicted path counts as skipped and stores nothing: an edit under
    // a descendant that changed the same line.
    let child = fx.commit(&a, &[("conflict.rs", b"fn x() {}\n")]).await;
    let grandchild = fx
        .commit_change(&child, &[("conflict.rs", b"fn y() {}\n")])
        .await;
    fx.edit(
        edit_files_request::Target::Revision(child.clone()),
        &[("conflict.rs", b"fn z() {}\n")],
    )
    .await;
    let rebased = fx
        .client()
        .get_change(v1::GetChangeRequest {
            repo: fx.repo.clone(),
            revision: grandchild.change_id.clone(),
            ..v1::GetChangeRequest::default()
        })
        .await
        .unwrap()
        .change
        .unwrap();
    assert_ne!(rebased.commit_id, grandchild.commit_id, "rebased");
    let status = fx.index(&rebased.commit_id).await;
    assert!(
        status
            .skipped
            .iter()
            .any(|s| s.reason == v1::SymbolSkipReason::Conflicted as i32 && s.files == 1),
        "{status:?}"
    );
    assert_eq!(fx.path(&rebased.commit_id, "conflict.rs").await.len(), 0);
}

// 18
#[tokio::test(flavor = "multi_thread")]
async fn gc_symbols() {
    let fx = start(|config| {
        config.file.gc.undo_window = Duration::ZERO;
    })
    .await;
    let a = fx.fixtures().await;
    // A workspace on a, so a's tree is live.
    fx.client()
        .create_workspace(CreateWorkspaceRequest {
            repo: fx.repo.clone(),
            owner: "tester@host".into(),
            start: Some(v1::create_workspace_request::Start::NewOn(a.clone())),
            ..CreateWorkspaceRequest::default()
        })
        .await
        .unwrap();
    let b = fx
        .commit(&fx.main, &[("only_b.py", b"def only_b():\n    pass\n")])
        .await;
    fx.index(&a).await;
    fx.index(&b).await;
    let extracted = fx.extracted();
    // GC keeps what was used at or after its start: start it a little later.
    tokio::time::sleep(Duration::from_millis(20)).await;
    fx.client().collect_garbage(&fx.repo, false).await.unwrap();
    // a's entries stay; b's own entry and tree row are gone, and a read re-extracts it.
    fx.index(&a).await;
    assert_eq!(fx.extracted(), extracted, "a's entries survived");
    let passes = fx.daemon.handle.state().symbols.passes_run();
    let status = fx.index(&b).await;
    assert_eq!(status.state, SymbolIndexState::Complete as i32);
    assert_eq!(
        fx.daemon.handle.state().symbols.passes_run() - passes,
        1,
        "b's row was swept, so it takes a pass again"
    );
    assert_eq!(
        fx.extracted(),
        extracted + 1,
        "only_b.py was swept and extracted again"
    );
    assert_eq!(fx.path(&b, "only_b.py").await.len(), 1);
}

// 19
#[tokio::test(flavor = "multi_thread")]
async fn cache_file_lifecycle() {
    let fx = start(|_| {}).await;
    let a = fx.fixtures().await;
    fx.index(&a).await;
    let repo_id = fx.repo.parse().unwrap();
    let path = fx.daemon.handle.state().symbols.path_of(repo_id);
    assert!(path.exists());

    // Garbage in the file while the daemon is stopped: recreated at next use.
    let stopped = fx.daemon.stop_keeping().await;
    std::fs::write(&path, b"this is not a database at all, not even close").unwrap();
    for suffix in ["-wal", "-shm"] {
        let _ = std::fs::remove_file(format!("{}{suffix}", path.display()));
    }
    let daemon = stopped.start().await;
    let fx = Fixture { daemon, ..fx };
    let status = fx.index(&a).await;
    assert_eq!(status.state, SymbolIndexState::Complete as i32);
    assert_eq!(fx.extracted(), 7, "rebuilt from nothing");

    // Forgetting the repository removes it.
    fx.client().forget_repository(&fx.repo).await.unwrap();
    assert!(!path.exists());
    let _ = &fx.root;
}

// 20
#[tokio::test(flavor = "multi_thread")]
async fn disabled_and_access() {
    let fx = start(|config| config.file.symbols.enabled = false).await;
    let a = fx.fixtures().await;
    let index = fx
        .client()
        .index_symbols(IndexSymbolsRequest {
            repo: fx.repo.clone(),
            revision: a.clone(),
            ..IndexSymbolsRequest::default()
        })
        .await
        .unwrap_err();
    let upload = fx
        .client()
        .upload_scip_documents(upload_request(
            &fx,
            &a,
            vec![document(
                "src/parser.py",
                &fixture("parser.py"),
                scip_document("p"),
            )],
        ))
        .await
        .unwrap_err();
    let list = fx
        .list(ListSymbolsRequest {
            path: "src".into(),
            ..fx.list_request(&a)
        })
        .await
        .unwrap_err();
    for error in [index, upload, list] {
        assert_eq!(error.kind(), ErrorKind::Conflict, "{error}");
        assert_eq!(reason(&error), "SYMBOLS_DISABLED");
    }
    let repo_id = fx.repo.parse().unwrap();
    assert!(!fx.daemon.handle.state().symbols.path_of(repo_id).exists());

    for (method, access) in [
        ("/reef.api.v1.SymbolService/IndexSymbols", v1::Access::Read),
        (
            "/reef.api.v1.SymbolService/UploadScipDocuments",
            v1::Access::Write,
        ),
        ("/reef.api.v1.SymbolService/ListSymbols", v1::Access::Read),
    ] {
        assert_eq!(reef_api::access(method), Some(access), "{method}");
    }
}

// 21
#[tokio::test(flavor = "multi_thread")]
async fn single_flight() {
    let fx = start(|_| {}).await;
    let a = fx.fixtures().await;
    let state = std::sync::Arc::clone(fx.daemon.handle.state());
    let before = state.symbols.passes_run();
    let calls: Vec<_> = (0..8)
        .map(|_| {
            let client = fx.client().clone();
            let repo = fx.repo.clone();
            let a = a.clone();
            tokio::spawn(async move {
                client
                    .index_symbols(IndexSymbolsRequest {
                        repo,
                        revision: a,
                        wait: Some(Duration::from_secs(120).try_into().unwrap()),
                        ..IndexSymbolsRequest::default()
                    })
                    .await
                    .unwrap()
            })
        })
        .collect();
    for call in calls {
        let status = call.await.unwrap().status.unwrap();
        assert_eq!(status.state, SymbolIndexState::Complete as i32);
    }
    assert_eq!(
        state.symbols.passes_run() - before,
        1,
        "eight calls, one pass"
    );
    assert_eq!(fx.extracted(), 7);
}

/// A Python file with a function and a method named `hit` and `refs` calls.
fn hits(refs: usize) -> Vec<u8> {
    let mut text = "def hit():\n    pass\n\n\nclass Box:\n    def hit(self):\n".to_owned();
    text.push_str(&"        hit()\n".repeat(refs));
    text.into_bytes()
}

/// A SCIP document for [`hits`]'s file at `path`: the function, and each
/// call as a reference to it.
fn scip_hits(path: &str, symbol: &str, refs: i32) -> Vec<u8> {
    let at = |line, start_character, end_character| {
        Some(scip::occurrence::TypedRange::SingleLineRange(
            scip::SingleLineRange {
                line,
                start_character,
                end_character,
            },
        ))
    };
    let mut occurrences = vec![scip::Occurrence {
        symbol: symbol.into(),
        symbol_roles: scip::SymbolRole::Definition as i32,
        typed_range: at(0, 4, 7),
        ..scip::Occurrence::default()
    }];
    occurrences.extend((0..refs).map(|i| scip::Occurrence {
        symbol: symbol.into(),
        typed_range: at(6 + i, 8, 11),
        ..scip::Occurrence::default()
    }));
    scip::Document {
        relative_path: path.into(),
        position_encoding: scip::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
        occurrences,
        ..scip::Document::default()
    }
    .encode_to_vec()
}

/// Every page of `request` by pages of `size`, each with the paths it
/// looked up in a tree.
async fn pages(
    fx: &Fixture,
    request: &ListSymbolsRequest,
    size: i32,
) -> Vec<(v1::ListSymbolsResponse, u64)> {
    let mut pages = Vec::new();
    let mut token = String::new();
    loop {
        let before = SymbolIndex::tree_lookups();
        let page = fx
            .client()
            .list_symbols_page(ListSymbolsRequest {
                page_size: size,
                page_token: std::mem::take(&mut token),
                ..request.clone()
            })
            .await
            .unwrap();
        token.clone_from(&page.next_page_token);
        pages.push((page, SymbolIndex::tree_lookups() - before));
        if token.is_empty() {
            return pages;
        }
    }
}

// R2.16, 21
#[tokio::test(flavor = "multi_thread")]
#[expect(
    clippy::too_many_lines,
    reason = "one fixture, one case per query form"
)]
async fn list_name_in_path_order() {
    let fx = start(|_| {}).await;
    // Paths whose byte order is not jj's: `-`, `.` and `0` sort around `/`,
    // and non-ASCII after all of them.
    let paths = [
        "a.py",
        "a/b.py",
        "a-b.py",
        "a.b/c.py",
        "a/b/c.py",
        "a0.py",
        "a_b.py",
        "A.py",
        "é.py",
        "z/é-ü.py",
        "gone.py",
        "x/scip.py",
    ];
    let first: Vec<(&str, Vec<u8>)> = paths
        .iter()
        .map(|p| (*p, hits(24)))
        .chain(
            ["g/a.go", "g/a-b.go", "g/é.go", "g/a/b.go"]
                .map(|p| (p, b"package g\n\nfunc Hit() {}\n".to_vec())),
        )
        .collect();
    let refs: Vec<(&str, &[u8])> = first.iter().map(|(p, b)| (*p, b.as_slice())).collect();
    let v1 = fx.commit(&fx.main, &refs).await;
    // A second version of `a.py` and `g/a.go`, and `gone.py` removed: the
    // first versions' rows stay in the store.
    let v2 = fx
        .commit(
            &v1,
            &[
                ("a.py", &hits(7)),
                ("g/a.go", b"package g\n\n// Hit again.\nfunc Hit() {}\n"),
                ("gone.py", b""),
            ],
        )
        .await;
    fx.index(&v1).await;
    fx.index(&v2).await;
    let scip_symbol = "scip-python python fixture 0.1 `x.scip`/hit().";
    let uploaded = fx
        .client()
        .upload_scip_documents(upload_request(
            &fx,
            &v2,
            vec![document(
                "x/scip.py",
                &hits(24),
                scip_hits("x/scip.py", scip_symbol, 24),
            )],
        ))
        .await
        .unwrap();
    assert_eq!(uploaded.accepted, 1, "{uploaded:?}");
    let named = fx.named(&v2, "Hit").await;
    assert_eq!(named.len(), 4, "{named:?}");
    let go_symbol = named
        .iter()
        .find(|o| o.path == "g/a.go")
        .unwrap()
        .symbol
        .clone();

    let request = |revision: &str| ListSymbolsRequest {
        name: "hit".into(),
        ..fx.list_request(revision)
    };
    let all = fx.named(&v2, "hit").await;
    assert!(all.len() > 250, "{}", all.len());
    let order: Vec<&str> = all.iter().map(|o| o.path.as_str()).collect();
    let mut sorted = order.clone();
    sorted.sort_by(|a, b| a.split('/').cmp(b.split('/')));
    assert_eq!(order, sorted, "jj's path order");
    assert!(order.iter().position(|p| *p == "a/b.py") < order.iter().position(|p| *p == "a-b.py"));
    assert!(!order.contains(&"gone.py"));
    let scip_rows = all.iter().filter(|o| o.path == "x/scip.py");
    assert!(scip_rows.clone().count() > 0);
    assert!(
        scip_rows
            .clone()
            .all(|o| o.source == SymbolSource::Scip as i32)
    );

    let cases = [
        (request(&v2), 7),
        (request(&v2), 50),
        (request(&v1), 7),
        (
            ListSymbolsRequest {
                path: "a".into(),
                ..request(&v2)
            },
            7,
        ),
        (
            ListSymbolsRequest {
                path: "a.b".into(),
                ..request(&v2)
            },
            7,
        ),
        (
            ListSymbolsRequest {
                path: "z/é-ü.py".into(),
                ..request(&v2)
            },
            7,
        ),
        (
            ListSymbolsRequest {
                roles: vec![SymbolRole::Definition as i32],
                ..request(&v2)
            },
            3,
        ),
        (
            ListSymbolsRequest {
                kinds: vec![v1::SymbolKind::Method as i32],
                ..request(&v2)
            },
            3,
        ),
        (
            ListSymbolsRequest {
                source: SymbolSource::Tags as i32,
                ..request(&v2)
            },
            7,
        ),
        (
            ListSymbolsRequest {
                source: SymbolSource::Scip as i32,
                ..request(&v2)
            },
            7,
        ),
        (
            ListSymbolsRequest {
                symbol: go_symbol.clone(),
                ..fx.list_request(&v2)
            },
            1,
        ),
        (
            ListSymbolsRequest {
                symbol: go_symbol,
                ..fx.list_request(&v1)
            },
            1,
        ),
        (
            ListSymbolsRequest {
                symbol: scip_symbol.into(),
                ..fx.list_request(&v2)
            },
            7,
        ),
    ];
    for (request, size) in &cases {
        SymbolIndex::answer_lookups_by_reference(true);
        let want = pages(&fx, request, *size).await;
        SymbolIndex::answer_lookups_by_reference(false);
        let got = pages(&fx, request, *size).await;
        let responses = |pages: &[(v1::ListSymbolsResponse, u64)]| -> Vec<v1::ListSymbolsResponse> {
            pages.iter().map(|(page, _)| page.clone()).collect()
        };
        assert_eq!(responses(&got), responses(&want), "{request:?}");
        // Each page looks up its own paths, and at most `gone.py` besides.
        let size = u64::try_from(*size).unwrap();
        for (page, lookups) in &got {
            assert!(*lookups <= size + 2, "{lookups} for {page:?}");
        }
        let lookups = |pages: &[(v1::ListSymbolsResponse, u64)]| -> u64 {
            pages.iter().map(|(_, n)| n).sum()
        };
        // The reference looks up every path on every page.
        let mut paths: Vec<&str> = want
            .iter()
            .flat_map(|(page, _)| page.symbols.iter().map(|o| o.path.as_str()))
            .collect();
        paths.dedup();
        if want.len() > 2 && paths.len() > 1 {
            assert!(lookups(&got) < lookups(&want), "{request:?}");
        }
    }

    // A token the reference issued resumes where it left off.
    SymbolIndex::answer_lookups_by_reference(true);
    let want = pages(&fx, &request(&v2), 7).await;
    SymbolIndex::answer_lookups_by_reference(false);
    for pair in want.windows(2) {
        let resumed = fx
            .client()
            .list_symbols_page(ListSymbolsRequest {
                page_size: 7,
                page_token: pair[0].0.next_page_token.clone(),
                ..request(&v2)
            })
            .await
            .unwrap();
        assert_eq!(resumed, pair[1].0);
    }
}
