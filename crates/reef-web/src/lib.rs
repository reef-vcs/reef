//! The web UI's assets, embedded at build time (R2.18, ADR 0047).
//!
//! `npm run build` in `web/` writes the app into this crate's `assets/`,
//! which git ignores: CI's `web` job and the release build it, and every
//! other build embeds `placeholder/`, a page saying the UI was not built.
//! [`BUILT`] says which this build has; the daemon refuses a web session on
//! a placeholder build (`WEB_UI_NOT_BUILT`). Both have the same shape, an
//! `index.html` and immutable files under `assets/`, so the daemon serves
//! and tests them the same way.

/// One embedded file.
#[derive(Debug)]
pub struct Asset {
    /// Its path, `/`-separated and without a leading `/`: `index.html`,
    /// `assets/index-<hash>.js`.
    pub path: &'static str,
    /// Its contents.
    pub bytes: &'static [u8],
    /// Its `Content-Type`.
    pub content_type: &'static str,
    /// Whether its name changes with its contents (under `assets/`), so a
    /// browser may cache it for good.
    pub immutable: bool,
}

include!(concat!(env!("OUT_DIR"), "/assets.rs"));

/// The asset at `path` (without a leading `/`), if any.
pub fn get(path: &str) -> Option<&'static Asset> {
    ASSETS
        .binary_search_by(|asset| asset.path.cmp(path))
        .ok()
        .map(|index| &ASSETS[index])
}

/// `index.html`, which every client-side route serves.
pub fn index() -> &'static Asset {
    &ASSETS[INDEX]
}

/// Every embedded asset, sorted by path.
pub fn all() -> &'static [Asset] {
    ASSETS
}
