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
//!    `REMOTE_TOKEN_INVALID`; each refusal counts against the address. On a
//!    connection an address opened before its backoff, only a live token
//!    still passes: other callers may share the address. The header is
//!    removed.
//!    A pairing code (R3.40, `Bearer reef_pair_<code>`) is taken for
//!    `RedeemPairing` at `[remote] origin`, or refused with
//!    `PAIRING_CODE_INVALID`, which counts the same; with any other method
//!    it is no token, and no token calls `RedeemPairing`. A device's token
//!    is at the address it was paired at, or `REMOTE_ORIGIN_CHANGED`.
//! 9. The method is in the token's scope (`read`: `ACCESS_READ`; `device`:
//!    also [`super::rules::DEVICE`]; `write`: also `ACCESS_WRITE`; never
//!    `ACCESS_ADMIN`), or `REMOTE_ACCESS_DENIED`.
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
use super::pairing::{self, Pairing, Pairings};
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
    /// `[remote] origin` (R3.40).
    pub(crate) origin: Option<reef_policy::Origin>,
    pub(crate) tokens: Arc<Tokens>,
    pub(crate) pairings: Arc<Pairings>,
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

    /// Whether `request` came to `origin`, byte for byte. Where the caller
    /// says it is, so it stops a page elsewhere, not a program.
    fn is_at(request: &http::Request<Body>, origin: &str) -> bool {
        authority(request).is_some_and(|authority| {
            origin
                .strip_prefix("https://")
                .is_some_and(|at| at == authority)
        })
    }

    /// Counts a refusal against `peer`, and logs it when one is due.
    fn count(&self, peer: SocketAddr, path: &str, state: &'static str) {
        if let Some(logged) = self.failures.refused(peer.ip()) {
            audit::token_refused(peer, path, state, &logged);
        }
    }

    /// Step 8 for a pairing code.
    fn redeem(
        &self,
        request: &http::Request<Body>,
        code: &str,
        peer: SocketAddr,
    ) -> Result<Pairing, RemoteError> {
        let path = request.uri().path();
        if path != pairing::REDEEM {
            let state = super::Refusal::Unknown;
            self.count(peer, path, state.as_str());
            return Err(RemoteError::TokenInvalid { state });
        }
        // The address first: a code typed at another one is not used up there.
        let at_origin = self
            .origin
            .as_ref()
            .is_some_and(|origin| Self::is_at(request, origin.as_str()));
        let taken = if at_origin {
            self.pairings.take(code).map_err(|wrong| wrong.ended_all)
        } else {
            Err(false)
        };
        taken
            .inspect(|pairing| audit::pairing_taken(peer, &pairing.name))
            .map_err(|ended_all| {
                self.count(peer, path, "pairing_code");
                if ended_all {
                    audit::pairings_ended(peer);
                }
                RemoteError::PairingCodeInvalid
            })
    }

    /// Steps 8 to 10 for `request` from `peer`: the caller and its hold on
    /// the token, or a pairing whose code it holds, or the refusal.
    fn admit(
        &self,
        request: &http::Request<Body>,
        peer: SocketAddr,
    ) -> Result<Admitted, tonic::Status> {
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
    ) -> Result<Admitted, Denied> {
        let path = request.uri().path();
        let bearer = bearer(request.headers());
        // An address in backoff is closed at accept; on a connection it
        // opened before, what is not a live token is refused here, uncounted,
        // until the backoff ends. Its other callers' tokens still pass: they
        // may share the address with whoever sent the wrong ones.
        let blocked = self.failures.blocked(peer.ip());
        if blocked && self.tokens.verify(bearer, peer).is_err() {
            return Err(Denied::Remote(RemoteError::TokenInvalid {
                state: super::Refusal::Unknown,
            }));
        }
        if let Some(code) = bearer.and_then(pairing::code_of) {
            return self
                .redeem(request, code, peer)
                .map(Admitted::Pairing)
                .map_err(Denied::Remote);
        }
        let verified = match self.tokens.verify(bearer, peer) {
            Ok(verified) => verified,
            Err(state) => {
                self.count(peer, path, state.as_str());
                return Err(Denied::Remote(RemoteError::TokenInvalid { state }));
            }
        };
        if let Some(device_origin) = &verified.device_origin {
            let origin = self.origin.as_ref().map(reef_policy::Origin::as_str);
            if origin != Some(device_origin) || !Self::is_at(request, device_origin) {
                let error = RemoteError::OriginChanged {
                    device_origin: device_origin.clone(),
                    origin: origin.unwrap_or_default().to_owned(),
                };
                audit::refused(&verified.id, &verified.name, peer, path, &error);
                return Err(Denied::Remote(error));
            }
        }
        let access = reef_api::access(path);
        // A v1 method this daemon does not know: the client is newer. Said
        // as every listener says it, so the client names the version
        // mismatch rather than blaming the token's scope.
        if access.is_none() && path.starts_with("/reef.api.v1.") {
            return Err(Denied::Status(crate::error::unknown_method(path)));
        }
        let allowed = match (access, verified.scope) {
            (Some(Access::Read), _) => true,
            (Some(Access::Write), TokenScope::Write) => path != pairing::REDEEM,
            (Some(Access::Write), TokenScope::Device) => super::rules::device_may(path),
            _ => false,
        };
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
        Ok(Admitted::Token(caller, held))
    }
}

/// What [`Gate::admit`] let through.
enum Admitted {
    Token(Caller, Use),
    /// A redeem whose code was taken (R3.40).
    Pairing(Pairing),
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
        let held = match admitted {
            Ok(Admitted::Token(caller, held)) => {
                request.extensions_mut().insert(caller);
                Some(held)
            }
            Ok(Admitted::Pairing(pairing)) => {
                request.extensions_mut().insert(pairing);
                None
            }
            Err(error) => return Box::pin(async move { Ok(secure(refusal(error, grpc_web))) }),
        };
        // The service that was polled ready serves this request; a clone
        // takes its place.
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        Box::pin(async move {
            let response = inner.call(request).await?;
            let (parts, body) = response.into_parts();
            // A trailers-only answer (its status in the headers) is whole:
            // the hold ends now, and the body stays one that says it is
            // empty, so hyper ends the stream on the HEADERS frame. Wrapped,
            // hyper would send an empty DATA frame after it, which gRPC
            // clients other than tonic refuse (R2.06).
            if super::streams::is_trailers_only(&parts.headers) {
                drop(held);
                return Ok(secure(http::Response::from_parts(parts, body)));
            }
            let body = match held {
                Some(held) => super::streams::Holding::body(body, held),
                None => body,
            };
            Ok(secure(http::Response::from_parts(parts, body)))
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
