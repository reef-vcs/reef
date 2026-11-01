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
    // Raised from 450 KiB by R2.20 (the workspaces page), for R2.21's room.
    assert!(js <= 500 << 10, "JavaScript is {js} bytes, over 500 KiB");
}
