use crate::connection::StreamCommand;
use crate::frame;
use crate::frame::Frame;
use crate::Result;
use futures::channel::mpsc;
use futures::stream::{Fuse, SelectAll};
use futures::{ready, AsyncRead, AsyncWrite, SinkExt, StreamExt};
use std::collections::VecDeque;
use std::future::Future;
use std::mem;
use std::pin::Pin;
use std::task::{Context, Poll, Waker};

/// A [`Future`] that gracefully closes the yamux connection.
#[must_use]
pub struct Closing<T> {
    state: State,
    stream_receivers: SelectAll<mpsc::Receiver<StreamCommand>>,
    pending_frames: VecDeque<Frame<()>>,
    pending_flush_wakers: Vec<Waker>,
    socket: Fuse<frame::Io<T>>,
}

impl<T> Closing<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(
        stream_receiver: SelectAll<mpsc::Receiver<StreamCommand>>,
        pending_frames: VecDeque<Frame<()>>,
        socket: Fuse<frame::Io<T>>,
    ) -> Self {
        Self {
            state: State::ClosingStreamReceiver,
            stream_receivers: stream_receiver,
            pending_frames,
            pending_flush_wakers: vec![],
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
        let mut this = self.get_mut();

        loop {
            match this.state {
                State::ClosingStreamReceiver => {
                    for stream in this.stream_receivers.iter_mut() {
                        stream.close();
                    }
                    this.state = State::DrainingStreamReceiver;
                }

                State::DrainingStreamReceiver => {
                    match ready!(this.stream_receivers.poll_next_unpin(cx)) {
                        Some(StreamCommand::SendFrame(frame)) => {
                            this.pending_frames.push_back(frame.into())
                        }
                        Some(StreamCommand::CloseStream { id, ack }) => {
                            this.pending_frames
                                .push_back(Frame::close_stream(id, ack).into());
                        }
                        None => {
                            // Receiver is closed, meaning we have queued all frames for sending.
                            // Notify all pending flush tasks.
                            for waker in mem::take(&mut this.pending_flush_wakers) {
                                waker.wake();
                            }

                            this.state = State::SendingTermFrame;
                        }
                    }
                }
                State::SendingTermFrame => {
                    this.pending_frames.push_back(Frame::term().into());
                    this.state = State::FlushingPendingFrames;
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
    SendingTermFrame,
    FlushingPendingFrames,
    ClosingSocket,
}
