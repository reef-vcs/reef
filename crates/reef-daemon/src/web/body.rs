//! Response bodies for the web listener: one whole buffer, and a body that
//! holds a session in use until it ends.

use std::future::Future as _;
use std::pin::Pin;
use std::task::{Context, Poll};

use http_body::{Frame, SizeHint};
use tokio_util::sync::WaitForCancellationFutureOwned;
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
/// so a session with a stream open never goes idle, and a stream ends when
/// its session does.
#[derive(Debug)]
pub(crate) struct Held {
    inner: Body,
    in_use: Option<InUse>,
    /// Fires when the session ends. `None` when it had already ended as the
    /// body was built, while the request that produced it was in flight.
    ended: Option<Pin<Box<WaitForCancellationFutureOwned>>>,
}

impl Held {
    pub(crate) fn body(inner: Body, in_use: InUse) -> Body {
        let ended = in_use.ended();
        Body::new(Held {
            inner,
            ended: (!ended.is_cancelled()).then(|| Box::pin(ended.cancelled_owned())),
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
        // A stream ends with its session, however the session ends. A session
        // that had already ended as this body was built has no `ended` to
        // watch: its response runs as it did before, and ends with `inner`.
        let ended_now = match self.ended.as_mut() {
            Some(ended) => ended.as_mut().poll(cx).is_ready(),
            None => false,
        };
        // Poll `inner` even when the session ended in this same poll: a frame
        // it produces now was ready while the session was live, so deliver it
        // before ending. A unary response built while its session was live is
        // exactly this case when the session ends before the first poll.
        let polled = Pin::new(&mut self.inner).poll_frame(cx);
        if ended_now {
            // End on the next poll: drop the guard, empty the body, and keep
            // no waker. A frame ready here still goes out first.
            self.ended = None;
            self.in_use = None;
            self.inner = Body::empty();
            return match polled {
                Poll::Ready(Some(frame)) => Poll::Ready(Some(frame)),
                // A pending `inner` would have left a waker on a body that is
                // now gone, so end here rather than return `Pending`.
                Poll::Pending | Poll::Ready(None) => Poll::Ready(None),
            };
        }
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

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::time::Duration;

    use http_body_util::BodyExt as _;

    use super::*;
    use crate::web::Limits;
    use crate::web::session::Sessions;

    /// A session ended while its request was in flight still delivers the
    /// response it produced, then ends.
    #[tokio::test]
    async fn a_session_that_ended_before_the_body_delivers_its_response() {
        let sessions = Arc::new(Sessions::new(Limits {
            max_sessions: 1,
            session_idle: Duration::from_hours(1),
            ..Limits::default()
        }));
        let (token, _) = sessions.mint();
        let (in_use, _) = sessions.use_token(&token).unwrap();
        // At the cap with the only session in use, minting ends it while the
        // request holding `in_use` is still in flight.
        sessions.mint();
        let mut body = Held::body(Full::body(Bytes::from_static(b"response")), in_use);
        let frame = body.frame().await.expect("a frame").expect("no error");
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"response"));
        assert!(body.frame().await.is_none(), "the body does not end");
    }

    /// A stream whose session ended before its body was built still delivers
    /// its frames and ends, even when its first poll is pending.
    #[tokio::test]
    async fn a_pending_stream_on_an_ended_session_still_finishes() {
        let sessions = Arc::new(Sessions::new(Limits {
            max_sessions: 1,
            session_idle: Duration::from_hours(1),
            ..Limits::default()
        }));
        let (token, _) = sessions.mint();
        let (in_use, _) = sessions.use_token(&token).unwrap();
        // At the cap with the only session in use, minting ends it while the
        // request holding `in_use` is still in flight.
        sessions.mint();

        // A body that stays pending on its first poll, then yields a frame and
        // ends, as a subscription's stream does.
        let (tx, rx) = tokio::sync::mpsc::channel::<Result<Frame<Bytes>, tonic::Status>>(1);
        tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(10)).await;
            // The receiver is gone if the body ended early.
            let _ = tx
                .send(Ok(Frame::data(Bytes::from_static(b"streamed"))))
                .await;
        });
        let inner = Body::new(http_body_util::StreamBody::new(
            tokio_stream::wrappers::ReceiverStream::new(rx),
        ));
        let mut body = Held::body(inner, in_use);

        let frame = tokio::time::timeout(Duration::from_secs(1), body.frame())
            .await
            .expect("the body woke after its session had ended")
            .expect("a frame")
            .expect("no error");
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"streamed"));
        assert!(
            tokio::time::timeout(Duration::from_secs(1), body.frame())
                .await
                .expect("the body ends")
                .is_none(),
            "the body does not end"
        );
    }

    /// A unary body built while its session was live, whose session ends
    /// before its first poll, still delivers the response, then ends.
    #[tokio::test]
    async fn a_unary_body_built_while_live_delivers_its_response() {
        let sessions = Arc::new(Sessions::new(Limits {
            max_sessions: 1,
            session_idle: Duration::from_hours(1),
            ..Limits::default()
        }));
        let (token, _) = sessions.mint();
        let (in_use, _) = sessions.use_token(&token).unwrap();
        // Built while live, so `ended` is `Some`.
        let mut body = Held::body(Full::body(Bytes::from_static(b"response")), in_use);
        // The session ends before the body's first poll.
        sessions.mint();
        let frame = body.frame().await.expect("a frame").expect("no error");
        assert_eq!(frame.into_data().unwrap(), Bytes::from_static(b"response"));
        assert!(
            body.frame().await.is_none(),
            "the body does not end after the response"
        );
    }
}
