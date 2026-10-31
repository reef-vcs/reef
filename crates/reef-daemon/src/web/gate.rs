//! What the web listener accepts (R2.18, ADR 0047): [`WebGateLayer`] sits
//! outside `tonic_web::GrpcWebLayer` and checks every request, in order:
//!
//! 1. `Host` (or HTTP/2's `:authority`) is exactly the bound authority, or
//!    `421`: DNS rebinding.
//! 2. No `Upgrade` (`400`) and no `OPTIONS` (`405`): no WebSocket, and
//!    every cross-origin preflight fails. No response carries CORS headers.
//! 3. `GET` and `HEAD` on any path but an API path serve the assets.
//! 4. An API path (`/reef.<package>.<Service>/<Rpc>`) is `POST` (`405`);
//!    any other path is `404` on `POST` and `405` otherwise.
//! 5. `Origin`, if sent, is the listener's own, and `Sec-Fetch-Site`, if
//!    sent, is `same-origin`, or `403`: a request from another page is
//!    refused before its token is looked at.
//! 6. The content type is binary gRPC-web (`application/grpc-web` or
//!    `application/grpc-web+proto`), or `415`.
//! 7. `authorization: Bearer <token>` names a live session, or
//!    `WEB_SESSION_INVALID`. The header is removed.
//! 8. The method is `ACCESS_READ`, or `WEB_ACCESS_DENIED`.
//! 9. The request passes, holding its session in use until its response
//!    body ends.
//!
//! Steps 7 and 8 answer as gRPC-web trailers-only responses, which
//! Connect-ES reads as typed errors; steps 1 to 6 are plain HTTP, since a
//! client that fails them is not a gRPC-web client of this origin. Every
//! response gets [`SECURITY_HEADERS`].

use std::sync::Arc;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use reef_api::v1::Access;
use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, Method, StatusCode, header};
use tower::{Layer, Service};

use super::WebError;
use super::assets;
use super::body::Held;
use super::session::Sessions;
use crate::error::status;

/// Headers on every response, assets and API alike.
pub(crate) const SECURITY_HEADERS: &[(&str, &str)] = &[
    (
        "content-security-policy",
        "default-src 'none'; script-src 'self'; style-src 'self'; img-src 'self' data:; font-src 'self'; connect-src 'self'; manifest-src 'self'; base-uri 'none'; form-action 'none'; frame-ancestors 'none'",
    ),
    ("x-content-type-options", "nosniff"),
    ("referrer-policy", "no-referrer"),
    ("x-frame-options", "DENY"),
    ("cross-origin-opener-policy", "same-origin"),
    ("cross-origin-resource-policy", "same-origin"),
    (
        "permissions-policy",
        "camera=(), microphone=(), geolocation=(), usb=(), payment=()",
    ),
];

/// The binary gRPC-web content types; text mode is refused.
pub(crate) const GRPC_WEB: [&str; 2] = ["application/grpc-web", "application/grpc-web+proto"];

/// What the gate checks against.
#[derive(Debug)]
pub(crate) struct Gate {
    /// The bound authority, `127.0.0.1:<port>` or `[::1]:<port>`.
    pub(crate) authority: String,
    /// `http://<authority>`.
    pub(crate) origin: String,
    pub(crate) sessions: Arc<Sessions>,
}

/// The gate as a tower layer; see the module documentation.
#[derive(Clone, Debug)]
pub(crate) struct WebGateLayer(pub(crate) Arc<Gate>);

impl<S> Layer<S> for WebGateLayer {
    type Service = WebGate<S>;

    fn layer(&self, inner: S) -> WebGate<S> {
        WebGate {
            gate: Arc::clone(&self.0),
            inner,
        }
    }
}

/// [`WebGateLayer`]'s service.
#[derive(Clone, Debug)]
pub(crate) struct WebGate<S> {
    gate: Arc<Gate>,
    inner: S,
}

/// An empty response with `status`.
pub(crate) fn plain(status: StatusCode) -> http::Response<Body> {
    let mut response = http::Response::new(Body::empty());
    *response.status_mut() = status;
    response
}

/// A gRPC-web trailers-only answer carrying `error`'s status and details.
fn refusal(error: WebError) -> http::Response<Body> {
    let mut response: http::Response<Body> = status(error).into_http();
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("application/grpc-web+proto"),
    );
    response
}

/// Whether `path` names an RPC: `/reef.<rest>/<Rpc>`, with no other `/`.
pub(crate) fn is_api_path(path: &str) -> bool {
    path.strip_prefix("/reef.")
        .is_some_and(|rest| rest.matches('/').count() == 1 && !rest.ends_with('/'))
}

/// The request's authority: `Host`, else HTTP/2's `:authority`.
pub(crate) fn authority<B>(request: &http::Request<B>) -> Option<&str> {
    match request.headers().get(header::HOST) {
        Some(host) => host.to_str().ok(),
        None => request.uri().authority().map(http::uri::Authority::as_str),
    }
}

/// The token of `authorization: Bearer <token>`.
pub(crate) fn bearer(headers: &http::HeaderMap) -> Option<&str> {
    let value = headers.get(header::AUTHORIZATION)?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    scheme
        .eq_ignore_ascii_case("bearer")
        .then_some(token.trim())
}

impl Gate {
    /// Steps 1 to 6: a refusal, the assets' answer, or `None` for a request
    /// that goes on to the session check.
    fn screen(&self, request: &http::Request<Body>) -> Option<http::Response<Body>> {
        if authority(request) != Some(self.authority.as_str()) {
            return Some(plain(StatusCode::MISDIRECTED_REQUEST));
        }
        let method = request.method();
        if request.headers().contains_key(header::UPGRADE) {
            return Some(plain(StatusCode::BAD_REQUEST));
        }
        if method == Method::OPTIONS {
            return Some(plain(StatusCode::METHOD_NOT_ALLOWED));
        }
        let path = request.uri().path();
        let api = is_api_path(path);
        if !api {
            return Some(if method == Method::GET || method == Method::HEAD {
                assets::serve(path, method == Method::HEAD)
            } else if method == Method::POST {
                plain(StatusCode::NOT_FOUND)
            } else {
                plain(StatusCode::METHOD_NOT_ALLOWED)
            });
        }
        if method != Method::POST {
            return Some(plain(StatusCode::METHOD_NOT_ALLOWED));
        }
        let headers = request.headers();
        let origin_ok = headers
            .get(header::ORIGIN)
            .is_none_or(|origin| origin.as_bytes() == self.origin.as_bytes());
        let site_ok = headers
            .get("sec-fetch-site")
            .is_none_or(|site| site.as_bytes() == b"same-origin");
        if !origin_ok || !site_ok {
            return Some(plain(StatusCode::FORBIDDEN));
        }
        let grpc_web = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .is_some_and(|value| GRPC_WEB.contains(&value));
        if !grpc_web {
            return Some(plain(StatusCode::UNSUPPORTED_MEDIA_TYPE));
        }
        None
    }
}

/// Adds [`SECURITY_HEADERS`], and `Cache-Control: no-store` unless the
/// response set its own.
pub(crate) fn secure(mut response: http::Response<Body>) -> http::Response<Body> {
    let headers = response.headers_mut();
    for (name, value) in SECURITY_HEADERS {
        headers.insert(*name, HeaderValue::from_static(value));
    }
    headers
        .entry(header::CACHE_CONTROL)
        .or_insert(HeaderValue::from_static("no-store"));
    response
}

impl<S> Service<http::Request<Body>> for WebGate<S>
where
    S: Service<http::Request<Body>, Response = http::Response<Body>> + Clone + Send + 'static,
    S::Future: Send + 'static,
{
    type Response = http::Response<Body>;
    type Error = S::Error;
    type Future = BoxFuture<'static, Result<http::Response<Body>, S::Error>>;

    fn poll_ready(&mut self, cx: &mut Context<'_>) -> Poll<Result<(), Self::Error>> {
        self.inner.poll_ready(cx)
    }

    fn call(&mut self, mut request: http::Request<Body>) -> Self::Future {
        if let Some(response) = self.gate.screen(&request) {
            return Box::pin(async move { Ok(secure(response)) });
        }
        // 7. The session.
        let used = bearer(request.headers()).and_then(|token| self.gate.sessions.use_token(token));
        request.headers_mut().remove(header::AUTHORIZATION);
        let Some((in_use, launch_file)) = used else {
            return Box::pin(async move { Ok(secure(refusal(WebError::SessionInvalid))) });
        };
        if let Some(path) = launch_file {
            super::launch::remove(&path);
        }
        // 8. Read methods only.
        let path = request.uri().path().to_owned();
        let access = match reef_api::access(&path) {
            Some(Access::Read) => None,
            Some(Access::Write) => Some("write"),
            Some(Access::Admin) => Some("admin"),
            _ => Some("unknown"),
        };
        if let Some(access) = access {
            drop(in_use);
            return Box::pin(async move {
                Ok(secure(refusal(WebError::AccessDenied {
                    method: path,
                    access,
                })))
            });
        }
        // 9. Pass, the session in use until the body ends. The service that
        // was polled ready serves this request; a clone takes its place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let response = inner.call(request).await?;
            let (parts, body) = response.into_parts();
            Ok(secure(http::Response::from_parts(
                parts,
                Held::body(body, in_use),
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn api_paths() {
        assert!(is_api_path("/reef.api.v1.DaemonService/GetDaemonStatus"));
        assert!(is_api_path("/reef.daemon.v0.Daemon/Status"));
        assert!(!is_api_path("/"));
        assert!(!is_api_path("/reef.api.v1.DaemonService/"));
        assert!(!is_api_path("/reef.api.v1.DaemonService/Get/x"));
        assert!(!is_api_path("/assets/index-abc.js"));
        assert!(!is_api_path("/repos/abc/smartlog"));
    }

    #[test]
    fn bearer_tokens() {
        let mut headers = http::HeaderMap::new();
        assert_eq!(bearer(&headers), None);
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer abc"),
        );
        assert_eq!(bearer(&headers), Some("abc"));
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static("Basic abc"));
        assert_eq!(bearer(&headers), None);
    }
}
