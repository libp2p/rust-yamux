use crate::connection::Result;
use crate::connection::{ControlCommand, StreamCommand};
use crate::frame::Frame;
use crate::{frame, ConnectionError};
use futures::channel::mpsc;
use futures::stream::Fuse;
use futures::{ready, AsyncRead, AsyncWrite, SinkExt, StreamExt};
use std::collections::VecDeque;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};

/// A [`Future`] that gracefully closes the yamux connection.
#[must_use]
pub struct Closing<T> {
    state: State,
    control_receiver: mpsc::Receiver<ControlCommand>,
    stream_receiver: mpsc::Receiver<StreamCommand>,
    pending_frames: VecDeque<Frame<()>>,
    socket: Fuse<frame::Io<T>>,
}

impl<T> Closing<T>
where
    T: AsyncRead + AsyncWrite + Unpin,
{
    pub(crate) fn new(
        control_receiver: mpsc::Receiver<ControlCommand>,
        stream_receiver: mpsc::Receiver<StreamCommand>,
        pending_frames: VecDeque<Frame<()>>,
        socket: Fuse<frame::Io<T>>,
    ) -> Self {
        Self {
            state: State::ClosingControlReceiver,
            control_receiver,
            stream_receiver,
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
                State::ClosingControlReceiver => {
                    this.control_receiver.close();
                    this.state = State::DrainingControlReceiver;
                }
                State::DrainingControlReceiver => {
                    match ready!(this.control_receiver.poll_next_unpin(cx)) {
                        Some(ControlCommand::OpenStream(reply)) => {
                            let _ = reply.send(Err(ConnectionError::Closed));
                        }
                        Some(ControlCommand::CloseConnection(reply)) => {
                            let _ = reply.send(());
                        }
                        None => this.state = State::ClosingStreamReceiver,
                    }
                }
                State::ClosingStreamReceiver => {
                    this.stream_receiver.close();
                    this.state = State::DrainingStreamReceiver;
                }

                State::DrainingStreamReceiver => {
                    this.stream_receiver.close();

                    match ready!(this.stream_receiver.poll_next_unpin(cx)) {
                        Some(StreamCommand::SendFrame(frame)) => {
                            this.pending_frames.push_back(frame.into())
                        }
                        Some(StreamCommand::CloseStream { id, ack }) => this
                            .pending_frames
                            .push_back(Frame::close_stream(id, ack).into()),
                        None => this.state = State::SendingTermFrame,
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

pub enum State {
    ClosingControlReceiver,
    DrainingControlReceiver,
    ClosingStreamReceiver,
    DrainingStreamReceiver,
    SendingTermFrame,
    FlushingPendingFrames,
    ClosingSocket,
}
