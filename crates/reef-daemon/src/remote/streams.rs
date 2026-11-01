//! Remote responses (R2.08): a body that holds its token's slots until it
//! ends ([`Holding`]), and [`RemoteCallLayer`], which runs inside
//! `tonic_web::GrpcWebLayer`, so it sees native gRPC whatever the client
//! spoke, and:
//!
//! - ends a stream whose token is revoked or expires: the body stops and
//!   its trailers say `PERMISSION_DENIED`, `REMOTE_TOKEN_INVALID`, within
//!   the expiry sweep's second. A unary call already running finishes.
//! - logs every WRITE call under the audit target, with its outcome read
//!   from the trailers.

use std::pin::Pin;
use std::task::{Context, Poll};

use futures::future::BoxFuture;
use http_body::{Frame, SizeHint};
use reef_api::v1::Access;
use std::future::Future as _;
use std::sync::{Arc, Mutex, PoisonError};

use tokio_util::sync::{CancellationToken, WaitForCancellationFutureOwned};
use tonic::body::Body;
use tonic::codegen::Bytes;
use tonic::codegen::http::{self, HeaderMap};
use tower::{Layer, Service};

use super::tokens::Use;
use super::{Caller, Refusal, RemoteError, audit};

/// `inner`, holding a token's slots until the body ends or is dropped.
#[derive(Debug)]
pub(crate) struct Holding {
    inner: Body,
    held: Option<Use>,
}

impl Holding {
    pub(crate) fn body(inner: Body, held: Use) -> Body {
        Body::new(Holding {
            inner,
            held: Some(held),
        })
    }
}

impl http_body::Body for Holding {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(polled, Poll::Ready(None)) {
            self.held = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        // Never reported ended up front: `Body::new` would drop an empty
        // body, and the hold with it, before the response is sent.
        false
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

/// The layer; see the module documentation.
#[derive(Clone, Copy, Debug, Default)]
pub(crate) struct RemoteCallLayer;

impl<S> Layer<S> for RemoteCallLayer {
    type Service = RemoteCall<S>;

    fn layer(&self, inner: S) -> RemoteCall<S> {
        RemoteCall { inner }
    }
}

/// [`RemoteCallLayer`]'s service.
#[derive(Clone, Debug)]
pub(crate) struct RemoteCall<S> {
    inner: S,
}

/// The trailers that end a stream whose token ended.
fn ended_trailers(state: Refusal) -> HeaderMap {
    let response: http::Response<Body> =
        crate::error::status(RemoteError::TokenInvalid { state }).into_http();
    let mut trailers = HeaderMap::new();
    for (name, value) in response.headers() {
        if name.as_str().starts_with("grpc-") {
            trailers.insert(name.clone(), value.clone());
        }
    }
    trailers
}

/// A stream that ends when its token does. The handler's body is dropped
/// by a task of its own as soon as the token ends, even if the client has
/// stopped reading (then hyper stops polling this body); the trailers are
/// sent when it next polls.
struct Revocable {
    inner: Arc<Mutex<Option<Body>>>,
    /// Kept across polls, so a revoke wakes the task polling this body.
    ended: Pin<Box<WaitForCancellationFutureOwned>>,
    /// Cancelled when this body is dropped, so the task stops waiting.
    dropped: CancellationToken,
    done: bool,
    tokens: std::sync::Weak<super::tokens::Tokens>,
    token_id: String,
}

impl Revocable {
    fn body(inner: Body, caller: &Caller) -> Body {
        let inner = Arc::new(Mutex::new(Some(inner)));
        let dropped = CancellationToken::new();
        let (held, ended, stop) = (
            Arc::downgrade(&inner),
            caller.ended.clone(),
            dropped.clone(),
        );
        tokio::spawn(async move {
            tokio::select! {
                () = ended.cancelled() => {
                    if let Some(inner) = held.upgrade() {
                        // The handler sees its receiver gone and stops.
                        drop(inner.lock().unwrap_or_else(PoisonError::into_inner).take());
                    }
                }
                () = stop.cancelled() => {}
            }
        });
        Body::new(Revocable {
            inner,
            ended: Box::pin(caller.ended.clone().cancelled_owned()),
            dropped,
            done: false,
            tokens: caller.tokens.clone(),
            token_id: caller.token_id.clone(),
        })
    }
}

impl Drop for Revocable {
    fn drop(&mut self) {
        self.dropped.cancel();
    }
}

impl http_body::Body for Revocable {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        if self.done {
            return Poll::Ready(None);
        }
        let ended = self.ended.as_mut().poll(cx).is_ready();
        let polled = {
            let mut inner = self.inner.lock().unwrap_or_else(PoisonError::into_inner);
            if ended {
                *inner = None;
            }
            inner.as_mut().map(|body| Pin::new(body).poll_frame(cx))
        };
        if let Some(polled) = polled {
            if matches!(polled, Poll::Ready(None)) {
                self.done = true;
            }
            return polled;
        }
        let state = self.tokens.upgrade().map_or(Refusal::Revoked, |tokens| {
            tokens.ended_state(&self.token_id)
        });
        self.done = true;
        Poll::Ready(Some(Ok(Frame::trailers(ended_trailers(state)))))
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::default()
    }
}

/// A WRITE call's body, logging its outcome from its trailers.
struct Audited {
    inner: Body,
    entry: Option<audit::Write>,
}

impl http_body::Body for Audited {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        match &polled {
            Poll::Ready(Some(Ok(frame))) => {
                if let Some(trailers) = frame.trailers_ref()
                    && let Some(entry) = self.entry.take()
                {
                    entry.finish(trailers);
                }
            }
            Poll::Ready(None | Some(Err(_))) => {
                if let Some(entry) = self.entry.take() {
                    entry.finish(&HeaderMap::new());
                }
            }
            Poll::Pending => {}
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        false
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}

impl<S> Service<http::Request<Body>> for RemoteCall<S>
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

    fn call(&mut self, request: http::Request<Body>) -> Self::Future {
        let caller = request.extensions().get::<Caller>().cloned();
        let path = request.uri().path().to_owned();
        let clone = self.inner.clone();
        let mut inner = std::mem::replace(&mut self.inner, clone);
        // Begun before the call: a client that cancels a write it started is
        // logged as cancelled, since the write may still finish.
        let entry = caller
            .as_ref()
            .filter(|_| reef_api::access(&path) == Some(Access::Write))
            .map(|caller| audit::Write::begin(caller, &path));
        if let Some(caller) = &caller
            && entry.is_none()
        {
            tracing::debug!(
                target: "reef_daemon::remote::audit",
                token_id = %caller.token_id,
                token_name = %caller.token_name,
                peer = %caller.peer,
                method = %path,
                "remote read"
            );
        }
        Box::pin(async move {
            let response = inner.call(request).await?;
            let Some(caller) = caller else {
                return Ok(response);
            };
            let (parts, body) = response.into_parts();
            let body = if let Some(entry) = entry {
                // A trailers-only answer carries its status in the headers.
                if parts.headers.contains_key("grpc-status") {
                    entry.finish(&parts.headers);
                    body
                } else {
                    Body::new(Audited {
                        inner: body,
                        entry: Some(entry),
                    })
                }
            } else {
                body
            };
            let body = if super::gate::is_streaming(&path) {
                Revocable::body(body, &caller)
            } else {
                body
            };
            Ok(http::Response::from_parts(parts, body))
        })
    }
}
