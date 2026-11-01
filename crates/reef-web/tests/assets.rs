//! The embedded table (R2.18): the placeholder's shape, and, when the app
//! is built, its shape and budget. CI's and the release's `web` jobs set
//! `REEF_WEB_EXPECT_BUILT=1`, so a placeholder there fails `built`.

#![allow(
    clippy::expect_used,
    clippy::assertions_on_constants,
    clippy::case_sensitive_file_extension_comparisons,
    reason = "test helpers; a panic is the failure report, and BUILT is the build's"
)]

use reef_web::{BUILT, all, get, index};

/// The placeholder's own sentence, which no real build contains.
const PLACEHOLDER: &str = "built without the web UI";

fn text(bytes: &[u8]) -> &str {
    std::str::from_utf8(bytes).expect("UTF-8")
}

#[test]
fn built() {
    if std::env::var_os("REEF_WEB_EXPECT_BUILT").is_some_and(|v| v == "1") {
        assert!(
            BUILT,
            "REEF_WEB_EXPECT_BUILT=1, but the placeholder is embedded: run `just web-build` first"
        );
    } else {
        eprintln!(
            "embedded: {}",
            if BUILT {
                "the built app"
            } else {
                "the placeholder"
            }
        );
    }
}

#[test]
fn placeholder_is_sound() {
    if BUILT {
        eprintln!("skipped: the built app is embedded");
        return;
    }
    let page = index();
    assert!(page.content_type.starts_with("text/html"));
    assert!(!page.immutable);
    let html = text(page.bytes);
    assert!(html.contains(PLACEHOLDER));
    assert!(html.contains("/assets/placeholder-0.css"));
    assert!(!html.contains("<script"));
    let css = get("assets/placeholder-0.css").expect("the placeholder's stylesheet");
    assert!(css.immutable);
    assert!(css.content_type.starts_with("text/css"));
}

/// `src` or `href` values in `html` that point at `/assets/`.
fn linked(html: &str, attribute: &str) -> Vec<String> {
    let needle = format!("{attribute}=\"/assets/");
    html.match_indices(&needle)
        .map(|(at, _)| {
            let start = at + needle.len() - "assets/".len();
            let end = html[start..].find('"').expect("closing quote") + start;
            html[start..end].to_owned()
        })
        .collect()
}

#[test]
fn table_is_sound() {
    if !BUILT {
        eprintln!("skipped: the placeholder is embedded");
        return;
    }
    let page = index();
    assert!(page.content_type.starts_with("text/html"));
    let html = text(page.bytes);
    // No inline script or handler, so `script-src 'self'` holds.
    for (at, _) in html.match_indices("<script") {
        let tag = &html[at..at + html[at..].find('>').expect("tag end")];
        assert!(tag.contains(" src="), "inline script in index.html: {tag}");
    }
    let lower = html.to_ascii_lowercase();
    for handler in [" onload=", " onerror=", " onclick=", " onmouseover="] {
        assert!(
            !lower.contains(handler),
            "inline handler {handler} in index.html"
        );
    }
    let scripts = linked(html, "src");
    assert_eq!(
        scripts.iter().filter(|s| s.ends_with(".js")).count(),
        1,
        "{scripts:?}"
    );
    let styles: Vec<String> = linked(html, "href")
        .into_iter()
        .filter(|s| s.ends_with(".css"))
        .collect();
    assert_eq!(styles.len(), 1, "{styles:?}");
    for path in scripts.iter().chain(&styles) {
        let asset = get(path).unwrap_or_else(|| panic!("{path} is linked but not embedded"));
        assert!(asset.immutable, "{path}");
    }
    for asset in all() {
        assert!(!asset.path.ends_with(".map"), "source map {}", asset.path);
        assert_eq!(
            asset.immutable,
            asset.path.starts_with("assets/"),
            "{}",
            asset.path
        );
        if asset.content_type.starts_with("text/") {
            assert!(!text(asset.bytes).contains(PLACEHOLDER), "{}", asset.path);
        }
    }
    let licenses = get("THIRD_PARTY_LICENSES.txt").expect("THIRD_PARTY_LICENSES.txt");
    assert!(text(licenses.bytes).contains("react"));
}

/// The little JSON Vite's manifest uses: objects, arrays, strings and
/// booleans. reef-web has no dependencies (`crate_graph`), so no serde.
#[derive(Debug)]
enum Json {
    Object(Vec<(String, Json)>),
    Array(Vec<Json>),
    String(String),
    Bool(bool),
}

impl Json {
    fn get(&self, key: &str) -> Option<&Json> {
        match self {
            Json::Object(fields) => fields.iter().find(|(k, _)| k == key).map(|(_, v)| v),
            _ => None,
        }
    }

    fn str(&self) -> &str {
        match self {
            Json::String(s) => s,
            other => panic!("not a string: {other:?}"),
        }
    }

    /// An array of strings at `key`, or none.
    fn strings(&self, key: &str) -> Vec<&str> {
        match self.get(key) {
            Some(Json::Array(items)) => items.iter().map(Json::str).collect(),
            _ => Vec::new(),
        }
    }

    fn is(&self, key: &str) -> bool {
        matches!(self.get(key), Some(Json::Bool(true)))
    }
}

fn parse(text: &str) -> Json {
    fn skip(s: &[u8], at: &mut usize) {
        while *at < s.len() && s[*at].is_ascii_whitespace() {
            *at += 1;
        }
    }
    fn string(s: &[u8], at: &mut usize) -> String {
        assert_eq!(s[*at], b'"');
        *at += 1;
        let mut out = Vec::new();
        while s[*at] != b'"' {
            if s[*at] == b'\\' {
                *at += 1;
                out.push(match s[*at] {
                    b'n' => b'\n',
                    b't' => b'\t',
                    other => other,
                });
            } else {
                out.push(s[*at]);
            }
            *at += 1;
        }
        *at += 1;
        String::from_utf8(out).expect("UTF-8")
    }
    fn value(s: &[u8], at: &mut usize) -> Json {
        skip(s, at);
        match s[*at] {
            b'{' => {
                *at += 1;
                let mut fields = Vec::new();
                loop {
                    skip(s, at);
                    if s[*at] == b'}' {
                        *at += 1;
                        return Json::Object(fields);
                    }
                    if s[*at] == b',' {
                        *at += 1;
                        skip(s, at);
                    }
                    let key = string(s, at);
                    skip(s, at);
                    assert_eq!(s[*at], b':');
                    *at += 1;
                    fields.push((key, value(s, at)));
                }
            }
            b'[' => {
                *at += 1;
                let mut items = Vec::new();
                loop {
                    skip(s, at);
                    if s[*at] == b']' {
                        *at += 1;
                        return Json::Array(items);
                    }
                    if s[*at] == b',' {
                        *at += 1;
                    }
                    items.push(value(s, at));
                }
            }
            b'"' => Json::String(string(s, at)),
            b't' => {
                *at += 4;
                Json::Bool(true)
            }
            b'f' => {
                *at += 5;
                Json::Bool(false)
            }
            other => panic!("unexpected {:?} in the manifest", other as char),
        }
    }
    let mut at = 0;
    value(text.as_bytes(), &mut at)
}

fn manifest() -> Json {
    parse(text(get("manifest.json").expect("manifest.json").bytes))
}

/// The pages that load lazily, by their source path in the manifest.
const PAGES: &[&str] = &[
    "src/views/RepoFrame.tsx",
    "src/views/Smartlog.tsx",
    "src/views/Fleet.tsx",
    "src/views/Activity.tsx",
    "src/views/States.tsx",
];

/// The chunks a route loads: the entry, each lazy chunk on its way (the
/// repository frame, then the page), and their static imports, transitively.
fn route_files(manifest: &Json, lazy: &[&str]) -> Vec<String> {
    let mut seen: Vec<String> = Vec::new();
    let mut stack: Vec<String> = vec!["index.html".to_owned()];
    stack.extend(lazy.iter().map(|&key| key.to_owned()));
    while let Some(key) = stack.pop() {
        let entry = manifest
            .get(&key)
            .unwrap_or_else(|| panic!("{key} not in the manifest"));
        let file = entry.get("file").expect("file").str().to_owned();
        if seen.contains(&file) {
            continue;
        }
        seen.push(file);
        stack.extend(entry.strings("imports").into_iter().map(str::to_owned));
    }
    seen
}

#[test]
fn manifest_is_sound() {
    if !BUILT {
        eprintln!("skipped: the placeholder is embedded");
        return;
    }
    let manifest = manifest();
    let Json::Object(entries) = &manifest else {
        panic!("the manifest is not an object")
    };
    for (key, entry) in entries {
        let file = entry.get("file").expect("file").str();
        assert!(get(file).is_some(), "{key}: {file} is not embedded");
        for css in entry.strings("css") {
            assert!(get(css).is_some(), "{key}: {css} is not embedded");
        }
    }
    let entry = manifest.get("index.html").expect("the entry");
    assert!(entry.is("isEntry"));
    let scripts = linked(text(index().bytes), "src");
    assert_eq!(
        scripts,
        vec![entry.get("file").expect("file").str().to_owned()]
    );
    for page in PAGES {
        assert!(
            manifest.get(page).is_some_and(|p| p.is("isDynamicEntry")),
            "{page} is not a lazy chunk"
        );
    }
}

#[test]
fn bundle_budget() {
    if !BUILT {
        eprintln!("skipped: the placeholder is embedded");
        return;
    }
    let total: usize = all().iter().map(|a| a.bytes.len()).sum();
    let js: usize = all()
        .iter()
        .filter(|a| a.path.ends_with(".js"))
        .map(|a| a.bytes.len())
        .sum();
    eprintln!("assets: {total} bytes, JavaScript: {js} bytes");
    // The licenses file is text the browser never loads; it is not app weight.
    let licenses = get("THIRD_PARTY_LICENSES.txt").map_or(0, |a| a.bytes.len());
    assert!(
        total - licenses <= 600 << 10,
        "assets are {} bytes, over 600 KiB",
        total - licenses
    );
    // What a page loads, not what is embedded (R2.21, one chunk per page).
    // 500 KiB since R2.20; per page since R2.21, decided with the user.
    let manifest = manifest();
    let routes: &[(&str, &[&str])] = &[
        ("/", &[]),
        (
            "/repos/<id>/smartlog",
            &["src/views/RepoFrame.tsx", "src/views/Smartlog.tsx"],
        ),
        (
            "/repos/<id>/workspaces",
            &["src/views/RepoFrame.tsx", "src/views/Fleet.tsx"],
        ),
        (
            "/repos/<id>/activity",
            &["src/views/RepoFrame.tsx", "src/views/Activity.tsx"],
        ),
        ("/states", &["src/views/States.tsx"]),
    ];
    for (route, lazy) in routes {
        let bytes: usize = route_files(&manifest, lazy)
            .iter()
            .map(|file| get(file).expect("embedded").bytes.len())
            .sum();
        eprintln!("{route}: {bytes} bytes of JavaScript");
        assert!(
            bytes <= 500 << 10,
            "{route} loads {bytes} bytes of JavaScript, over 500 KiB"
        );
    }
}
