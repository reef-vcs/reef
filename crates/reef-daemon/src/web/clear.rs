//! Clearing what another process left in the browser at the web UI's
//! address (#346, ADR 0094).
//!
//! At a port that does not change, a process that held it while the daemon
//! was down may have left a service worker and cached pages there, which
//! answer every navigation to it before the daemon sees a request. So a
//! launch file does not open that address. It opens a page on the launch
//! listener, whose port the kernel picked for this run and no one could
//! have prepared. That page asks the web listener for `/reset/<id>`, a
//! request from another origin, which no worker of the address intercepts;
//! the answer's `Clear-Site-Data` removes the workers, the caches and the
//! stored session, and the page goes on to the session's URL. Chromium
//! honours the header there. Safari's engine does not, and relies on the gate's
//! worker eviction and the app's own cleanup.

use std::collections::VecDeque;
use std::sync::{Mutex, PoisonError};

use rand::RngExt as _;
use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, Method, StatusCode, header};

use super::body::Full;

/// The most resets waiting to be asked for; one more drops the oldest.
const MAX: usize = 64;

/// The path of a reset, before its id.
pub(crate) const PATH: &str = "/reset/";

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
        let mut response = http::Response::new(Body::empty());
        if !self.take(id) {
            *response.status_mut() = StatusCode::NOT_FOUND;
            return response;
        }
        *response.status_mut() = StatusCode::NO_CONTENT;
        let headers = response.headers_mut();
        headers.insert(
            "clear-site-data",
            HeaderValue::from_static("\"cache\", \"storage\""),
        );
        // The launch page is another origin of the same site.
        headers.insert(
            "cross-origin-resource-policy",
            HeaderValue::from_static("same-site"),
        );
        response
    }
}

/// The fragment a launch page goes on with: a reset's id, then the session
/// URL's path, query and fragment.
pub(crate) fn fragment(id: &str, session_path: &str) -> String {
    format!("#reset={id}&to={session_path}")
}

/// The launch page's script. It goes nowhere but to a path at the web UI's
/// own origin, whatever its fragment says.
const SCRIPT: &str = r#"const origin = document.querySelector('meta[name="reef-origin"]').content;
const [, id, to] = /^#reset=([0-9a-f]{32})&to=(\/.*)$/.exec(location.hash) ?? [];
history.replaceState(null, "", "/");
if (to) {
  fetch(`${origin}/reset/${id}`, {
    mode: "no-cors",
    credentials: "include",
    cache: "no-store",
    signal: AbortSignal.timeout(3000),
  })
    .catch(() => undefined)
    .then(() => location.replace(origin + to));
}
"#;

/// The launch listener's answer to anything, for the web UI at `origin`.
pub(crate) fn launch_page(method: &Method, path: &str, origin: &str) -> http::Response<Body> {
    let (content_type, body) = match path {
        _ if method != Method::GET => return status(StatusCode::METHOD_NOT_ALLOWED),
        "/" => (
            "text/html; charset=utf-8",
            format!(
                "<!doctype html>\n<html lang=\"en\">\n<head>\n<meta charset=\"utf-8\">\n\
                 <meta name=\"reef-origin\" content=\"{origin}\">\n<title>Reef</title>\n\
                 <script type=\"module\" src=\"/launch.js\"></script>\n</head>\n<body></body>\n</html>\n"
            ),
        ),
        "/launch.js" => ("text/javascript; charset=utf-8", SCRIPT.to_owned()),
        _ => return status(StatusCode::NOT_FOUND),
    };
    let mut response = http::Response::new(Full::body(body));
    let headers = response.headers_mut();
    headers.insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    let csp = format!(
        "default-src 'none'; script-src 'self'; connect-src {origin}; worker-src 'none'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'"
    );
    if let Ok(csp) = HeaderValue::from_str(&csp) {
        headers.insert("content-security-policy", csp);
    }
    response
}

fn status(status: StatusCode) -> http::Response<Body> {
    let mut response = http::Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_reset_is_answered_once_and_only_when_minted() {
        let resets = Resets::default();
        let id = resets.mint();
        assert_eq!(id.len(), 32);
        assert_eq!(resets.answer("").status(), StatusCode::NOT_FOUND);
        assert_eq!(
            resets.answer(&"0".repeat(32)).status(),
            StatusCode::NOT_FOUND
        );
        let cleared = resets.answer(&id);
        assert_eq!(cleared.status(), StatusCode::NO_CONTENT);
        assert_eq!(
            cleared.headers()["clear-site-data"],
            "\"cache\", \"storage\""
        );
        let again = resets.answer(&id);
        assert_eq!(again.status(), StatusCode::NOT_FOUND);
        assert!(!again.headers().contains_key("clear-site-data"));
        // The oldest goes when too many wait.
        let first = resets.mint();
        for _ in 0..MAX {
            resets.mint();
        }
        assert_eq!(resets.answer(&first).status(), StatusCode::NOT_FOUND);
    }
}
