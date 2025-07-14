use crate::connection::StreamCommand;
use crate::frame::Frame;
use crate::tagged_stream::TaggedStream;
use crate::Result;
use crate::{frame, StreamId};
use futures::channel::mpsc;
use futures::stream::{Fuse, SelectAll};
use futures::{ready, AsyncRead, AsyncWrite, SinkExt, StreamExt};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A [`Future`] that gracefully closes the yamux connection.
#[must_use]
pub struct Closing<T> {
    state: State,
    stream_receivers: SelectAll<TaggedStream<StreamId, mpsc::Receiver<StreamCommand>>>,
    pending_frames: VecDeque<Frame<()>>,
    socket: Fuse<frame::Io<T>>,
}

impl<T> Closing<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(
        stream_receivers: SelectAll<TaggedStream<StreamId, mpsc::Receiver<StreamCommand>>>,
        pending_frames: VecDeque<Frame<()>>,
        socket: Fuse<frame::Io<T>>,
    ) -> Self {
        Self {
            state: State::ClosingStreamReceiver,
            stream_receivers,
            pending_frames,
            socket,
        }
    }
}

impl<T> Future for Closing<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    type Output = Result<()>;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();

        loop {
            match this.state {
                State::ClosingStreamReceiver => {
                    for stream in this.stream_receivers.iter_mut() {
                        stream.inner_mut().close();
                    }
                    this.state = State::DrainingStreamReceiver;
                }

                State::DrainingStreamReceiver => {
                    match this.stream_receivers.poll_next_unpin(cx) {
                        Poll::Ready(Some((_, Some(StreamCommand::SendFrame(frame))))) => {
                            this.pending_frames.push_back(frame.into());
                        }
                        Poll::Ready(Some((id, Some(StreamCommand::CloseStream { ack })))) => {
                            this.pending_frames
                                .push_back(Frame::close_stream(id, ack).into());
                        }
                        Poll::Ready(Some((_, None))) => {}
                        Poll::Pending | Poll::Ready(None) => {
                            // No more frames from streams, append `Term` frame and flush them all.
                            this.pending_frames.push_back(Frame::term().into());
                            this.state = State::FlushingPendingFrames;
                            continue;
                        }
                    }
                }
                State::FlushingPendingFrames => {
                    ready!(this.socket.poll_ready_unpin(cx))?;

                    match this.pending_frames.pop_front() {
                        Some(frame) => this.socket.start_send_unpin(frame)?,
                        None => this.state = State::ClosingSocket,
                    }
                }
                State::ClosingSocket => {
                    ready!(this.socket.poll_close_unpin(cx))?;

                    return Poll::Ready(Ok(()));
                }
            }
        }
    }
}

enum State {
    ClosingStreamReceiver,
    DrainingStreamReceiver,
    FlushingPendingFrames,
    ClosingSocket,
}

#[cfg(test)]
mod tests {
    use super::*;
    use futures::future::poll_fn;
    use futures::FutureExt;

    struct Socket {
        written: Vec<u8>,
        closed: bool,
    }
    impl AsyncRead for Socket {
        fn poll_read(
            self: Pin<&mut Self>,
            _: &mut Context<'_>,
            _: &mut [u8],
        ) -> Poll<std::io::Result<usize>> {
            unimplemented!()
        }
    }
    impl AsyncWrite for Socket {
        fn poll_write(
            mut self: Pin<&mut Self>,
            _: &mut Context<'_>,
            buf: &[u8],
        ) -> Poll<std::io::Result<usize>> {
            assert!(!self.closed);
            self.written.extend_from_slice(buf);
            Poll::Ready(Ok(buf.len()))
        }

        fn poll_flush(self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            unimplemented!()
        }

        fn poll_close(mut self: Pin<&mut Self>, _: &mut Context<'_>) -> Poll<std::io::Result<()>> {
            assert!(!self.closed);
            self.closed = true;
            Poll::Ready(Ok(()))
        }
    }

    #[test]
    fn pending_frames() {
        let frame_pending = Frame::data(StreamId::new(1), vec![2]).unwrap().into();
        let frame_data = Frame::data(StreamId::new(3), vec![4]).unwrap().into();
        let frame_close = Frame::close_stream(StreamId::new(5), false).into();
        let frame_close_ack = Frame::close_stream(StreamId::new(6), true).into();
        let frame_term = Frame::term().into();
        fn encode(buf: &mut Vec<u8>, frame: &Frame<()>) {
            buf.extend_from_slice(&frame::header::encode(frame.header()));
            if frame.header().tag() == frame::header::Tag::Data {
                buf.extend_from_slice(frame.clone().into_data().body());
            }
        }
        let mut expected_written = vec![];
        encode(&mut expected_written, &frame_pending);
        encode(&mut expected_written, &frame_data);
        encode(&mut expected_written, &frame_close);
        encode(&mut expected_written, &frame_close_ack);
        encode(&mut expected_written, &frame_term);

        let receiver = |frame: &Frame<_>, command: StreamCommand| {
            TaggedStream::new(frame.header().stream_id(), {
                let (mut tx, rx) = mpsc::channel(1);
                tx.try_send(command).unwrap();
                rx
            })
        };

        let mut stream_receivers: SelectAll<_> = Default::default();
        stream_receivers.push(receiver(
            &frame_data,
            StreamCommand::SendFrame(frame_data.clone().into_data().left()),
        ));
        stream_receivers.push(receiver(
            &frame_close,
            StreamCommand::CloseStream { ack: false },
        ));
        stream_receivers.push(receiver(
            &frame_close_ack,
            StreamCommand::CloseStream { ack: true },
        ));
        let pending_frames = vec![frame_pending];
        let mut socket = Socket {
            written: vec![],
            closed: false,
        };
        let mut closing = Closing::new(
            stream_receivers,
            pending_frames.into(),
            frame::Io::new(crate::connection::Id(0), &mut socket).fuse(),
        );
        futures::executor::block_on(async { poll_fn(|cx| closing.poll_unpin(cx)).await.unwrap() });
        assert!(closing.pending_frames.is_empty());
        assert!(socket.closed);
        assert_eq!(socket.written, expected_written);
    }
}
