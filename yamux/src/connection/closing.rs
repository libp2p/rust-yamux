use crate::connection::command_receivers::CommandReceivers;
use crate::connection::StreamCommand;
use crate::frame;
use crate::frame::Frame;
use crate::Result;
use futures::stream::Fuse;
use futures::{ready, AsyncRead, AsyncWrite, SinkExt};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A [`Future`] that gracefully closes the yamux connection.
#[must_use]
pub struct Closing<T> {
    state: State,
    stream_receivers: CommandReceivers,
    pending_frames: VecDeque<Frame<()>>,
    socket: Fuse<frame::Io<T>>,
}

impl<T> Closing<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(
        stream_receivers: CommandReceivers,
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
        let mut this = self.get_mut();

        loop {
            match this.state {
                State::ClosingStreamReceiver => {
                    this.stream_receivers.close();
                    this.state = State::DrainingStreamReceiver;
                }

                State::DrainingStreamReceiver => match this.stream_receivers.poll_next(cx) {
                    Poll::Ready(StreamCommand::SendFrame(frame)) => {
                        this.pending_frames.push_back(frame.into())
                    }
                    Poll::Ready(StreamCommand::CloseStream { id, ack }) => {
                        this.pending_frames
                            .push_back(Frame::close_stream(id, ack).into());
                    }
                    Poll::Pending => {
                        if this.stream_receivers.is_empty() {
                            this.state = State::SendingTermFrame;
                            continue;
                        }

                        return Poll::Pending;
                    }
                },
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
