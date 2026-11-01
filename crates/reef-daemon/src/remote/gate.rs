//! What the remote listener accepts (R2.08, ADR 0049): [`RemoteGateLayer`]
//! sits outside `tonic_web::GrpcWebLayer`, as R2.18's gate does on the
//! loopback listener, and checks every request in order:
//!
//! 1. (In the accept loop, before TLS: an address in backoff is closed.)
//! 2. `Host` (or `:authority`) is a name in `[remote] names` or an IP
//!    literal, or `421`. Any port: a port forward changes it.
//! 3. No `Upgrade` (`400`) and no `OPTIONS` (`405`); no CORS headers.
//! 4. `GET` and `HEAD` off API paths serve the assets, public.
//! 5. An API path is `POST`, else `405` (`404` for `POST` elsewhere).
//! 6. `Origin`, if sent, is `https://` and the request's own `Host`, and
//!    `Sec-Fetch-Site`, if sent, is `same-origin`, or `403`.
//! 7. The content type is binary gRPC-web or native gRPC, or `415`.
//! 8. `authorization: Bearer <token>` names a live token, or
//!    `REMOTE_TOKEN_INVALID`; each refusal counts against the address, and
//!    an address in backoff is refused here too, on a connection it opened
//!    before. The header is removed.
//! 9. The method is in the token's scope (`read`: `ACCESS_READ`; `write`:
//!    also `ACCESS_WRITE`; never `ACCESS_ADMIN`), or `REMOTE_ACCESS_DENIED`.
//! 10. The token is under its limits of calls and streams, or
//!     `REMOTE_BUSY`.
//! 11. The request passes with a [`Caller`] in its extensions, which a
//!     client cannot set, and any `reef-remote*` header removed; it holds
//!     its token's slots until the response body ends.
//!
//! Steps 8 to 10 answer as gRPC trailers-only responses, in gRPC-web's
//! content type for a gRPC-web request. Every response gets R2.18's
//! security headers.

use std::collections::HashSet;
use std::net::{IpAddr, SocketAddr};
use std::sync::{Arc, LazyLock};
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use prost::Message as _;
use reef_api::v1::Access;
use tonic::body::Body;
use tonic::codegen::http::{self, HeaderValue, Method, StatusCode, header};
use tower::{Layer, Service};

use super::failures::Failures;
use super::incoming::Peer;
use super::tokens::{Refused, Tokens, Use};
use super::{Caller, RemoteError, access_name, audit};
use crate::db::TokenScope;
use crate::error::status;
use crate::web::assets;
use crate::web::gate::{GRPC_WEB, authority, bearer, is_api_path, plain, secure};

/// The native gRPC content types.
const GRPC: [&str; 2] = ["application/grpc", "application/grpc+proto"];

/// The paths of every server-streaming v1 method.
static STREAMING: LazyLock<HashSet<String>> = LazyLock::new(|| {
    let set =
        prost_types::FileDescriptorSet::decode(reef_api::FILE_DESCRIPTOR_SET).unwrap_or_default();
    set.file
        .iter()
        .filter(|file| file.package() == "reef.api.v1")
        .flat_map(|file| {
            file.service.iter().flat_map(|service| {
                service
                    .method
                    .iter()
                    .filter(|method| method.server_streaming())
                    .map(|method| format!("/reef.api.v1.{}/{}", service.name(), method.name()))
            })
        })
        .collect()
});

/// Whether the method at `path` streams its response.
pub(crate) fn is_streaming(path: &str) -> bool {
    STREAMING.contains(path)
}

/// What the gate checks against.
#[derive(Debug)]
pub(crate) struct Gate {
    /// `[remote] names`, or the host name, lowercased.
    pub(crate) names: Vec<String>,
    pub(crate) tokens: Arc<Tokens>,
    pub(crate) failures: Arc<Failures>,
}

/// The gate as a tower layer; see the module documentation.
#[derive(Clone, Debug)]
pub(crate) struct RemoteGateLayer(pub(crate) Arc<Gate>);

impl<S> Layer<S> for RemoteGateLayer {
    type Service = RemoteGate<S>;

    fn layer(&self, inner: S) -> RemoteGate<S> {
        RemoteGate {
            gate: Arc::clone(&self.0),
            inner,
        }
    }
}

/// [`RemoteGateLayer`]'s service.
#[derive(Clone, Debug)]
pub(crate) struct RemoteGate<S> {
    gate: Arc<Gate>,
    inner: S,
}

/// A gRPC trailers-only answer carrying `status`, in the request's content
/// type.
fn refusal(status: tonic::Status, grpc_web: bool) -> http::Response<Body> {
    let mut response: http::Response<Body> = status.into_http();
    if grpc_web {
        response.headers_mut().insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/grpc-web+proto"),
        );
    }
    response
}

/// `authority` split into its host (brackets removed) and port.
fn host_and_port(authority: &str) -> Option<(&str, Option<u16>)> {
    if let Some(rest) = authority.strip_prefix('[') {
        let (host, after) = rest.split_once(']')?;
        let port = match after {
            "" => None,
            port => Some(port.strip_prefix(':')?.parse().ok()?),
        };
        return Some((host, port));
    }
    match authority.rsplit_once(':') {
        Some((host, port)) => Some((host, Some(port.parse().ok()?))),
        None => Some((authority, None)),
    }
}

impl Gate {
    /// Step 2: whether `authority` names this listener. The port is not
    /// checked: behind a port forward or a load balancer, clients use
    /// another port than the one bound, and the names are what stop a
    /// rebound page.
    fn serves(&self, authority: &str) -> bool {
        let Some((host, _)) = host_and_port(authority) else {
            return false;
        };
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        host.parse::<IpAddr>().is_ok() || self.names.contains(&host)
    }

    /// Steps 2 to 7: a refusal, the assets' answer, or `None` for a
    /// request that goes on to the token check, with whether it is
    /// gRPC-web.
    fn screen(&self, request: &http::Request<Body>) -> Result<bool, Box<http::Response<Body>>> {
        let Some(authority) = authority(request).filter(|a| self.serves(a)) else {
            return Err(Box::new(plain(StatusCode::MISDIRECTED_REQUEST)));
        };
        let method = request.method();
        if request.headers().contains_key(header::UPGRADE) {
            return Err(Box::new(plain(StatusCode::BAD_REQUEST)));
        }
        if method == Method::OPTIONS {
            return Err(Box::new(plain(StatusCode::METHOD_NOT_ALLOWED)));
        }
        let path = request.uri().path();
        if !is_api_path(path) {
            return Err(Box::new(
                if method == Method::GET || method == Method::HEAD {
                    assets::serve(path, method == Method::HEAD)
                } else if method == Method::POST {
                    plain(StatusCode::NOT_FOUND)
                } else {
                    plain(StatusCode::METHOD_NOT_ALLOWED)
                },
            ));
        }
        if method != Method::POST {
            return Err(Box::new(plain(StatusCode::METHOD_NOT_ALLOWED)));
        }
        let headers = request.headers();
        let own_origin = format!("https://{authority}");
        let origin_ok = headers.get(header::ORIGIN).is_none_or(|origin| {
            origin
                .as_bytes()
                .eq_ignore_ascii_case(own_origin.as_bytes())
        });
        let site_ok = headers
            .get("sec-fetch-site")
            .is_none_or(|site| site.as_bytes() == b"same-origin");
        if !origin_ok || !site_ok {
            return Err(Box::new(plain(StatusCode::FORBIDDEN)));
        }
        let content_type = headers
            .get(header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("");
        if GRPC_WEB.contains(&content_type) {
            Ok(true)
        } else if GRPC.contains(&content_type) {
            Ok(false)
        } else {
            Err(Box::new(plain(StatusCode::UNSUPPORTED_MEDIA_TYPE)))
        }
    }

    /// Steps 8 to 10 for `request` from `peer`: the caller and its hold on
    /// the token, or the refusal.
    fn admit(
        &self,
        request: &http::Request<Body>,
        peer: SocketAddr,
    ) -> Result<(Caller, Use), tonic::Status> {
        self.admit_inner(request, peer)
            .map_err(|denied| match denied {
                Denied::Remote(error) => status(error),
                Denied::Status(status) => status,
            })
    }

    fn admit_inner(
        &self,
        request: &http::Request<Body>,
        peer: SocketAddr,
    ) -> Result<(Caller, Use), Denied> {
        let path = request.uri().path();
        // An address in backoff is closed at accept; a connection it opened
        // before is refused here, uncounted, until the backoff ends.
        if self.failures.blocked(peer.ip()) {
            return Err(Denied::Remote(RemoteError::TokenInvalid {
                state: super::Refusal::Unknown,
            }));
        }
        let verified = match self.tokens.verify(bearer(request.headers()), peer) {
            Ok(verified) => verified,
            Err(state) => {
                if let Some(logged) = self.failures.refused(peer.ip()) {
                    audit::token_refused(peer, path, state, &logged);
                }
                return Err(Denied::Remote(RemoteError::TokenInvalid { state }));
            }
        };
        let access = reef_api::access(path);
        // A v1 method this daemon does not know: the client is newer. Said
        // as every listener says it, so the client names the version
        // mismatch rather than blaming the token's scope.
        if access.is_none() && path.starts_with("/reef.api.v1.") {
            return Err(Denied::Status(crate::error::unknown_method(path)));
        }
        let allowed = matches!(
            (access, verified.scope),
            (Some(Access::Read), _) | (Some(Access::Write), TokenScope::Write)
        );
        if !allowed {
            let error = RemoteError::AccessDenied {
                method: path.to_owned(),
                access: access_name(access),
                scope: verified.scope,
            };
            audit::refused(&verified.id, &verified.name, peer, path, &error);
            return Err(Denied::Remote(error));
        }
        let held = match self.tokens.hold(&verified.id, is_streaming(path)) {
            Ok(held) => held,
            Err(Refused::Busy(limit)) => {
                let error = RemoteError::Busy { limit };
                audit::refused(&verified.id, &verified.name, peer, path, &error);
                return Err(Denied::Remote(error));
            }
            Err(Refused::Token(state)) => {
                return Err(Denied::Remote(RemoteError::TokenInvalid { state }));
            }
        };
        let caller = Caller {
            token_id: verified.id,
            token_name: verified.name,
            scope: verified.scope,
            peer,
            ended: verified.ended,
            tokens: Arc::downgrade(&self.tokens),
        };
        Ok((caller, held))
    }
}

/// Why [`Gate::admit_inner`] refused.
enum Denied {
    Remote(RemoteError),
    Status(tonic::Status),
}

impl<S> Service<http::Request<Body>> for RemoteGate<S>
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
        let grpc_web = match self.gate.screen(&request) {
            Ok(grpc_web) => grpc_web,
            Err(response) => return Box::pin(async move { Ok(secure(*response)) }),
        };
        let Some(peer) = request.extensions().get::<Peer>().cloned() else {
            // Every connection of this listener has a peer; refuse rather
            // than serve one that does not.
            return Box::pin(async move { Ok(secure(plain(StatusCode::FORBIDDEN))) });
        };
        let admitted = self.gate.admit(&request, peer.addr);
        if admitted.is_ok() {
            peer.authenticated();
        }
        let headers = request.headers_mut();
        headers.remove(header::AUTHORIZATION);
        let forged: Vec<_> = headers
            .keys()
            .filter(|name| name.as_str().starts_with("reef-remote"))
            .cloned()
            .collect();
        for name in forged {
            headers.remove(name);
        }
        let (caller, held) = match admitted {
            Ok(admitted) => admitted,
            Err(error) => return Box::pin(async move { Ok(secure(refusal(error, grpc_web))) }),
        };
        request.extensions_mut().insert(caller);
        // The service that was polled ready serves this request; a clone
        // takes its place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let response = inner.call(request).await?;
            let (parts, body) = response.into_parts();
            Ok(secure(http::Response::from_parts(
                parts,
                super::streams::Holding::body(body, held),
            )))
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hosts() {
        assert_eq!(host_and_port("vm:7470"), Some(("vm", Some(7470))));
        assert_eq!(host_and_port("vm"), Some(("vm", None)));
        assert_eq!(host_and_port("[::1]:9"), Some(("::1", Some(9))));
        assert_eq!(host_and_port("[::1]"), Some(("::1", None)));
        assert_eq!(host_and_port("vm:x"), None);
    }

    #[test]
    fn streaming_methods() {
        assert!(is_streaming(
            "/reef.api.v1.OperationService/SubscribeOperations"
        ));
        assert!(is_streaming(
            "/reef.api.v1.PresenceService/SubscribePresence"
        ));
        assert!(!is_streaming("/reef.api.v1.DaemonService/GetDaemonStatus"));
    }
}
