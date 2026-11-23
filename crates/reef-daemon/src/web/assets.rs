//! Serving `reef-web`'s embedded assets (R2.18): an exact path serves its
//! asset; any other path whose last segment has no `.` serves `index.html`,
//! so the app's client-side routes survive a reload; anything else is
//! `404`.
//!
//! The hashed files under `assets/` are served only under this run's own
//! prefix, `/a/<16 hex>/`, which `index.html` names them by (#346). At a
//! port that does not change, a process that held it while the daemon was
//! down may have had the browser cache its own script at `/assets/…` for a
//! year, and Reef's page would run it; no cache holds this run's URLs.

use rand::RngExt as _;
use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, StatusCode, header};

use super::body::Full;

/// How `index.html` names a file next to it.
const RELATIVE: &str = "=\"./";

/// The hashed files' directory.
const HASHED: &str = "assets/";

/// One run's assets.
#[derive(Debug)]
pub(crate) struct Assets {
    /// `a/<16 hex>/`.
    prefix: String,
    /// `index.html`, naming the hashed files under `prefix`.
    index: String,
}

impl Assets {
    /// The assets of a new run.
    pub(crate) fn new() -> Assets {
        let prefix = format!("a/{:016x}/", rand::rng().random::<u64>());
        // Every path is absolute in the page served, which answers any route.
        let index = String::from_utf8_lossy(reef_web::index().bytes)
            .replace(
                &format!("{RELATIVE}{HASHED}"),
                &format!("=\"/{prefix}{HASHED}"),
            )
            .replace(RELATIVE, "=\"/");
        Assets { prefix, index }
    }

    /// The answer to `GET` (or `HEAD`, without the body) of `path`.
    pub(crate) fn serve(&self, path: &str, head: bool) -> http::Response<Body> {
        let relative = path.trim_start_matches('/');
        let asset = match relative.strip_prefix(&self.prefix) {
            Some(hashed) => reef_web::get(hashed).filter(|asset| asset.immutable),
            None => reef_web::get(relative).filter(|asset| !asset.immutable),
        };
        let last = relative.rsplit('/').next().unwrap_or_default();
        let (content_type, cache, body) = match asset {
            Some(asset) if asset.path != reef_web::index().path => (
                asset.content_type,
                if asset.immutable {
                    "public, max-age=31536000, immutable"
                } else {
                    "no-cache"
                },
                asset.bytes.to_vec(),
            ),
            None if last.contains('.') => {
                let mut response = http::Response::new(Body::empty());
                *response.status_mut() = StatusCode::NOT_FOUND;
                return response;
            }
            Some(_) | None => (
                reef_web::index().content_type,
                "no-store",
                self.index.clone().into_bytes(),
            ),
        };
        let length = HeaderValue::from(body.len());
        let body = if head {
            Body::empty()
        } else {
            Full::body(body)
        };
        let mut response = http::Response::new(body);
        let headers = response.headers_mut();
        headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
        headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
        headers.insert(header::CONTENT_LENGTH, length);
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hashed_files_are_served_under_this_runs_prefix_only() {
        let assets = Assets::new();
        let hashed = reef_web::all().iter().find(|a| a.immutable).unwrap();
        let status = |path: &str| assets.serve(path, false).status();
        assert_eq!(
            status(&format!("/{}{}", assets.prefix, hashed.path)),
            StatusCode::OK
        );
        assert_eq!(status(&format!("/{}", hashed.path)), StatusCode::NOT_FOUND);
        let other = Assets::new();
        assert_ne!(other.prefix, assets.prefix);
        assert_eq!(
            status(&format!("/{}{}", other.prefix, hashed.path)),
            StatusCode::NOT_FOUND
        );
        // The prefix serves nothing else, and the page names no other path.
        let page = format!("/{}index.html", assets.prefix);
        assert_eq!(status(&page), StatusCode::NOT_FOUND);
        assert!(
            assets
                .index
                .contains(&format!("=\"/{}{HASHED}", assets.prefix))
        );
        assert!(!assets.index.contains(RELATIVE), "{}", assets.index);
        assert!(!assets.index.contains("=\"/assets/"), "{}", assets.index);
        for route in ["/", "/index.html", "/repos/abc/smartlog"] {
            let response = assets.serve(route, true);
            assert_eq!(response.status(), StatusCode::OK, "{route}");
            assert_eq!(response.headers()[header::CACHE_CONTROL], "no-store");
        }
    }
}
