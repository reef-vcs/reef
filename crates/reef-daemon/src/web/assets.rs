//! Serving `reef-web`'s embedded assets (R2.18): an exact path serves its
//! asset; any other path whose last segment has no `.` serves `index.html`,
//! so the app's client-side routes survive a reload; anything else is
//! `404`.

use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, StatusCode, header};

use super::body::Full;

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
        let mut response = http::Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_FOUND;
        return response;
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
