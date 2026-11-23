//! Clearing what another process left in the browser at the web UI's
//! address (#346, ADR 0094).
//!
//! At a port that does not change, a process that held it while the daemon
//! was down may have left a service worker and cached pages there, which
//! answer every navigation to it before the daemon sees a request. So a
//! launch file does not open that address at once. It first loads
//! `/reset/<id>` from it as an image. The file is another origin, so no
//! worker of the address intercepts that request, and no process can have
//! prepared the file. The answer's `Clear-Site-Data` removes the address's
//! workers, caches and stored session. The file goes on only when the image
//! loads, which the daemon's answer alone makes it do; anything else leaves
//! a page that says what to do.
//!
//! Chromium honours the header there. Playwright's `WebKit` does not, and
//! relies on the gate's worker eviction and the app's own cleanup.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use rand::RngExt as _;
use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, StatusCode, header};

use super::body::Full;

/// The most resets waiting to be asked for; one more drops the oldest.
const MAX: usize = 64;

/// The path of a reset, before its id.
pub(crate) const PATH: &str = "/reset/";

/// An image, so that the launch file knows it loaded.
const IMAGE: &str = "<svg xmlns=\"http://www.w3.org/2000/svg\" width=\"1\" height=\"1\"></svg>";

/// The resets launch files may ask for, each once. The id keeps another
/// site from clearing the person's session.
#[derive(Debug, Default)]
pub(crate) struct Resets(Mutex<VecDeque<String>>);

impl Resets {
    /// A new reset's id.
    pub(crate) fn mint(&self) -> String {
        let id = format!("{:032x}", rand::rng().random::<u128>());
        let mut waiting = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        if waiting.len() >= MAX {
            waiting.pop_front();
        }
        waiting.push_back(id.clone());
        id
    }

    /// Whether `id` was waiting; it is not any more.
    fn take(&self, id: &str) -> bool {
        let mut waiting = self.0.lock().unwrap_or_else(PoisonError::into_inner);
        let found = waiting.iter().position(|waiting| waiting == id);
        found.is_some_and(|at| waiting.remove(at).is_some())
    }

    /// The answer to `GET /reset/<id>` at the web UI's address.
    pub(crate) fn answer(&self, id: &str) -> http::Response<Body> {
        if !self.take(id) {
            let mut response = http::Response::new(Body::empty());
            *response.status_mut() = StatusCode::NOT_FOUND;
            return response;
        }
        let mut response = http::Response::new(Full::body(IMAGE));
        let headers = response.headers_mut();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("image/svg+xml"),
        );
        headers.insert(
            "clear-site-data",
            HeaderValue::from_static("\"cache\", \"storage\""),
        );
        // The launch file is another origin, and another site.
        headers.insert(
            "cross-origin-resource-policy",
            HeaderValue::from_static("cross-origin"),
        );
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_is_answered_once_and_only_when_minted() {
        let resets = Resets::default();
        let id = resets.mint();
        assert_eq!(id.len(), 32);
        for unknown in ["", "0", &"0".repeat(32)] {
            let refused = resets.answer(unknown);
            assert_eq!(refused.status(), StatusCode::NOT_FOUND, "{unknown:?}");
            assert!(!refused.headers().contains_key("clear-site-data"));
        }
        let cleared = resets.answer(&id);
        assert_eq!(cleared.status(), StatusCode::OK);
        assert_eq!(
            cleared.headers()["clear-site-data"],
            "\"cache\", \"storage\""
        );
        assert_eq!(resets.answer(&id).status(), StatusCode::NOT_FOUND);
        // The oldest goes when too many wait.
        let first = resets.mint();
        for _ in 0..MAX {
            resets.mint();
        }
        assert_eq!(resets.answer(&first).status(), StatusCode::NOT_FOUND);
    }
}
