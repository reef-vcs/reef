//! Serving `reef-web`'s embedded assets (R2.18): an exact path serves its
//! asset; any other path whose last segment has no `.` serves `index.html`,
//! so the app's client-side routes survive a reload; anything else is
//! `404`. What a browser installs the app from is served by the remote
//! listener alone.

use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, StatusCode, header};

use super::body::Full;
use super::gate::plain;

/// What a browser installs the app from, and a service worker's script: an
/// app installed from the local listener would be at a run's name, dead at the
/// next start (R3.40).
const INSTALLED: [&str; 5] = [
    "app.webmanifest",
    "apple-touch-icon.png",
    "icon-192.png",
    "icon-512.png",
    "sw.js",
];

/// [`serve`], as the local listener does.
pub(crate) fn serve_local(path: &str, head: bool) -> http::Response<Body> {
    if INSTALLED.contains(&path.trim_start_matches('/')) {
        return plain(StatusCode::NOT_FOUND);
    }
    serve(path, head)
}

/// The answer to `GET` (or `HEAD`, without the body) of `path`.
pub(crate) fn serve(path: &str, head: bool) -> http::Response<Body> {
    let relative = path.trim_start_matches('/');
    let asset = if relative.is_empty() {
        Some(reef_web::index())
    } else {
        reef_web::get(relative).or_else(|| {
            let last = relative.rsplit('/').next().unwrap_or_default();
            (!last.contains('.')).then(reef_web::index)
        })
    };
    let Some(asset) = asset else {
        return plain(StatusCode::NOT_FOUND);
    };
    let cache = if asset.immutable {
        "public, max-age=31536000, immutable"
    } else if asset.path == "index.html" {
        "no-store"
    } else {
        "no-cache"
    };
    let body = if head {
        Body::empty()
    } else {
        Full::body(asset.bytes)
    };
    let mut response = http::Response::new(body);
    let headers = response.headers_mut();
    headers.insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static(asset.content_type),
    );
    headers.insert(header::CACHE_CONTROL, HeaderValue::from_static(cache));
    headers.insert(header::CONTENT_LENGTH, HeaderValue::from(asset.bytes.len()));
    response
}
