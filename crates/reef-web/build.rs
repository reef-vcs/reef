//! Embeds the web UI (R2.18, ADR 0047): `assets/` when `npm run build` has
//! written it (`assets/index.html` exists), else `placeholder/`. Writes
//! `assets.rs` into `OUT_DIR`: [`BUILT`] and one `Asset` per file, sorted by
//! path, with its content type from the extension and whether it is
//! immutable (under `assets/assets/`, where Vite puts hashed names).

use std::env;
use std::error::Error;
use std::fmt::Write as _;
use std::fs;
use std::path::{Path, PathBuf};

/// Content types by extension; any other extension fails the build, so a
/// file the server would send with a guessed type never ships.
const TYPES: &[(&str, &str)] = &[
    ("css", "text/css; charset=utf-8"),
    ("html", "text/html; charset=utf-8"),
    ("ico", "image/x-icon"),
    ("js", "text/javascript; charset=utf-8"),
    ("json", "application/json"),
    ("png", "image/png"),
    ("svg", "image/svg+xml"),
    ("txt", "text/plain; charset=utf-8"),
    ("woff2", "font/woff2"),
];

fn main() -> Result<(), Box<dyn Error>> {
    let manifest = PathBuf::from(env::var_os("CARGO_MANIFEST_DIR").ok_or("no CARGO_MANIFEST_DIR")?);
    let built = manifest.join("assets");
    let placeholder = manifest.join("placeholder");
    println!("cargo:rerun-if-changed=assets");
    println!("cargo:rerun-if-changed=placeholder");
    let is_built = built.join("index.html").is_file();
    let root = if is_built { built } else { placeholder };
    if !is_built {
        println!(
            "cargo:warning=the web UI is the placeholder; run `just web-build` before building for the real one"
        );
    }
    let mut files = Vec::new();
    walk(&root, &root, &mut files)?;
    files.sort();
    let index = files
        .iter()
        .position(|(path, _)| path == "index.html")
        .ok_or_else(|| {
            format!(
                "{} has no index.html; run `just web-build`, or `just web-clean` for the placeholder",
                root.display()
            )
        })?;

    let mut out = String::new();
    writeln!(
        out,
        "/// Whether the real app is embedded, not the placeholder."
    )?;
    writeln!(out, "pub const BUILT: bool = {is_built};")?;
    writeln!(out, "const INDEX: usize = {index};")?;
    writeln!(out, "static ASSETS: &[Asset] = &[")?;
    for (path, file) in &files {
        let extension = Path::new(path)
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or_default();
        let content_type = TYPES
            .iter()
            .find(|(e, _)| *e == extension)
            .map(|(_, t)| *t)
            .ok_or_else(|| {
                format!("{path}: no content type for .{extension}; add one to reef-web's build.rs")
            })?;
        let immutable = path.starts_with("assets/");
        writeln!(
            out,
            "    Asset {{ path: {path:?}, bytes: include_bytes!({file:?}), content_type: {content_type:?}, immutable: {immutable} }},"
        )?;
    }
    writeln!(out, "];")?;
    let dest = PathBuf::from(env::var_os("OUT_DIR").ok_or("no OUT_DIR")?).join("assets.rs");
    fs::write(dest, out)?;
    Ok(())
}

/// Every file under `dir`, as (`/`-separated path relative to `root`,
/// absolute path), skipping dot files.
fn walk(root: &Path, dir: &Path, files: &mut Vec<(String, String)>) -> Result<(), Box<dyn Error>> {
    for entry in fs::read_dir(dir).map_err(|e| format!("read {}: {e}", dir.display()))? {
        let entry = entry?;
        let path = entry.path();
        if entry.file_name().to_string_lossy().starts_with('.') {
            continue;
        }
        if path.is_dir() {
            walk(root, &path, files)?;
        } else {
            let relative = path
                .strip_prefix(root)?
                .components()
                .map(|c| c.as_os_str().to_string_lossy().into_owned())
                .collect::<Vec<_>>()
                .join("/");
            let absolute = path
                .to_str()
                .ok_or_else(|| format!("{} is not UTF-8", path.display()))?
                .to_owned();
            files.push((relative, absolute));
        }
    }
    Ok(())
}
