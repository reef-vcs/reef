//! Response bodies for the web listener: one whole buffer, and a body that
//! holds a session in use until it ends.

use std::pin::Pin;
use std::task::{Context, Poll};

use http_body::{Frame, SizeHint};
use tonic::body::Body;
use tonic::codegen::Bytes;

use super::session::InUse;

/// One buffer, sent whole.
#[derive(Debug)]
pub(crate) struct Full(Option<Bytes>);

impl Full {
    pub(crate) fn body(bytes: impl Into<Bytes>) -> Body {
        Body::new(Full(Some(bytes.into())))
    }
}

impl http_body::Body for Full {
    type Data = Bytes;
    type Error = std::convert::Infallible;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        _cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        Poll::Ready(self.0.take().map(|bytes| Ok(Frame::data(bytes))))
    }

    fn is_end_stream(&self) -> bool {
        self.0.as_ref().is_none_or(Bytes::is_empty)
    }

    fn size_hint(&self) -> SizeHint {
        SizeHint::with_exact(self.0.as_ref().map_or(0, |b| b.len() as u64))
    }
}

/// `inner`, holding its session in use until the body ends or is dropped,
/// so a session with a stream open never goes idle.
#[derive(Debug)]
pub(crate) struct Held {
    inner: Body,
    in_use: Option<InUse>,
}

impl Held {
    pub(crate) fn body(inner: Body, in_use: InUse) -> Body {
        Body::new(Held {
            inner,
            in_use: Some(in_use),
        })
    }
}

impl http_body::Body for Held {
    type Data = Bytes;
    type Error = tonic::Status;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if matches!(polled, Poll::Ready(None)) {
            self.in_use = None;
        }
        polled
    }

    fn is_end_stream(&self) -> bool {
        // Never reported ended up front: `Body::new` would drop an empty
        // body, and the guard with it, before the response is sent.
        false
    }

    fn size_hint(&self) -> SizeHint {
        self.inner.size_hint()
    }
}
