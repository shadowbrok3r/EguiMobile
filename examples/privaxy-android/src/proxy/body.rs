//! A bounded inspection tee that preserves HTTP frames, including HTTP/2/gRPC trailers.

use super::session::header_pairs;
use super::state::Exchange;
use bytes::Bytes;
use hyper::body::{Body, Frame, SizeHint};
use std::future::Future;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll, ready};

pub struct CaptureBody<B> {
    inner: Pin<Box<B>>,
    exchange: Arc<Mutex<Exchange>>,
    request: bool,
    finished: bool,
    pending: Option<(Frame<Bytes>, tokio::task::JoinHandle<()>)>,
}

impl<B: Body> CaptureBody<B> {
    pub fn new(inner: B, exchange: Arc<Mutex<Exchange>>, request: bool) -> Self {
        let finished = inner.is_end_stream() || inner.size_hint().exact() == Some(0);
        // Hyper can finish a Content-Length: 0 response without ever polling its body.
        if !request
            && (inner.is_end_stream() || inner.size_hint().exact() == Some(0))
            && let Ok(mut open) = exchange.lock()
        {
            open.finished_at = Some(chrono::Local::now());
        }
        Self {
            inner: Box::pin(inner),
            exchange,
            request,
            finished,
            pending: None,
        }
    }
}

impl<B> Drop for CaptureBody<B> {
    fn drop(&mut self) {
        if !self.finished
            && let Ok(open) = self.exchange.lock()
        {
            let body = if self.request {
                &open.request_body
            } else {
                &open.response_body
            };
            body.mark_incomplete("Capture ended before this body completed.".into());
        }
    }
}

impl<B> Body for CaptureBody<B>
where
    B: Body<Data = Bytes>,
    B::Error: std::fmt::Display,
{
    type Data = Bytes;
    type Error = B::Error;

    fn poll_frame(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
    ) -> Poll<Option<Result<Frame<Bytes>, Self::Error>>> {
        let frame = if let Some((_, write)) = &mut self.pending {
            if let Err(error) = ready!(Pin::new(write).poll(cx)) {
                let message = format!("Body storage worker failed: {error}");
                if let Ok(open) = self.exchange.lock() {
                    let body = if self.request {
                        &open.request_body
                    } else {
                        &open.response_body
                    };
                    body.mark_incomplete(message);
                }
            }
            Some(Ok(self.pending.take().unwrap().0))
        } else {
            let frame = ready!(self.inner.as_mut().poll_frame(cx));
            if let Some(Ok(frame)) = frame {
                if let Some(data) = frame.data_ref().filter(|data| !data.is_empty()) {
                    let body = self.exchange.lock().ok().map(|open| {
                        if self.request {
                            open.request_body.clone()
                        } else {
                            open.response_body.clone()
                        }
                    });
                    if let Some(body) = body.filter(|body| body.enabled()) {
                        let bytes = data.clone();
                        let write = tokio::task::spawn_blocking(move || body.push(&bytes));
                        self.pending = Some((frame, write));
                        return self.poll_frame(cx);
                    }
                }
                Some(Ok(frame))
            } else {
                frame
            }
        };
        if let Ok(mut open) = self.exchange.lock() {
            match &frame {
                Some(Ok(frame)) => {
                    if let Some(trailers) = frame.trailers_ref() {
                        if self.request {
                            open.request_trailers = header_pairs(trailers);
                        } else {
                            open.response_trailers = header_pairs(trailers);
                        }
                    }
                }
                Some(Err(error)) => {
                    let message = format!(
                        "{} body failed: {error}",
                        if self.request { "Request" } else { "Response" }
                    );
                    let body = if self.request {
                        &open.request_body
                    } else {
                        &open.response_body
                    };
                    body.mark_incomplete(message.clone());
                    open.fail(message);
                }
                None => {}
            }
            // Trailers terminate an HTTP body; Hyper need not poll us again after forwarding
            // them, even if the decoder wrapper still reports is_end_stream() == false.
            let trailers = frame
                .as_ref()
                .is_some_and(|frame| frame.as_ref().is_ok_and(|frame| frame.is_trailers()));
            if !self.request
                && (frame.is_none()
                    || trailers
                    || self.inner.is_end_stream()
                    || self.inner.size_hint().exact() == Some(0))
            {
                open.finished_at = Some(chrono::Local::now());
            }
        }
        self.finished = frame.is_none()
            || frame
                .as_ref()
                .is_some_and(|frame| frame.as_ref().map_or(true, |frame| frame.is_trailers()))
            || self.inner.is_end_stream()
            || self.inner.size_hint().exact() == Some(0);
        Poll::Ready(frame)
    }

    fn is_end_stream(&self) -> bool {
        self.pending.is_none() && self.inner.is_end_stream()
    }

    fn size_hint(&self) -> SizeHint {
        let mut hint = self.inner.size_hint();
        if let Some((frame, _)) = &self.pending {
            let len = frame.data_ref().map_or(0, |data| data.len() as u64);
            if let Some(upper) = hint.upper() {
                hint.set_upper(upper.saturating_add(len));
            }
            hint.set_lower(hint.lower().saturating_add(len));
        }
        hint
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use http_body_util::{BodyExt, Empty, StreamBody};

    #[tokio::test]
    async fn a_full_capture_disk_does_not_interrupt_forwarded_traffic() {
        let store = crate::proxy::storage::BodyStore::temporary(1);
        let exchange = Arc::new(Mutex::new(Exchange {
            response_body: store.body(),
            ..Exchange::default()
        }));
        let source = http_body_util::Full::new(Bytes::from_static(b"all bytes still forwarded"));
        let output = CaptureBody::new(source, exchange.clone(), false)
            .collect()
            .await
            .unwrap()
            .to_bytes();
        assert_eq!(output, "all bytes still forwarded");
        let open = exchange.lock().unwrap();
        assert!(open.response_body.snapshot().error.is_some());
        assert!(open.error.is_none());
        assert!(open.finished_at.is_some());
    }

    #[tokio::test]
    async fn records_and_preserves_trailers() {
        for request in [true, false] {
            let exchange = Arc::new(Mutex::new(Exchange::for_test()));
            let mut trailers = http::HeaderMap::new();
            trailers.insert("grpc-status", "0".parse().unwrap());
            let frames: Vec<Result<_, std::io::Error>> = vec![
                Ok(Frame::data(Bytes::from_static(b"payload"))),
                Ok(Frame::trailers(trailers.clone())),
            ];
            let body = StreamBody::new(futures::stream::iter(frames));
            let collected = CaptureBody::new(body, exchange.clone(), request)
                .collect()
                .await
                .unwrap();
            assert_eq!(collected.trailers(), Some(&trailers));
            assert_eq!(collected.to_bytes(), "payload");
            let open = exchange.lock().unwrap();
            let (body, captured) = if request {
                (&open.request_body, &open.request_trailers)
            } else {
                (&open.response_body, &open.response_trailers)
            };
            assert_eq!(body.snapshot().read_range(0, 100).unwrap(), b"payload");
            assert_eq!(captured, &vec![("grpc-status".into(), "0".into())]);
            assert_eq!(open.finished_at.is_some(), !request);
        }
    }

    #[tokio::test]
    async fn empty_and_failed_responses_have_a_terminal_state() {
        let empty = Arc::new(Mutex::new(Exchange::for_test()));
        let _ = CaptureBody::new(Empty::<Bytes>::new(), empty.clone(), false);
        assert!(empty.lock().unwrap().finished_at.is_some());
        let failed = Arc::new(Mutex::new(Exchange::for_test()));
        let frames = futures::stream::iter(vec![
            Ok(Frame::data(Bytes::from_static(b"prefix"))),
            Err(std::io::Error::other("connection reset")),
        ]);
        assert!(
            CaptureBody::new(StreamBody::new(frames), failed.clone(), false)
                .collect()
                .await
                .is_err()
        );
        let open = failed.lock().unwrap();
        assert_eq!(
            open.response_body.snapshot().read_range(0, 100).unwrap(),
            b"prefix"
        );
        assert!(open.error.as_ref().unwrap().contains("connection reset"));
        assert!(open.response_body.snapshot().require_complete().is_err());
        assert!(open.finished_at.is_some());
    }
}
