//! `GetDiff` with symbols through the daemon (R2.15, tests 7 to 17 and
//! 19): symbol changes per file across four languages, nesting, renamed
//! files and moves, call sites by name and by SCIP symbol, their bounds,
//! coverage, the 1,000-file limit, the index on demand, paging, expansion,
//! and the disabled index.

#![allow(
    clippy::unwrap_used,
    clippy::expect_used,
    reason = "test helpers outside #[test] fns; a panic is the failure report"
)]

use std::fmt::Write as _;
use std::path::Path;
use std::time::Duration;

use prost::Message as _;
use reef_api::v1::{
    self, CallSiteStatus, DiffFormat, EditFilesRequest, FileChangeKind, FileEdit, GetDiffRequest,
    GetDiffResponse, IfNotJj, IndexSymbolsRequest, ScipDocument, ScipTool, SymbolChangeKind,
    SymbolCoverage, SymbolIndexState, SymbolSource, UploadScipDocumentsRequest, edit_files_request,
    file_edit,
};
use reef_daemon::client::{Client, ClientError};
use reef_daemon::testing::TestDaemon;
use reef_symbols::scip::proto as scip;
use reef_testing::git::init_repo;

const FIXTURES: &str = concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../reef-symbols/tests/fixtures/diff"
);

fn fixture(name: &str) -> Vec<u8> {
    std::fs::read(Path::new(FIXTURES).join(name)).unwrap()
}

struct Fixture {
    daemon: TestDaemon,
    repo: String,
    main: String,
}

async fn start(configure: impl FnOnce(&mut reef_daemon::config::Config)) -> Fixture {
    let daemon = TestDaemon::start_with(|config| {
        config.file.watcher.debounce = Duration::from_hours(1);
        config.file.watcher.watched_fallback_interval = Duration::from_hours(1);
        // The history index's worker extracts entries inline too (R2.16),
        // which would make the counts here race it.
        config.file.history.enabled = false;
        configure(config);
    })
    .await;
    let root = daemon.tmp.path().join("repo");
    std::fs::create_dir_all(&root).unwrap();
    let main = init_repo(&root, &[("README.md", "# fixture\n")]);
    let registered = daemon
        .client
        .register_repository(&root, IfNotJj::InitAlongsideGit)
        .await
        .unwrap();
    Fixture {
        repo: registered.repository.unwrap().id,
        daemon,
        main,
    }
}

/// One edit: content, a delete (empty), or a copy from another path.
enum Edit<'a> {
    Write(&'a str, Vec<u8>),
    Delete(&'a str),
    Copy(&'a str, &'a str),
}

impl Fixture {
    fn client(&self) -> &Client {
        &self.daemon.client
    }

    fn state(&self) -> &reef_daemon::daemon::AppState {
        self.daemon.handle.state()
    }

    /// A new commit on `on` making `edits`; its commit id.
    async fn edit(&self, on: &str, edits: Vec<Edit<'_>>) -> String {
        let edits = edits
            .into_iter()
            .map(|edit| match edit {
                Edit::Write(path, bytes) => FileEdit {
                    path: path.to_owned(),
                    action: Some(file_edit::Action::Content(bytes)),
                    executable: None,
                },
                Edit::Delete(path) => FileEdit {
                    path: path.to_owned(),
                    action: Some(file_edit::Action::Delete(v1::FileDelete::default())),
                    executable: None,
                },
                Edit::Copy(path, from) => FileEdit {
                    path: path.to_owned(),
                    action: Some(file_edit::Action::CopyFrom(from.to_owned())),
                    executable: None,
                },
            })
            .collect();
        self.client()
            .edit_files(EditFilesRequest {
                repo: self.repo.clone(),
                target: Some(edit_files_request::Target::NewOn(on.to_owned())),
                edits,
                ..EditFilesRequest::default()
            })
            .await
            .unwrap()
            .change
            .unwrap()
            .commit_id
    }

    /// A new commit on `on` writing `files`.
    async fn write(&self, on: &str, files: &[(&str, &[u8])]) -> String {
        self.edit(
            on,
            files
                .iter()
                .map(|(p, b)| Edit::Write(p, b.to_vec()))
                .collect(),
        )
        .await
    }

    fn request(&self, from: &str, to: &str) -> GetDiffRequest {
        GetDiffRequest {
            repo: self.repo.clone(),
            from_revision: from.to_owned(),
            to_revision: to.to_owned(),
            symbols: true,
            ..GetDiffRequest::default()
        }
    }

    async fn diff(&self, request: GetDiffRequest) -> Result<GetDiffResponse, ClientError> {
        self.client().get_diff_page(request).await
    }

    async fn symbols(&self, from: &str, to: &str) -> GetDiffResponse {
        self.diff(self.request(from, to)).await.unwrap()
    }

    async fn index(&self, revision: &str) -> v1::SymbolIndexStatus {
        self.client()
            .index_symbols(IndexSymbolsRequest {
                repo: self.repo.clone(),
                revision: revision.to_owned(),
                wait: Some(Duration::from_secs(120).try_into().unwrap()),
                ..IndexSymbolsRequest::default()
            })
            .await
            .unwrap()
            .status
            .unwrap()
    }
}

fn file<'a>(diff: &'a GetDiffResponse, path: &str) -> &'a v1::FileDiff {
    diff.files
        .iter()
        .find(|f| f.change.as_ref().unwrap().path == path)
        .unwrap_or_else(|| panic!("no file {path} in {diff:?}"))
}

fn symbols<'a>(diff: &'a GetDiffResponse, path: &str) -> &'a v1::FileSymbolDiff {
    file(diff, path).symbols.as_ref().unwrap()
}

/// `(kind, name)` of a file's changes, in order.
fn kinds(diff: &GetDiffResponse, path: &str) -> Vec<(SymbolChangeKind, String)> {
    symbols(diff, path)
        .changes
        .iter()
        .map(|c| (c.kind(), c.name.clone()))
        .collect()
}

fn change<'a>(diff: &'a GetDiffResponse, path: &str, name: &str) -> &'a v1::SymbolChange {
    symbols(diff, path)
        .changes
        .iter()
        .find(|c| c.name == name)
        .unwrap_or_else(|| panic!("no change to {name} in {path}: {diff:?}"))
}

fn reason(error: &ClientError) -> String {
    error.reason().to_owned()
}

fn field(error: &ClientError) -> String {
    error.metadata().get("field").cloned().unwrap_or_default()
}

/// The four languages' fixtures before and after, and a README.
async fn four_languages(fx: &Fixture) -> (String, String) {
    let before = fx
        .write(
            &fx.main,
            &[
                ("src/parser.py", &fixture("before/parser.py")),
                ("web/pick.ts", &fixture("before/pick.ts")),
                ("pkg/store.go", &fixture("before/store.go")),
                ("src/lib.rs", &fixture("before/lib.rs")),
            ],
        )
        .await;
    let after = fx
        .write(
            &before,
            &[
                ("src/parser.py", &fixture("after/parser.py")),
                ("web/pick.ts", &fixture("after/pick.ts")),
                ("pkg/store.go", &fixture("after/store.go")),
                ("src/lib.rs", &fixture("after/lib.rs")),
                ("README.md", b"# changed\n"),
            ],
        )
        .await;
    (before, after)
}

// 7
#[tokio::test(flavor = "multi_thread")]
async fn four_languages_are_covered() {
    let fx = start(|_| {}).await;
    let (before, after) = four_languages(&fx).await;
    let diff = fx.symbols(&before, &after).await;
    for path in ["src/parser.py", "web/pick.ts", "pkg/store.go", "src/lib.rs"] {
        let file = symbols(&diff, path);
        assert_eq!(file.coverage(), SymbolCoverage::Covered, "{path}");
        assert_eq!(file.source(), SymbolSource::Tags, "{path}");
    }
    assert_eq!(
        kinds(&diff, "src/parser.py"),
        [
            (SymbolChangeKind::Changed, "Parser".to_owned()),
            (SymbolChangeKind::Changed, "parse".to_owned()),
            (SymbolChangeKind::Added, "helper".to_owned()),
            (SymbolChangeKind::Removed, "legacy".to_owned()),
        ]
    );
    assert_eq!(
        kinds(&diff, "web/pick.ts"),
        [(SymbolChangeKind::Removed, "pick".to_owned())]
    );
    assert_eq!(
        kinds(&diff, "pkg/store.go"),
        [(SymbolChangeKind::Removed, "Put".to_owned())]
    );
    assert_eq!(
        kinds(&diff, "src/lib.rs"),
        [
            (SymbolChangeKind::Changed, "norm".to_owned()),
            (SymbolChangeKind::Added, "twice".to_owned()),
            (SymbolChangeKind::Removed, "origin".to_owned()),
        ]
    );
    let parse = change(&diff, "src/parser.py", "parse");
    assert_eq!(
        parse.symbol,
        "reef-tags . . . src/`parser.py`/Parser#parse()."
    );
    assert_eq!(
        parse.enclosing_symbol,
        "reef-tags . . . src/`parser.py`/Parser#"
    );
    assert_eq!((parse.lines_added, parse.lines_removed), (1, 1));
    assert_eq!(parse.new_range.unwrap().start_line, 10);
    assert_eq!(parse.old_range.unwrap().start_line, 9);
    let helper = change(&diff, "src/parser.py", "helper");
    assert_eq!((helper.lines_added, helper.lines_removed), (2, 0));
    let readme = symbols(&diff, "README.md");
    assert_eq!(readme.coverage(), SymbolCoverage::Unsupported);
    assert_eq!(readme.changes, [] as [v1::SymbolChange; 0]);

    let summary = diff.symbol_summary.clone().unwrap();
    assert_eq!(
        (
            summary.added,
            summary.removed,
            summary.changed,
            summary.moved
        ),
        (2, 4, 3, 0)
    );
    assert_eq!(
        (
            summary.files_covered,
            summary.files_unsupported,
            summary.files_skipped
        ),
        (4, 1, 0)
    );
    assert!(!summary.diff_too_large);

    // Without `symbols`, the response is what it was before R2.15.
    let plain = fx
        .diff(GetDiffRequest {
            symbols: false,
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    assert!(plain.symbol_summary.is_none());
    assert!(plain.files.iter().all(|f| f.symbols.is_none()));
    let mut stripped = diff.clone();
    stripped.symbol_summary = None;
    for f in &mut stripped.files {
        f.symbols = None;
    }
    assert_eq!(stripped.encode_to_vec(), plain.encode_to_vec());
}

// 8
#[tokio::test(flavor = "multi_thread")]
async fn nesting() {
    let fx = start(|_| {}).await;
    let class = |body: &str, extra: &str| {
        format!(
            "class Parser:\n{extra}    def parse(self):\n        return {body}\n\n    def other(self):\n        return 0\n"
        )
    };
    let rust = |body: &str| {
        format!("struct S;\n\nimpl S {{\n    fn run(&self) -> i32 {{\n        {body}\n    }}\n}}\n")
    };
    let before = fx
        .write(
            &fx.main,
            &[
                ("a.py", class("1", "").as_bytes()),
                ("b.rs", rust("1").as_bytes()),
                ("c.py", b"def f():\n    return 1\n"),
            ],
        )
        .await;
    let after = fx
        .write(
            &before,
            &[
                ("a.py", class("2", "").as_bytes()),
                ("b.rs", rust("2").as_bytes()),
                ("c.py", b"@cache\ndef f():\n    return 1\n"),
            ],
        )
        .await;
    let diff = fx.symbols(&before, &after).await;
    let parser = change(&diff, "a.py", "Parser");
    assert_eq!(parser.kind(), SymbolChangeKind::Changed);
    assert!(parser.nested_only);
    assert_eq!(parser.call_site_status(), CallSiteStatus::Unspecified);
    let parse = change(&diff, "a.py", "parse");
    assert!(!parse.nested_only);
    assert_eq!(
        kinds(&diff, "b.rs"),
        [(SymbolChangeKind::Changed, "run".to_owned())]
    );
    assert_eq!(
        kinds(&diff, "c.py"),
        [(SymbolChangeKind::Changed, "f".to_owned())]
    );

    // A new line in the class outside every method.
    let attribute = fx
        .write(&before, &[("a.py", class("2", "    x = 1\n").as_bytes())])
        .await;
    let diff = fx.symbols(&before, &attribute).await;
    assert!(!change(&diff, "a.py", "Parser").nested_only);
}

// 9
#[tokio::test(flavor = "multi_thread")]
async fn renames_and_moves_end_to_end() {
    let fx = start(|_| {}).await;
    let before = fx
        .write(
            &fx.main,
            &[
                ("a.py", &fixture("before/parser.py")),
                ("pkg/store.go", &fixture("before/store.go")),
                (
                    "util.py",
                    b"def slugify(s):\n    return s.lower()\n\n\ndef keep():\n    return 1\n",
                ),
            ],
        )
        .await;
    let after = fx
        .edit(
            &before,
            vec![
                Edit::Copy("b/a.py", "a.py"),
                Edit::Delete("a.py"),
                Edit::Write("pkg/store.go", fixture("after/store.go")),
                Edit::Write("pkg/put.go", fixture("after/put.go")),
                Edit::Write("util.py", b"def keep():\n    return 1\n".to_vec()),
                Edit::Write(
                    "text/slug.py",
                    b"def slugify(s):\n    return s.lower()\n".to_vec(),
                ),
            ],
        )
        .await;
    let diff = fx.symbols(&before, &after).await;
    let renamed = file(&diff, "b/a.py");
    assert_eq!(
        renamed.change.as_ref().unwrap().kind(),
        FileChangeKind::Renamed
    );
    assert_eq!(kinds(&diff, "b/a.py"), []);
    let put = change(&diff, "pkg/put.go", "Put");
    assert_eq!(put.kind(), SymbolChangeKind::Moved);
    assert_eq!(put.old_path, "pkg/store.go");
    let slug = change(&diff, "text/slug.py", "slugify");
    assert_eq!(slug.kind(), SymbolChangeKind::Moved);
    assert_eq!(slug.old_path, "util.py");
    assert_eq!(slug.old_symbol, "reef-tags . . . `util.py`/slugify().");
    assert_eq!(kinds(&diff, "util.py"), []);

    // Renamed with a body changed: changed, with the old symbol.
    let changed = fx
        .edit(
            &before,
            vec![Edit::Copy("b/a.py", "a.py"), Edit::Delete("a.py")],
        )
        .await;
    let changed = fx
        .write(&changed, &[("b/a.py", &fixture("after/parser.py"))])
        .await;
    let diff = fx.symbols(&before, &changed).await;
    if file(&diff, "b/a.py").change.as_ref().unwrap().kind() == FileChangeKind::Renamed {
        let parse = change(&diff, "b/a.py", "parse");
        assert_eq!(parse.kind(), SymbolChangeKind::Changed);
        assert_eq!(parse.old_symbol, "reef-tags . . . `a.py`/Parser#parse().");
        assert_eq!(parse.old_path, "");
    }
}

/// A library whose `parse` is called from three files and itself.
async fn callers(fx: &Fixture) -> String {
    fx.write(
        &fx.main,
        &[
            ("lib/parse.py", b"def parse(x):\n    if x:\n        return parse(x[1:])\n    return 1\n\n\nLIMIT = 3\n\n\ndef gone():\n    return 0\n"),
            ("app/one.py", b"from lib.parse import parse\n\n\ndef main():\n    return parse('a')\n"),
            ("app/two.py", b"def run():\n    return parse('b')\n\n\ndef use_gone():\n    return gone()\n"),
            ("app/three.py", b"class Handler:\n    def post(self):\n        return parse('c')\n"),
            ("app/main.go", b"package app\n\nfunc parse() {}\n\nfunc main() {\n\tparse()\n}\n"),
        ],
    )
    .await
}

// 10
#[tokio::test(flavor = "multi_thread")]
async fn call_sites_by_name() {
    let fx = start(|_| {}).await;
    let before = callers(&fx).await;
    let after = fx
        .write(
            &before,
            &[(
                "lib/parse.py",
                b"def parse(x):\n    if x:\n        return parse(x[2:])\n    return 1\n\n\nLIMIT = 4\n",
            )],
        )
        .await;
    let diff = fx.symbols(&before, &after).await;
    let parse = change(&diff, "lib/parse.py", "parse");
    assert_eq!(parse.call_site_status(), CallSiteStatus::Found);
    let sites: Vec<(&str, &str)> = parse
        .call_sites
        .iter()
        .map(|s| (s.path.as_str(), s.enclosing_symbol.as_str()))
        .collect();
    assert_eq!(
        sites,
        [
            ("app/one.py", "reef-tags . . . app/`one.py`/main()."),
            (
                "app/three.py",
                "reef-tags . . . app/`three.py`/Handler#post()."
            ),
            ("app/two.py", "reef-tags . . . app/`two.py`/run()."),
        ],
        "the recursive call and Go's parse are not call sites"
    );
    assert_eq!(parse.call_site_count, 3);
    assert!(
        parse
            .call_sites
            .iter()
            .all(|s| s.source() == SymbolSource::Tags)
    );
    assert_eq!(parse.name_definitions, 1);
    assert_eq!(
        change(&diff, "lib/parse.py", "LIMIT").call_site_status(),
        CallSiteStatus::NotTracked
    );
    let gone = change(&diff, "lib/parse.py", "gone");
    assert_eq!(gone.kind(), SymbolChangeKind::Removed);
    assert_eq!(gone.call_site_count, 1);
    assert_eq!(gone.name_definitions, 0);
    let summary = diff.symbol_summary.clone().unwrap();
    assert_eq!(
        summary.to_index.as_ref().unwrap().state(),
        SymbolIndexState::Complete
    );

    // A second `parse` in the family.
    let twice = fx
        .write(
            &after,
            &[(
                "app/other.py",
                b"class P:\n    def parse(self):\n        return 0\n",
            )],
        )
        .await;
    let diff = fx.symbols(&before, &twice).await;
    assert_eq!(change(&diff, "lib/parse.py", "parse").name_definitions, 2);
}

/// A SCIP document for a Python file defining or calling `target`.
fn scip_python(path: &str, defines: Option<(i32, i32)>, calls: &[i32]) -> Vec<u8> {
    let symbol = "scip-python python fixture 0.1 `lib.a`/target().";
    let mut occurrences = Vec::new();
    if let Some((first, last)) = defines {
        occurrences.push(scip::Occurrence {
            symbol: symbol.into(),
            symbol_roles: scip::SymbolRole::Definition as i32,
            typed_range: Some(scip::occurrence::TypedRange::SingleLineRange(
                scip::SingleLineRange {
                    line: first,
                    start_character: 4,
                    end_character: 10,
                },
            )),
            typed_enclosing_range: Some(
                scip::occurrence::TypedEnclosingRange::MultiLineEnclosingRange(
                    scip::MultiLineRange {
                        start_line: first,
                        start_character: 0,
                        end_line: last,
                        end_character: 12,
                    },
                ),
            ),
            ..scip::Occurrence::default()
        });
    }
    for line in calls {
        occurrences.push(scip::Occurrence {
            symbol: symbol.into(),
            typed_range: Some(scip::occurrence::TypedRange::SingleLineRange(
                scip::SingleLineRange {
                    line: *line,
                    start_character: 11,
                    end_character: 17,
                },
            )),
            ..scip::Occurrence::default()
        });
    }
    scip::Document {
        relative_path: path.into(),
        position_encoding: scip::PositionEncoding::Utf8CodeUnitOffsetFromLineStart as i32,
        occurrences,
        ..scip::Document::default()
    }
    .encode_to_vec()
}

async fn upload(fx: &Fixture, revision: &str, documents: Vec<(&str, &[u8], Vec<u8>)>) {
    let response = fx
        .client()
        .upload_scip_documents(UploadScipDocumentsRequest {
            repo: fx.repo.clone(),
            revision: revision.into(),
            tool: Some(ScipTool {
                name: "scip-python".into(),
                version: "0.6.0".into(),
            }),
            documents: documents
                .into_iter()
                .map(|(path, bytes, document)| ScipDocument {
                    path: path.into(),
                    content_digest: reef_symbols::content_digest(bytes),
                    document,
                })
                .collect(),
            ..UploadScipDocumentsRequest::default()
        })
        .await
        .unwrap();
    assert!(response.rejected.is_empty(), "{response:?}");
}

// 11
#[tokio::test(flavor = "multi_thread")]
async fn call_sites_scip() {
    let fx = start(|_| {}).await;
    let a_before: &[u8] = b"def target():\n    return 1\n";
    let a_after: &[u8] = b"def target():\n    return 2\n";
    // `b.py` calls through SCIP; `c.py` is tags only; `d.py` has a
    // same-named function SCIP knows is another symbol.
    let b: &[u8] = b"def run():\n    return target()\n";
    let c: &[u8] = b"def go():\n    return target()\n";
    let d: &[u8] = b"def other():\n    return target()\n";
    let before = fx
        .write(
            &fx.main,
            &[
                ("lib/a.py", a_before),
                ("b.py", b),
                ("c.py", c),
                ("d.py", d),
            ],
        )
        .await;
    let after = fx.write(&before, &[("lib/a.py", a_after)]).await;
    upload(
        &fx,
        &before,
        vec![
            (
                "lib/a.py",
                a_before,
                scip_python("lib/a.py", Some((0, 1)), &[]),
            ),
            ("b.py", b, scip_python("b.py", None, &[1])),
            (
                "d.py",
                d,
                scip::Document {
                    relative_path: "d.py".into(),
                    occurrences: vec![scip::Occurrence {
                        symbol: "scip-python python fixture 0.1 `d`/target().".into(),
                        typed_range: Some(scip::occurrence::TypedRange::SingleLineRange(
                            scip::SingleLineRange {
                                line: 1,
                                start_character: 11,
                                end_character: 17,
                            },
                        )),
                        ..scip::Occurrence::default()
                    }],
                    ..scip::Document::default()
                }
                .encode_to_vec(),
            ),
        ],
    )
    .await;
    upload(
        &fx,
        &after,
        vec![(
            "lib/a.py",
            a_after,
            scip_python("lib/a.py", Some((0, 1)), &[]),
        )],
    )
    .await;
    let diff = fx.symbols(&before, &after).await;
    assert_eq!(symbols(&diff, "lib/a.py").source(), SymbolSource::Scip);
    let target = change(&diff, "lib/a.py", "target");
    assert_eq!(target.kind(), SymbolChangeKind::Changed);
    let sites: Vec<(&str, SymbolSource)> = target
        .call_sites
        .iter()
        .map(|s| (s.path.as_str(), s.source()))
        .collect();
    assert_eq!(
        sites,
        [("b.py", SymbolSource::Scip), ("c.py", SymbolSource::Tags)],
        "d.py's target is another symbol"
    );
    // Its own definition counts, though SCIP answers for its file (review
    // finding 4).
    assert_eq!(target.name_definitions, 1);
}

/// A tags change's references found by name in a file SCIP answers for
/// keep SCIP's kind for them, the referenced symbol's (review finding 3).
#[tokio::test(flavor = "multi_thread")]
async fn scip_references_by_name() {
    let fx = start(|_| {}).await;
    let p_before: &[u8] = b"class Parser:\n    def parse(self):\n        return 1\n";
    let p_after: &[u8] = b"class Parser:\n    def parse(self):\n        return 2\n";
    let e: &[u8] = b"def run(p):\n    return p.parse()\n";
    let before = fx
        .write(&fx.main, &[("lib/p.py", p_before), ("e.py", e)])
        .await;
    let after = fx.write(&before, &[("lib/p.py", p_after)]).await;
    let document = scip::Document {
        relative_path: "e.py".into(),
        occurrences: vec![scip::Occurrence {
            symbol: "scip-python python fixture 0.1 `lib.p`/Parser#parse().".into(),
            typed_range: Some(scip::occurrence::TypedRange::SingleLineRange(
                scip::SingleLineRange {
                    line: 1,
                    start_character: 13,
                    end_character: 18,
                },
            )),
            ..scip::Occurrence::default()
        }],
        ..scip::Document::default()
    }
    .encode_to_vec();
    upload(&fx, &before, vec![("e.py", e, document)]).await;
    let diff = fx.symbols(&before, &after).await;
    let parse = change(&diff, "lib/p.py", "parse");
    let sites: Vec<(&str, SymbolSource)> = parse
        .call_sites
        .iter()
        .map(|s| (s.path.as_str(), s.source()))
        .collect();
    assert_eq!(sites, [("e.py", SymbolSource::Scip)], "{parse:?}");
}

/// `n` Python functions `f<i>` returning `value`.
fn functions(n: usize, value: u32) -> Vec<u8> {
    let mut text = String::new();
    for i in 0..n {
        let _ = write!(text, "def f{i}():\n    return {value}\n\n\n");
    }
    text.into_bytes()
}

// 12
#[tokio::test(flavor = "multi_thread")]
async fn call_site_bounds() {
    let fx = start(|_| {}).await;
    let before = callers(&fx).await;
    let after = fx
        .write(
            &before,
            &[("lib/parse.py", b"def parse(x):\n    return 2\n")],
        )
        .await;
    let request = |max: Option<u32>| GetDiffRequest {
        max_call_sites: max,
        ..fx.request(&before, &after)
    };
    let diff = fx.diff(request(Some(2))).await.unwrap();
    let parse = change(&diff, "lib/parse.py", "parse");
    assert_eq!((parse.call_sites.len(), parse.call_site_count), (2, 3));

    let lookups = fx.state().symbols.lookups();
    let diff = fx.diff(request(Some(0))).await.unwrap();
    let parse = change(&diff, "lib/parse.py", "parse");
    assert_eq!(parse.call_site_status(), CallSiteStatus::Unspecified);
    assert_eq!(parse.call_sites, [] as [v1::SymbolOccurrence; 0]);
    assert_eq!(fx.state().symbols.lookups(), lookups, "0 looks nothing up");
    let diff = fx.diff(request(Some(500))).await.unwrap();
    assert_eq!(change(&diff, "lib/parse.py", "parse").call_sites.len(), 3);

    // More candidates than are counted, over files small enough that no
    // parse nears the one-second budget on a slow runner.
    let mut files: Vec<(String, Vec<u8>)> = (0..101)
        .map(|i| {
            let mut calls = String::from("def caller():\n");
            for _ in 0..100 {
                calls.push_str("    common()\n");
            }
            (format!("calls/c{i:03}.py"), calls.into_bytes())
        })
        .collect();
    files.push((
        "common.py".to_owned(),
        b"def common():\n    return 1\n".to_vec(),
    ));
    let base = write_many(&fx, &fx.main, &files).await;
    let changed = fx
        .write(&base, &[("common.py", b"def common():\n    return 2\n")])
        .await;
    let diff = fx.symbols(&base, &changed).await;
    let common = change(&diff, "common.py", "common");
    assert_eq!(common.call_site_status(), CallSiteStatus::Capped);
    assert_eq!(common.call_site_count, 10_000);

    // 1,001 changes needing lookups on one page, and 1,001 in one file.
    let many = fx
        .write(
            &fx.main,
            &[("x.py", &functions(600, 0)), ("y.py", &functions(401, 0))],
        )
        .await;
    let many_after = fx
        .write(
            &many,
            &[("x.py", &functions(600, 1)), ("y.py", &functions(401, 1))],
        )
        .await;
    let diff = fx.symbols(&many, &many_after).await;
    let statuses: Vec<CallSiteStatus> = ["x.py", "y.py"]
        .iter()
        .flat_map(|p| {
            symbols(&diff, p)
                .changes
                .iter()
                .map(v1::SymbolChange::call_site_status)
        })
        .collect();
    assert_eq!(statuses.len(), 1001);
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == CallSiteStatus::Found)
            .count(),
        1000
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|s| **s == CallSiteStatus::PageLimit)
            .count(),
        1
    );
    let one = fx.write(&fx.main, &[("z.py", &functions(1001, 0))]).await;
    let one_after = fx.write(&one, &[("z.py", &functions(1001, 1))]).await;
    let diff = fx.symbols(&one, &one_after).await;
    let z = symbols(&diff, "z.py");
    assert!(z.truncated);
    assert_eq!(z.changes.len(), 1000);
    assert_eq!(diff.symbol_summary.as_ref().unwrap().changed, 1001);
}

// 13
#[tokio::test(flavor = "multi_thread")]
async fn coverage() {
    let fx = start(|_| {}).await;
    let large = {
        let mut text = functions(1, 0);
        text.resize(2 << 20, b'#');
        text
    };
    let before = fx
        .write(
            &fx.main,
            &[
                ("big.py", b"def f():\n    return 0\n"),
                ("bin.go", b"package x\n"),
                ("Main.java", b"class Main {}\n"),
                ("a.py", b"def g():\n    return 0\n"),
                ("broken.py", b"def h():\n    return 0\n"),
            ],
        )
        .await;
    let after = fx
        .edit(
            &before,
            vec![
                Edit::Write("big.py", large),
                Edit::Write("bin.go", b"package x\n\0\n".to_vec()),
                Edit::Write("Main.java", b"class Main { int x; }\n".to_vec()),
                Edit::Copy("a.txt", "a.py"),
                Edit::Delete("a.py"),
                Edit::Write("broken.py", b"def h(:\n    return 1\n".to_vec()),
            ],
        )
        .await;
    let diff = fx.symbols(&before, &after).await;
    let big = symbols(&diff, "big.py");
    assert_eq!(
        (big.coverage(), big.skip_reason()),
        (SymbolCoverage::Skipped, v1::SymbolSkipReason::TooLarge)
    );
    let bin = symbols(&diff, "bin.go");
    assert_eq!(
        (bin.coverage(), bin.skip_reason()),
        (SymbolCoverage::Skipped, v1::SymbolSkipReason::Binary)
    );
    assert_eq!(
        symbols(&diff, "Main.java").coverage(),
        SymbolCoverage::Unsupported
    );
    let renamed = diff
        .files
        .iter()
        .find(|f| f.change.as_ref().unwrap().path == "a.txt")
        .unwrap();
    let renamed = renamed.symbols.as_ref().unwrap();
    assert_eq!(renamed.coverage(), SymbolCoverage::Covered);
    assert!(
        renamed
            .changes
            .iter()
            .all(|c| c.kind() == SymbolChangeKind::Removed),
        "{renamed:?}"
    );
    assert!(symbols(&diff, "broken.py").parse_errors);
    let summary = diff.symbol_summary.clone().unwrap();
    let files = u32::try_from(diff.files.len()).unwrap();
    assert_eq!(
        summary.files_covered + summary.files_unsupported + summary.files_skipped,
        files
    );

    // SCIP forced on a tags-only file.
    let diff = fx
        .diff(GetDiffRequest {
            symbol_source: SymbolSource::Scip.into(),
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    assert_eq!(
        symbols(&diff, "broken.py").coverage(),
        SymbolCoverage::NoScip
    );
}

/// `n` small Python files under `dir`, each with `body`.
fn small_files(dir: &str, n: usize, body: &str) -> Vec<(String, Vec<u8>)> {
    (0..n)
        .map(|i| {
            (
                format!("{dir}/m{i:04}.py"),
                format!("def f():\n    return {body}\n").into_bytes(),
            )
        })
        .collect()
}

async fn write_many(fx: &Fixture, on: &str, files: &[(String, Vec<u8>)]) -> String {
    let mut at = on.to_owned();
    for chunk in files.chunks(1000) {
        let refs: Vec<(&str, &[u8])> = chunk
            .iter()
            .map(|(p, b)| (p.as_str(), b.as_slice()))
            .collect();
        at = fx.write(&at, &refs).await;
    }
    at
}

// 14
#[tokio::test(flavor = "multi_thread")]
async fn diff_too_large() {
    let fx = start(|config| config.file.symbols.background = false).await;
    let before = write_many(&fx, &fx.main, &small_files("m", 1001, "0")).await;
    let after = write_many(&fx, &before, &small_files("m", 1001, "1")).await;
    let extracted = fx.state().symbols.extracted();
    let diff = fx
        .diff(GetDiffRequest {
            page_size: 10,
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    assert!(diff.symbol_summary.as_ref().unwrap().diff_too_large);
    assert!(
        diff.files
            .iter()
            .all(|f| f.symbols.as_ref().unwrap().coverage() == SymbolCoverage::DiffTooLarge)
    );
    assert_eq!(
        fx.state().symbols.extracted(),
        extracted,
        "nothing extracted"
    );

    // Paths narrow it under the limit.
    let diff = fx
        .diff(GetDiffRequest {
            paths: vec!["m/m0001.py".into()],
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    assert!(!diff.symbol_summary.as_ref().unwrap().diff_too_large);
    assert_eq!(
        symbols(&diff, "m/m0001.py").coverage(),
        SymbolCoverage::Covered
    );
}

// 15
#[tokio::test(flavor = "multi_thread")]
async fn on_demand() {
    let fx = start(|config| config.file.symbols.background = false).await;
    let before = callers(&fx).await;
    let after = fx
        .write(
            &before,
            &[("lib/parse.py", b"def parse(x):\n    return 2\n")],
        )
        .await;
    fx.state().symbols.set_call_site_wait(Duration::ZERO);
    let extracted = fx.state().symbols.extracted();
    let diff = fx.symbols(&before, &after).await;
    assert_eq!(
        fx.state().symbols.extracted() - extracted,
        2,
        "both sides of the one changed file"
    );
    let parse = change(&diff, "lib/parse.py", "parse");
    assert_eq!(parse.call_site_status(), CallSiteStatus::IndexIncomplete);
    let to_index = diff
        .symbol_summary
        .as_ref()
        .unwrap()
        .to_index
        .as_ref()
        .unwrap();
    assert_ne!(to_index.state(), SymbolIndexState::Complete);

    fx.index(&after).await;
    let diff = fx.symbols(&before, &after).await;
    assert_eq!(
        change(&diff, "lib/parse.py", "parse").call_site_status(),
        CallSiteStatus::Found
    );
    assert_eq!(
        diff.symbol_summary
            .as_ref()
            .unwrap()
            .to_index
            .as_ref()
            .unwrap()
            .state(),
        SymbolIndexState::Complete
    );
}

// 16
#[tokio::test(flavor = "multi_thread")]
async fn paging() {
    let fx = start(|_| {}).await;
    let mut files = small_files("p", 30, "0");
    files[0].1 = b"def first():\n    return 0\n\n\ndef moving():\n    return 'm'\n".to_vec();
    let before = write_many(&fx, &fx.main, &files).await;
    let mut changed = small_files("p", 30, "1");
    changed[0].1 = b"def first():\n    return 1\n".to_vec();
    changed[29].1 = b"def f():\n    return 1\n\n\ndef moving():\n    return 'm'\n".to_vec();
    let after = write_many(&fx, &before, &changed).await;
    let whole = fx.symbols(&before, &after).await;
    let mut request = GetDiffRequest {
        page_size: 7,
        ..fx.request(&before, &after)
    };
    let mut pages = Vec::new();
    loop {
        let page = fx.diff(request.clone()).await.unwrap();
        // The counts are the same on every page; `to_index` is the page's.
        let counts = |s: &Option<v1::SymbolDiffSummary>| {
            s.clone().map(|s| v1::SymbolDiffSummary {
                to_index: None,
                ..s
            })
        };
        assert_eq!(counts(&page.symbol_summary), counts(&whole.symbol_summary));
        request.page_token = page.next_page_token.clone();
        pages.extend(page.files);
        if request.page_token.is_empty() {
            break;
        }
    }
    assert_eq!(pages, whole.files);
    let moving = change(&whole, "p/m0029.py", "moving");
    assert_eq!(moving.kind(), SymbolChangeKind::Moved);
    assert_eq!(moving.old_path, "p/m0000.py");

    // A token is bound to the symbol fields.
    let first = fx
        .diff(GetDiffRequest {
            page_size: 7,
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    let error = fx
        .diff(GetDiffRequest {
            page_size: 7,
            max_call_sites: Some(1),
            page_token: first.next_page_token,
            ..fx.request(&before, &after)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "INVALID_PAGE_TOKEN");
}

// 17
#[tokio::test(flavor = "multi_thread")]
#[expect(clippy::too_many_lines, reason = "one fixture, every expansion case")]
async fn expand() {
    let fx = start(|_| {}).await;
    let (before, after) = four_languages(&fx).await;
    let request = |entries: &[&str]| GetDiffRequest {
        format: DiffFormat::Hunks.into(),
        expand_symbols: entries.iter().map(|e| (*e).to_owned()).collect(),
        ..fx.request(&before, &after)
    };
    let full = fx
        .diff(GetDiffRequest {
            format: DiffFormat::Hunks.into(),
            symbols: false,
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    for entry in [
        "reef-tags . . . src/`parser.py`/Parser#parse().",
        "Parser#parse()",
        "parse",
    ] {
        let diff = fx.diff(request(&[entry])).await.unwrap();
        let paths: Vec<&str> = diff
            .files
            .iter()
            .map(|f| f.change.as_ref().unwrap().path.as_str())
            .collect();
        assert_eq!(paths, ["src/parser.py"], "{entry}");
        let hunks = &diff.files[0].hunks;
        assert_eq!(hunks.len(), 1, "{entry}");
        let changed: Vec<&str> = hunks[0]
            .lines
            .lines()
            .filter(|l| l.starts_with('+') || l.starts_with('-'))
            .collect();
        let whole = &file(&full, "src/parser.py").hunks;
        let all: String = whole.iter().map(|h| h.lines.as_str()).collect();
        for line in &changed {
            assert!(all.contains(line), "{line} is not in the file's diff");
        }
        assert_eq!(hunks[0].new_start, 11, "numbered in file lines");
    }
    let diff = fx.diff(request(&["helper"])).await.unwrap();
    assert!(
        diff.files[0].hunks[0]
            .lines
            .lines()
            .all(|l| l.starts_with('+'))
    );
    let diff = fx.diff(request(&["legacy"])).await.unwrap();
    assert!(
        diff.files[0].hunks[0]
            .lines
            .lines()
            .all(|l| l.starts_with('-'))
    );
    let diff = fx.diff(request(&["Parser", "parse"])).await.unwrap();
    assert_eq!(diff.files[0].hunks.len(), 1, "the class's hunks once");
    // An added symbol's hunk sits where the file's diff has it, and hunks
    // come in line order (review finding 5).
    let diff = fx
        .diff(request(&["legacy", "helper", "parse"]))
        .await
        .unwrap();
    let hunks = &diff.files[0].hunks;
    assert!(
        hunks.iter().all(|h| h.old_start > 0 && h.new_start > 0),
        "{hunks:?}"
    );
    assert!(
        hunks.windows(2).all(|w| w[0].new_start <= w[1].new_start),
        "{hunks:?}"
    );
    // A removed method inside an expanded class is shown with the class
    // (review finding 6).
    let diff = fx.diff(request(&["Parser", "legacy"])).await.unwrap();
    let removed: usize = diff.files[0]
        .hunks
        .iter()
        .map(|h| h.lines.matches("-    def legacy").count())
        .sum();
    assert_eq!(removed, 1, "{diff:?}");

    let diff = fx.diff(request(&["nothing_matches"])).await.unwrap();
    assert_eq!(diff.files, [] as [v1::FileDiff; 0]);

    // The expanded symbol's own end, not the file's (review finding 7).
    let open = fx
        .write(
            &fx.main,
            &[(
                "z.py",
                b"def a():\n    return 1\n\n\ndef b():\n    return 1",
            )],
        )
        .await;
    let open_after = fx
        .write(
            &open,
            &[(
                "z.py",
                b"def a():\n    return 2\n\n\ndef b():\n    return 2",
            )],
        )
        .await;
    let z = |entries: &[&str]| GetDiffRequest {
        format: DiffFormat::Hunks.into(),
        expand_symbols: entries.iter().map(|e| (*e).to_owned()).collect(),
        ..fx.request(&open, &open_after)
    };
    let a = fx.diff(z(&["a"])).await.unwrap();
    assert!(!a.files[0].new_no_final_newline && !a.files[0].old_no_final_newline);
    let b = fx.diff(z(&["b"])).await.unwrap();
    assert!(b.files[0].new_no_final_newline && b.files[0].old_no_final_newline);

    let summary = fx
        .diff(GetDiffRequest {
            format: DiffFormat::Summary.into(),
            ..request(&["parse"])
        })
        .await
        .unwrap_err();
    assert_eq!(
        (reason(&summary), field(&summary)),
        (
            "INVALID_SYMBOL_QUERY".to_owned(),
            "expand_symbols".to_owned()
        )
    );
    let empty = fx.diff(request(&[""])).await.unwrap_err();
    assert_eq!(field(&empty), "expand_symbols");
    let entries: Vec<String> = (0..101).map(|i| format!("s{i}")).collect();
    let refs: Vec<&str> = entries.iter().map(String::as_str).collect();
    let many = fx.diff(request(&refs)).await.unwrap_err();
    assert_eq!(field(&many), "expand_symbols");
    let calls = fx
        .diff(GetDiffRequest {
            symbols: false,
            max_call_sites: Some(3),
            ..fx.request(&before, &after)
        })
        .await
        .unwrap_err();
    assert_eq!(
        (reason(&calls), field(&calls)),
        (
            "INVALID_SYMBOL_QUERY".to_owned(),
            "max_call_sites".to_owned()
        )
    );
}

// 19
#[tokio::test(flavor = "multi_thread")]
async fn disabled() {
    let fx = start(|config| config.file.symbols.enabled = false).await;
    let (before, after) = four_languages(&fx).await;
    let plain = fx
        .diff(GetDiffRequest {
            symbols: false,
            ..fx.request(&before, &after)
        })
        .await
        .unwrap();
    assert_eq!(plain.files.len(), 5);
    let error = fx.diff(fx.request(&before, &after)).await.unwrap_err();
    assert_eq!(reason(&error), "SYMBOLS_DISABLED");
    let error = fx
        .diff(GetDiffRequest {
            symbols: false,
            format: DiffFormat::Hunks.into(),
            expand_symbols: vec!["parse".into()],
            ..fx.request(&before, &after)
        })
        .await
        .unwrap_err();
    assert_eq!(reason(&error), "SYMBOLS_DISABLED");
    let repo_id = fx.repo.parse().unwrap();
    assert!(
        !fx.state().symbols.path_of(repo_id).exists(),
        "no symbol file is created"
    );
}
